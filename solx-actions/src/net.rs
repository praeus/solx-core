//! The outbound-host gate.
//!
//! One check, shared by every path that can make solx reach the network on a
//! caller's behalf: `Webhook`-type actions (`exec::run_webhook`) and the
//! `/builtin/web/*` built-ins (`internal::http`, `internal::http_stream`,
//! `internal::open-url`).
//!
//! It used to live inline in `run_webhook` and cover only webhook rows, which
//! left `http-request` as an unrestricted egress proxy reachable by any WASM
//! guest, MCP client or widget holding a bearer token — the deny-by-default
//! posture the README advertises with a hole straight through it. Extracting
//! it is what lets all four call it, and keeping it in one place is what stops
//! a second implementation drifting.
//!
//! **Deny-by-default**: an unset or empty allowlist rejects every URL.
//!
//! Each entry is split into its `scheme://host[:port]` origin and the path
//! after it. The origin must match the URL's exactly, compared parsed and
//! normalised (lowercased host, default port elided) — so `https://a.com`
//! can't admit `https://a.com.evil.net`, nor `http://h:80` admit
//! `http://h:8080`, as a raw string prefix would. An entry with no port allows
//! only the scheme's default; one ending in a bare `:` (`http://127.0.0.1:`)
//! allows any port. The path part is then a plain prefix of the URL's
//! normalised path and query, with `..` already resolved, so it can't be
//! climbed out of.
//!
//! The check covers every hop, not just the first: HTTP clients built for a
//! gated request take [`redirect_policy`], so an allowed host's redirect to an
//! unlisted one is refused rather than followed.

use solx_config::ConfigService;
use solx_surface::error::{Result, SolxError};
use url::{Position, Url};

/// Schemes an outbound URL may use. Anything else is refused outright rather
/// than prefix-matched — `file:` reads the local disk, `javascript:` and
/// `data:` execute in whatever opens them, and none of the three carries a
/// host for a prefix to meaningfully constrain.
const ALLOWED_SCHEMES: [&str; 2] = ["http", "https"];

/// The same ceiling reqwest's default policy uses.
const MAX_REDIRECTS: usize = 10;

/// Refuse `url` unless it starts with one of the configured base-URL
/// prefixes.
///
/// Callers must apply this *before* opening a connection or handing the URL
/// to anything else, so a denial has no side effect at all.
pub fn check_outbound_url(cfg: &ConfigService, url: &str) -> Result<()> {
    let parsed = Url::parse(url)
        .map_err(|e| SolxError::Exec(format!("outbound URL '{url}' is not a valid URL: {e}")))?;
    if !ALLOWED_SCHEMES.contains(&parsed.scheme()) {
        return Err(SolxError::Exec(format!(
            "outbound URL '{url}' must use http:// or https://"
        )));
    }
    if !is_allowed(&cfg.allowed_base_urls(), &parsed) {
        return Err(SolxError::Exec(format!(
            "outbound URL '{url}' does not match any prefix in solx-config.json's \
             'allowed_base_urls' allowlist; add a matching prefix there to \
             allow this request"
        )));
    }
    Ok(())
}

/// A redirect policy that holds every hop to the same allowlist as the
/// original request. Without it reqwest follows up to ten redirects anywhere,
/// so any allowed host with an open redirect would be an exit to every other.
///
/// The allowlist is snapshotted here, once per client, matching the
/// per-request reading [`check_outbound_url`] does.
pub fn redirect_policy(cfg: &ConfigService) -> reqwest::redirect::Policy {
    let allowlist = cfg.allowed_base_urls();
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error(format!("too many redirects (more than {MAX_REDIRECTS})"));
        }
        let url = attempt.url();
        if ALLOWED_SCHEMES.contains(&url.scheme()) && is_allowed(&allowlist, url) {
            attempt.follow()
        } else {
            let msg = format!(
                "redirect to '{url}' refused: it does not match any prefix in \
                 solx-config.json's 'allowed_base_urls' allowlist"
            );
            attempt.error(msg)
        }
    })
}

