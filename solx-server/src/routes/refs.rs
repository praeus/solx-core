//! Turning a `{*ref}` catch-all URL segment back into an entity's
//! `(path, name)` pair.
//!
//! Every entity route is shaped `/{resource}/{*ref}`, where `ref` is the
//! full reference with its leading slash dropped by the router —
//! `GET /docs/research/ai/note` yields `research/ai/note`.
//! [`solx_surface::path::split_ref`] normalizes that (re-adding the leading
//! slash) and splits off the last segment as the name, so root-level
//! entities like `/docs/note` fall out as `("/", "note")` with no special
//! casing here.

use solx_surface::path::split_ref;

use crate::error::ApiError;

/// Split a catch-all URL capture into `(path, name)`.
///
/// axum percent-decodes the capture before we see it, so a name containing
/// `%`, `#`, `?` or a space arrives intact as long as the client encoded it.
/// The forbidden characters (`/`, `\`, `:`) are still rejected downstream by
/// `solx_surface::path`'s segment validation, which surfaces here as a 400.
pub fn split_url_ref(raw: &str) -> Result<(String, String), ApiError> {
    split_ref(raw).map_err(ApiError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solx_surface::error::SolxError;

    #[test]
    fn splits_nested_and_root_captures() {
        assert_eq!(
            split_url_ref("research/ai/note").unwrap(),
            ("/research/ai".to_string(), "note".to_string())
        );
        assert_eq!(split_url_ref("note").unwrap(), ("/".to_string(), "note".to_string()));
    }

    #[test]
    fn rejects_traversal_and_forbidden_characters() {
        // No conformant client can even send these — URL parsers strip dot
        // segments — but the server must not depend on that.
        assert!(matches!(split_url_ref("a/../b").unwrap_err().0, SolxError::Invalid(_)));
        assert!(matches!(split_url_ref("a/./b").unwrap_err().0, SolxError::Invalid(_)));
        assert!(matches!(split_url_ref("a:b").unwrap_err().0, SolxError::Invalid(_)));
        assert!(matches!(split_url_ref(r"a\b").unwrap_err().0, SolxError::Invalid(_)));
    }

    #[test]
    fn rejects_a_capture_with_no_name_segment() {
        assert!(split_url_ref("/").is_err());
    }
}
