//! Turn an HTTP response from `solx-server` back into the exact
//! `solx_surface::SolxError` variant it originated from.
//!
//! `SolxError` is adjacently tagged (`{"kind":"...","message":"..."}`, see
//! `solx-surface/src/error.rs`), so a non-2xx response's JSON body
//! deserializes straight back into the right variant — no separate
//! status-code-to-variant mapping table needed here.

use serde::de::DeserializeOwned;
use solx_surface::error::SolxError;

/// Read a `solx-server` response: on success, deserialize the body as `T`;
/// on failure, deserialize the body as a `SolxError` and return that `Err`
/// directly (falling back to `SolxError::Other` if the body itself isn't
/// well-formed, e.g. a proxy/gateway error page).
pub async fn read_response<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T, SolxError> {
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| SolxError::Io(format!("reading response body: {e}")))?;

    if status.is_success() {
        serde_json::from_slice::<T>(&bytes)
            .map_err(|e| SolxError::Other(format!("malformed response body: {e}")))
    } else {
        match serde_json::from_slice::<SolxError>(&bytes) {
            Ok(err) => Err(err),
            Err(_) => Err(SolxError::Other(format!(
                "solx-server returned {status} with an unrecognized body: {}",
                String::from_utf8_lossy(&bytes)
            ))),
        }
    }
}

/// Map a transport-level failure (couldn't even reach the server) to a
/// `SolxError`.
pub fn transport_err(e: reqwest::Error) -> SolxError {
    SolxError::Io(format!("solx-server request failed: {e}"))
}
