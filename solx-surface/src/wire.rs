//! Shared HTTP request/response DTOs for the `solx-server`/`solx-client`
//! seam. Defined once here (rather than duplicated in each crate) so the
//! two sides can't drift on shape.
//!
//! There is very little left here, by design. The HTTP surface is RESTful:
//! an entity's `path`/`name` live in the URL (`GET /docs/research/ai/note`,
//! split back apart by [`crate::path::split_ref`]), `list`/`search` take
//! their [`crate::query::ListOptions`]/[`crate::query::SearchQuery`] as a
//! query string, `save` takes the bare input DTO as its body, and the
//! entity DTOs (`TypeEntity`, `Document`, `Action`, `ActionExecResult`, ...)
//! are already `Serialize + Deserialize` and are used directly as response
//! bodies. What remains is the handful of shapes with no natural home:
//! `POST /validate`'s body, and the two `/files` responses that wrap a
//! plain string or list of strings (raw file *content* is transferred as
//! unencoded bytes, so only the metadata replies are JSON).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Body for `POST /validate` (`TypeManager::validate`). A top-level route
/// rather than something under `/types/...`, since a static segment beside
/// the `/types/{*ref}` catch-all would permanently shadow any type stored
/// at that reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidateRequest {
    pub value: Value,
    pub type_ref: String,
}

/// Response body for `PUT /files/{*rel_path}` — the stored path, which may
/// differ from the requested one (see `FileStore::put`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilePutResponse {
    pub rel_path: String,
}

/// Response body for `GET /files?prefix=...`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileListResponse {
    pub paths: Vec<String>,
}
