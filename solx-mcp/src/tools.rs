//! Action-reference <-> MCP tool-name encoding.
//!
//! Every action in the actions database becomes one MCP tool. Tool names
//! can't contain `/`, so a `(path, name)` pair has to be encoded into a
//! single string — and decoded back, without depending on a prior
//! `tools/list` call or any cached lookup table (`call_tool` decodes and
//! calls `actions.exec()` directly; `exec()` already does its own `get()`
//! internally, so a stale/deleted action just surfaces as an in-band
//! `NotFound`).

const PREFIX: &str = "act__";
const OPAQUE_MARKER: &str = "b32_";

/// Encode `(path, name)` into an MCP tool name. Readable for the common case
/// (no segment contains a literal `__`); falls back to an unambiguous opaque
/// form for the rare case where one does, rather than risking a collision.
pub fn encode_tool_name(path: &str, name: &str) -> String {
    let segments: Vec<&str> = if path == "/" {
        Vec::new()
    } else {
        path.trim_start_matches('/').split('/').collect()
    };

    let all_pretty = !name.contains("__") && segments.iter().all(|s| !s.contains("__"));
    if all_pretty {
        let mut parts: Vec<&str> = segments;
        parts.push(name);
        format!("{PREFIX}{}", parts.join("__"))
    } else {
        let full = solx_surface::path::full_ref(path, name).unwrap_or_else(|_| format!("{path}/{name}"));
        format!("{PREFIX}{OPAQUE_MARKER}{}", base32_encode(full.as_bytes()))
    }
}

/// Decode an MCP tool name back into `(path, name)`. Returns `None` for
/// anything that isn't a `PREFIX`-prefixed name this module could have
/// produced — the one case `call_tool` treats as a hard protocol error
/// rather than an in-band domain error, since no plausible action
/// reference could be identified at all.
pub fn decode_tool_name(tool_name: &str) -> Option<(String, String)> {
    let rest = tool_name.strip_prefix(PREFIX)?;
    if let Some(b32) = rest.strip_prefix(OPAQUE_MARKER) {
        let bytes = base32_decode(b32)?;
        let full = String::from_utf8(bytes).ok()?;
        return solx_surface::path::split_ref(&full).ok();
    }
    let mut parts: Vec<&str> = rest.split("__").collect();
    if parts.is_empty() {
        return None;
    }
    let name = parts.pop()?;
    if name.is_empty() {
        return None;
    }
    let path = if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    };
    Some((path, name.to_string()))
}

// ── base32 (RFC 4648, no padding) — only alnum output, safe for tool names ──

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() * 8).div_ceil(5));
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buf >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buf << (5 - bits)) & 0x1f) as usize] as char);
    }
    out.to_ascii_lowercase()
}

fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    for c in s.to_ascii_uppercase().bytes() {
        let v = ALPHABET.iter().position(|&a| a == c)? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_case_is_readable_and_reversible() {
        let name = encode_tool_name("/builtin", "entity_new_document");
        assert_eq!(name, "act__builtin__entity_new_document");
        assert_eq!(
            decode_tool_name(&name),
            Some(("/builtin".to_string(), "entity_new_document".to_string()))
        );
    }

    #[test]
    fn root_path_round_trips() {
        let name = encode_tool_name("/", "weather");
        assert_eq!(name, "act__weather");
        assert_eq!(decode_tool_name(&name), Some(("/".to_string(), "weather".to_string())));
    }

    #[test]
    fn nested_path_round_trips() {
        let name = encode_tool_name("/research/ai", "summarize-doc");
        assert_eq!(name, "act__research__ai__summarize-doc");
        assert_eq!(
            decode_tool_name(&name),
            Some(("/research/ai".to_string(), "summarize-doc".to_string()))
        );
    }

    #[test]
    fn double_underscore_in_segment_falls_back_to_opaque_and_still_round_trips() {
        let name = encode_tool_name("/a__b", "c");
        assert!(name.starts_with("act__b32_"));
        assert_eq!(decode_tool_name(&name), Some(("/a__b".to_string(), "c".to_string())));

        // The pathological pretty-encoding collision this fallback avoids:
        // (/a__b, c) and (/a, b__c) would otherwise both encode to
        // "act__a__b__c".
        let other = encode_tool_name("/a", "b__c");
        assert_ne!(name, other);
    }

    #[test]
    fn garbage_tool_name_does_not_decode() {
        assert_eq!(decode_tool_name("doc_post"), None);
        assert_eq!(decode_tool_name(""), None);
        assert_eq!(decode_tool_name("act__"), None);
    }
}
