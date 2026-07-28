//! Path & name normalization/validation — the core of the directory-style
//! namespace that replaces knowledge bases.
//!
//! - A `path` is a directory-like string: leading slash canonical, no trailing
//!   slash (except the root `"/"`). Example: `/research/ai`.
//! - A `name` is a single segment (no separators).
//! - A "full reference" is `"<path>/<name>"` (the root case collapses the
//!   double slash), e.g. `/research/ai/note` or `/note` at the root.

use crate::error::{Result, SolxError};

/// Characters forbidden anywhere in a path segment or a name.
const FORBIDDEN: &[char] = &['/', '\\', ':'];

fn validate_segment(seg: &str) -> Result<()> {
    if seg.is_empty() {
        return Err(SolxError::Invalid("empty path/name segment".into()));
    }
    if seg == "." || seg == ".." {
        return Err(SolxError::Invalid(format!("reserved segment '{seg}'")));
    }
    if let Some(bad) = seg.chars().find(|c| FORBIDDEN.contains(c) || c.is_control()) {
        return Err(SolxError::Invalid(format!(
            "segment '{seg}' contains forbidden character {bad:?}"
        )));
    }
    Ok(())
}

/// Validate a bare entity name (a single segment).
pub fn validate_name(name: &str) -> Result<()> {
    let name = name.trim();
    validate_segment(name)
}

/// Canonicalize a directory path. Accepts inputs with or without a leading
/// slash, collapses repeated slashes, and strips a trailing slash. The root is
/// `"/"`.
pub fn normalize_path(path: &str) -> Result<String> {
    let path = path.trim();
    if path.is_empty() || path == "/" {
        return Ok("/".to_string());
    }
    let mut out = String::new();
    for seg in path.split('/') {
        if seg.is_empty() {
            continue; // collapse leading/duplicate/trailing slashes
        }
        validate_segment(seg)?;
        out.push('/');
        out.push_str(seg);
    }
    if out.is_empty() {
        out.push('/');
    }
    Ok(out)
}

/// Build the canonical full reference `"<path>/<name>"` for an entity.
pub fn full_ref(path: &str, name: &str) -> Result<String> {
    let path = normalize_path(path)?;
    validate_name(name)?;
    let name = name.trim();
    if path == "/" {
        Ok(format!("/{name}"))
    } else {
        Ok(format!("{path}/{name}"))
    }
}

/// Split a full reference into `(path, name)`.
///
/// `/research/ai/note` -> (`/research/ai`, `note`); `/note` -> (`/`, `note`).
pub fn split_ref(full: &str) -> Result<(String, String)> {
    let normalized = normalize_path(full)?;
    if normalized == "/" {
        return Err(SolxError::Invalid("reference has no name segment".into()));
    }
    let idx = normalized
        .rfind('/')
        .expect("normalized path always contains '/'");
    let name = normalized[idx + 1..].to_string();
    let path = if idx == 0 {
        "/".to_string()
    } else {
        normalized[..idx].to_string()
    };
    validate_name(&name)?;
    Ok((path, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize_path("").unwrap(), "/");
        assert_eq!(normalize_path("/").unwrap(), "/");
        assert_eq!(normalize_path("research/ai").unwrap(), "/research/ai");
        assert_eq!(normalize_path("/research/ai/").unwrap(), "/research/ai");
        assert_eq!(normalize_path("//research//ai//").unwrap(), "/research/ai");
    }

    #[test]
    fn rejects_bad_segments() {
        assert!(normalize_path("/a:b").is_err());
        assert!(normalize_path("/a\\b").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(normalize_path("/a/../b").is_err());
    }

    #[test]
    fn full_ref_and_split_roundtrip() {
        assert_eq!(full_ref("/research/ai", "note").unwrap(), "/research/ai/note");
        assert_eq!(full_ref("/", "note").unwrap(), "/note");
        assert_eq!(
            split_ref("/research/ai/note").unwrap(),
            ("/research/ai".to_string(), "note".to_string())
        );
        assert_eq!(split_ref("/note").unwrap(), ("/".to_string(), "note".to_string()));
        assert!(split_ref("/").is_err());
    }
}
