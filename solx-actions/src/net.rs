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

use solx_config::ConfigService;
use solx_surface::error::{Result, SolxError};

/// Schemes an outbound URL may use. Anything else is refused outright rather
/// than prefix-matched — `file:` reads the local disk, `javascript:` and
/// `data:` execute in whatever opens them, and none of the three carries a
/// host for a prefix to meaningfully constrain.
const ALLOWED_SCHEMES: [&str; 2] = ["http://", "https://"];

/// Refuse `url` unless it starts with one of the configured base-URL
/// prefixes.
///
/// Callers must apply this *before* opening a connection or handing the URL
/// to anything else, so a denial has no side effect at all.
pub fn check_outbound_url(cfg: &ConfigService, url: &str) -> Result<()> {
    if !ALLOWED_SCHEMES.iter().any(|s| url.starts_with(s)) {
        return Err(SolxError::Exec(format!(
            "outbound URL '{url}' must use http:// or https://"
        )));
    }
    let allowlist = cfg.allowed_base_urls();
    if !allowlist.iter().any(|base| url.starts_with(base.as_str())) {
        return Err(SolxError::Exec(format!(
            "outbound URL '{url}' does not match any prefix in solx-config.json's \
             'allowed_base_urls' allowlist; add a matching prefix there to \
             allow this request"
        )));
    }
    Ok(())
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
