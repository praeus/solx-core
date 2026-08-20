//! Shared request-sending helpers used by every `Remote*Manager`.
//!
//! The `solx-server` surface is RESTful, so an entity's `path`/`name` go in
//! the URL and `list`/`search` options go in the query string. [`entity_url`]
//! is what keeps that safe: names may legally contain `%`, `#`, `?` or
//! spaces (only `/`, `\` and `:` are forbidden — see
//! `solx_surface::path`), all of which must be percent-encoded to survive
//! the trip. `Url::path_segments_mut` does exactly that, per segment.

use reqwest::Method;
use serde::de::DeserializeOwned;
use serde::Serialize;
use solx_surface::error::SolxError;
use solx_surface::path::full_ref;
use url::Url;

use crate::error::{read_bytes_response, read_response, transport_err};

/// Build `{base_url}/{resource}/{path}/{name}`, percent-encoding each
/// segment. The root path (`"/"`) contributes no segments, so a root-level
/// entity lands at `/docs/note` — exactly what the server's `split_ref`
/// reads back as `("/", "note")`.
///
/// Goes through [`full_ref`] first, which applies the same validation the
/// server would. That matters beyond an early error message: an empty name
/// would otherwise produce a trailing slash that normalizes to a
/// *different, valid* entity server-side.
pub(crate) fn entity_url(
    base_url: &str,
    resource: &str,
    path: &str,
    name: &str,
) -> Result<Url, SolxError> {
    nested_url(base_url, resource, &full_ref(path, name)?)
}

/// Build `{base_url}/{resource}/{rel}`, percent-encoding each segment of
/// `rel` individually so separators survive but the segment contents are
/// escaped.
pub(crate) fn nested_url(base_url: &str, resource: &str, rel: &str) -> Result<Url, SolxError> {
    let mut url = collection_url(base_url, resource)?;
    url.path_segments_mut()
        .map_err(|_| SolxError::Invalid(format!("base URL cannot be a base: {base_url}")))?
        .extend(rel.split('/').filter(|s| !s.is_empty()));
    Ok(url)
}

/// Build `{base_url}/{resource}` for a collection-level route.
pub(crate) fn collection_url(base_url: &str, resource: &str) -> Result<Url, SolxError> {
    let joined = format!("{}/{}", base_url.trim_end_matches('/'), resource.trim_start_matches('/'));
    Url::parse(&joined).map_err(|e| SolxError::Invalid(format!("invalid server URL {joined}: {e}")))
}

/// Send a request with a bearer token and an optional JSON body, then
/// deserialize the response (success or error) via
/// [`crate::error::read_response`].
pub(crate) async fn send_json<Req, Resp>(
    http: &reqwest::Client,
    token: &str,
    method: Method,
    url: Url,
    body: Option<&Req>,
) -> Result<Resp, SolxError>
where
    Req: Serialize + ?Sized,
    Resp: DeserializeOwned,
{
    let mut req = http.request(method, url).bearer_auth(token);
    if let Some(body) = body {
        req = req.json(body);
    }
    let resp = req.send().await.map_err(transport_err)?;
    read_response(resp).await
}

/// `GET url` with no body. Query parameters, when needed, are applied by
/// the caller via [`Url::query_pairs_mut`] or `reqwest`'s `.query()`.
pub(crate) async fn get_json<Resp: DeserializeOwned>(
    http: &reqwest::Client,
    token: &str,
    url: Url,
) -> Result<Resp, SolxError> {
    send_json::<(), Resp>(http, token, Method::GET, url, None).await
}

/// `GET url?{query}`, where `query` serializes to a flat set of pairs.
pub(crate) async fn get_json_query<Q: Serialize, Resp: DeserializeOwned>(
    http: &reqwest::Client,
    token: &str,
    url: Url,
    query: &Q,
) -> Result<Resp, SolxError> {
    let resp = http
        .get(url)
        .bearer_auth(token)
        .query(query)
        .send()
        .await
        .map_err(transport_err)?;
    read_response(resp).await
}

/// `PUT url` with a JSON body.
pub(crate) async fn put_json<Req, Resp>(
    http: &reqwest::Client,
    token: &str,
    url: Url,
    body: &Req,
) -> Result<Resp, SolxError>
where
    Req: Serialize + ?Sized,
    Resp: DeserializeOwned,
{
    send_json(http, token, Method::PUT, url, Some(body)).await
}

/// `POST url` with a JSON body.
pub(crate) async fn post_json<Req, Resp>(
    http: &reqwest::Client,
    token: &str,
    url: Url,
    body: &Req,
) -> Result<Resp, SolxError>
where
    Req: Serialize + ?Sized,
    Resp: DeserializeOwned,
{
    send_json(http, token, Method::POST, url, Some(body)).await
}

/// `PUT url` with a raw byte body (file content — no base64, no JSON).
pub(crate) async fn put_bytes<Resp: DeserializeOwned>(
    http: &reqwest::Client,
    token: &str,
    url: Url,
    bytes: Vec<u8>,
) -> Result<Resp, SolxError> {
    let resp = http
        .put(url)
        .bearer_auth(token)
        .body(bytes)
        .send()
        .await
        .map_err(transport_err)?;
    read_response(resp).await
}

/// `GET url`, returning the raw response body.
pub(crate) async fn get_bytes(
    http: &reqwest::Client,
    token: &str,
    url: Url,
) -> Result<Vec<u8>, SolxError> {
    let resp = http.get(url).bearer_auth(token).send().await.map_err(transport_err)?;
    read_bytes_response(resp).await
}

/// `DELETE url`. The server answers `204 No Content`, which
/// [`read_response`] maps to `()`.
pub(crate) async fn delete(http: &reqwest::Client, token: &str, url: Url) -> Result<(), SolxError> {
    send_json::<(), ()>(http, token, Method::DELETE, url, None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_nested_and_root_entity_urls() {
        let u = entity_url("http://127.0.0.1:9000", "docs", "/research/ai", "note").unwrap();
        assert_eq!(u.as_str(), "http://127.0.0.1:9000/docs/research/ai/note");

        let root = entity_url("http://127.0.0.1:9000", "docs", "/", "note").unwrap();
        assert_eq!(root.as_str(), "http://127.0.0.1:9000/docs/note");
    }

    #[test]
    fn percent_encodes_awkward_names() {
        let u = entity_url("http://127.0.0.1:9000", "docs", "/a b", "100% #1?").unwrap();
        assert_eq!(u.as_str(), "http://127.0.0.1:9000/docs/a%20b/100%25%20%231%3F");
    }

    #[test]
    fn tolerates_trailing_slash_in_base_url() {
        let u = entity_url("http://127.0.0.1:9000/", "types", "types/core", "String").unwrap();
        assert_eq!(u.as_str(), "http://127.0.0.1:9000/types/types/core/String");
    }

    #[test]
    fn rejects_a_name_that_would_retarget_the_url() {
        // An empty name would otherwise build `/docs/research/ai/`, which the
        // server normalizes to the *different* entity `/research` + `ai`.
        assert!(entity_url("http://127.0.0.1:9000", "docs", "/research/ai", "").is_err());
        assert!(entity_url("http://127.0.0.1:9000", "docs", "/research", "a/b").is_err());
    }

    #[test]
    fn builds_nested_file_urls() {
        let u = nested_url("http://127.0.0.1:9000", "files", "media/my pic.png").unwrap();
        assert_eq!(u.as_str(), "http://127.0.0.1:9000/files/media/my%20pic.png");
    }
}