fn is_allowed(allowlist: &[String], url: &Url) -> bool {
    allowlist.iter().any(|prefix| prefix_admits(prefix, url))
}

/// Whether allowlist entry `prefix` admits `url`: same origin, and the
/// entry's path (if any) a prefix of the URL's. See the module docs.
fn prefix_admits(prefix: &str, url: &Url) -> bool {
    let Some(scheme_end) = prefix.find("://") else { return false };
    let authority_start = scheme_end + "://".len();
    let authority_end = prefix[authority_start..]
        .find(['/', '?', '#'])
        .map_or(prefix.len(), |i| authority_start + i);
    let (origin, path) = prefix.split_at(authority_end);
    let (origin, any_port) = match origin.strip_suffix(':') {
        Some(origin) => (origin, true),
        None => (origin, false),
    };
    let Ok(base) = Url::parse(origin) else { return false };
    base.scheme() == url.scheme()
        && base.host().is_some()
        && base.host() == url.host()
        && (any_port || base.port() == url.port())
        && url[Position::BeforePath..].starts_with(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(prefixes: &[&str]) -> (tempfile::TempDir, ConfigService) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ConfigService::open_in(dir.path()).unwrap();
        if !prefixes.is_empty() {
            cfg.set_allowed_base_urls(prefixes.iter().map(|s| s.to_string()).collect())
                .unwrap();
        }
        (dir, cfg)
    }

    #[test]
    fn unset_allowlist_denies_everything() {
        let (_d, cfg) = cfg_with(&[]);
        let err = check_outbound_url(&cfg, "https://example.com/x").unwrap_err();
        assert!(err.to_string().contains("allowed_base_urls"), "{err}");
    }

    #[test]
    fn empty_allowlist_denies_everything() {
        let (_d, cfg) = cfg_with(&[]);
        cfg.set_allowed_base_urls(vec![]).unwrap();
        assert!(check_outbound_url(&cfg, "https://example.com/x").is_err());
    }

    #[test]
    fn a_listed_prefix_passes_and_an_unlisted_one_does_not() {
        let (_d, cfg) = cfg_with(&["https://example.com/"]);
        assert!(check_outbound_url(&cfg, "https://example.com/a/b").is_ok());
        assert!(check_outbound_url(&cfg, "https://evil.example.net/").is_err());
    }

    /// The prefix is matched against the whole URL, so a host that merely
    /// *contains* an allowed one is not allowed.
    #[test]
    fn a_prefix_match_is_not_a_substring_match() {
        let (_d, cfg) = cfg_with(&["https://example.com/"]);
        assert!(check_outbound_url(&cfg, "https://evil.net/?u=https://example.com/").is_err());
    }

    #[test]
    fn non_http_schemes_are_refused_whatever_is_allowed() {
        let (_d, cfg) = cfg_with(&["about:", "file:///", "javascript:"]);
        for url in ["about:blank", "file:///etc/passwd", "javascript:alert(1)", "data:text/html,x"] {
            let err = check_outbound_url(&cfg, url).unwrap_err();
            assert!(err.to_string().contains("http:// or https://"), "{url}: {err}");
        }
    }

    /// A prefix that stops partway through a host or port admits nothing it
    /// merely spells the start of.
    #[test]
    fn a_prefix_cannot_end_inside_the_host_or_port() {
        let (_d, cfg) = cfg_with(&["https://api.example.com", "http://127.0.0.1:80"]);
        for url in [
            "https://api.example.com.evil.net/",
            "https://api.example.comx/",
            "http://127.0.0.1:8080/",
            // No port in the entry means the default port only.
            "https://api.example.com:8443/",
        ] {
            assert!(check_outbound_url(&cfg, url).is_err(), "{url} should be refused");
        }
        for url in [
            "https://api.example.com/v1",
            "https://api.example.com:443/v1",
            "http://127.0.0.1:80/x",
            "http://127.0.0.1/x",
        ] {
            assert!(check_outbound_url(&cfg, url).is_ok(), "{url} should be allowed");
        }
    }

    /// `http://host:` is the way to allow a host on any port.
    #[test]
    fn a_prefix_ending_on_the_port_colon_allows_any_port() {
        let (_d, cfg) = cfg_with(&["http://127.0.0.1:"]);
        assert!(check_outbound_url(&cfg, "http://127.0.0.1:11434/api").is_ok());
        assert!(check_outbound_url(&cfg, "http://127.0.0.1.evil.net:1/").is_err());
    }

    /// Userinfo can make a URL *start* with an allowed host while actually
    /// naming another.
    #[test]
    fn userinfo_cannot_smuggle_another_host() {
        let (_d, cfg) = cfg_with(&["https://example.com"]);
        assert!(check_outbound_url(&cfg, "https://example.com@evil.net/").is_err());
        assert!(check_outbound_url(&cfg, "https://example.com.@evil.net/").is_err());
    }

    /// The match is on the URL that will actually be sent, so `..` can't
    /// climb out of an allowed path, and host case doesn't matter.
    #[test]
    fn matching_uses_the_normalised_url() {
        let (_d, cfg) = cfg_with(&["https://example.com/api/"]);
        assert!(check_outbound_url(&cfg, "https://example.com/api/../admin").is_err());
        assert!(check_outbound_url(&cfg, "https://example.com/api/%2e%2e/admin").is_err());
        assert!(check_outbound_url(&cfg, "https://EXAMPLE.com/api/x").is_ok());
    }

    /// A redirect from an allowed host to an unlisted one is refused; one
    /// that stays inside the allowlist is followed.
    #[tokio::test]
    async fn redirects_are_held_to_the_allowlist() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Serves `302 -> <location>` on `/hop`, and `200 landed` otherwise.
        async fn serve(listener: tokio::net::TcpListener, location: String) {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let location = location.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let resp = if req.starts_with("GET /hop") {
                        format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    } else {
                        "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nlanded".to_string()
                    };
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        }

        let allowed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let allowed_base = format!("http://127.0.0.1:{}/", allowed.local_addr().unwrap().port());
        // `localhost` is a different host string from `127.0.0.1`, so it's
        // outside the allowlist even though it reaches the same machine.
        let outside = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let outside_url = format!("http://localhost:{}/", outside.local_addr().unwrap().port());
        tokio::spawn(serve(outside, String::new()));

        let (_d, cfg) = cfg_with(&[allowed_base.as_str()]);
        let client = reqwest::Client::builder()
            .redirect(redirect_policy(&cfg))
            .no_proxy()
            .build()
            .unwrap();

        // Allowed host redirecting off the allowlist: refused.
        let listener_task = tokio::spawn(serve(allowed, outside_url.clone()));
        let err = client.get(format!("{allowed_base}hop")).send().await.unwrap_err();
        assert!(err.is_redirect(), "{err}");
        listener_task.abort();

        // Same check, staying on the allowed host: followed.
        let allowed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}/", allowed.local_addr().unwrap().port());
        tokio::spawn(serve(allowed, format!("{base}landing")));
        let (_d2, cfg) = cfg_with(&[base.as_str()]);
        let client = reqwest::Client::builder()
            .redirect(redirect_policy(&cfg))
            .no_proxy()
            .build()
            .unwrap();
        let resp = client.get(format!("{base}hop")).send().await.unwrap();
        assert_eq!(resp.text().await.unwrap(), "landed");
    }

    /// A config written before the rename still gates, via `merged_base_urls`.
    #[test]
    fn the_legacy_config_key_still_gates() {
        let (_d, cfg) = cfg_with(&[]);
        cfg.set(
            "allowed_webhook_base_urls",
            serde_json::json!(["https://legacy.example.com/"]),
        )
        .unwrap();
        assert!(check_outbound_url(&cfg, "https://legacy.example.com/x").is_ok());
        assert!(check_outbound_url(&cfg, "https://other.example.com/x").is_err());
    }
}
