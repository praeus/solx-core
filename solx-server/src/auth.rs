use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use solx_surface::error::SolxError;

use crate::error::ApiError;
use crate::state::AppState;

/// Bearer-token middleware: checks `Authorization: Bearer <token>` against
/// `state.token`. This shared secret is the entire auth model, which matches
/// the trust model everywhere else in solx today (there is no
/// multi-user/permissions system anywhere else either). The server binds
/// `127.0.0.1` by default. When `server_host` puts it on the network, this
/// check is the only gate, which is why the comparison is constant-time.
///
/// A rejection is a 401 whose body is the usual `SolxError` JSON
/// (`{"kind":"unauthorized",...}`), so clients can tell "no token" from
/// "wrong token" instead of seeing an empty body.
pub async fn require_bearer(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let provided = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match provided {
        Some(token) if constant_time_eq(token.as_bytes(), state.token.as_bytes()) => {
            Ok(next.run(request).await)
        }
        Some(_) => Err(SolxError::Unauthorized(
            "the bearer token does not match this solx-server's server_token".into(),
        )
        .into()),
        None => Err(SolxError::Unauthorized(
            "missing `Authorization: Bearer <token>` header".into(),
        )
        .into()),
    }
}

/// Byte comparison whose running time doesn't depend on where the first
/// mismatch is, so a network caller can't recover the token one byte at a
/// time. A length mismatch returns early, which reveals only the length.
/// The token always has a fixed length, so that leaks nothing useful.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn constant_time_eq_matches_only_identical_bytes() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}
