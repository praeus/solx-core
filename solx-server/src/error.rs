use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use solx_surface::error::SolxError;

/// Wraps a [`SolxError`] so route handlers can use `?` and get a proper
/// HTTP response — the JSON body is always the tagged `SolxError` itself
/// (`{"kind":"...","message":"..."}`), so `solx-client` can reconstruct the
/// exact variant regardless of which status code came back.
pub struct ApiError(pub SolxError);

impl From<SolxError> for ApiError {
    fn from(e: SolxError) -> Self {
        ApiError(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = status_for(&self.0);
        (status, Json(self.0)).into_response()
    }
}

fn status_for(e: &SolxError) -> StatusCode {
    match e {
        SolxError::NotFound(_) => StatusCode::NOT_FOUND,
        SolxError::Invalid(_) => StatusCode::BAD_REQUEST,
        SolxError::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
        SolxError::Conflict(_) => StatusCode::CONFLICT,
        SolxError::Io(_) | SolxError::Db(_) | SolxError::Exec(_) | SolxError::Config(_) | SolxError::Other(_) => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
