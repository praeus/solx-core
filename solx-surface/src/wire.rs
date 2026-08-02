//! Shared HTTP request/response DTOs for the `solx-server`/`solx-client`
//! seam. Defined once here (rather than duplicated in each crate) so the
//! two sides can't drift on shape. `path`/`name` are carried in the JSON
//! body rather than the URL path — a `path` value like `/research/ai` is
//! awkward to embed cleanly as a URL path segment, and body-based POST
//! avoids URL-encoding entirely.
//!
//! `ListOptions` and `SearchQuery` (see [`crate::query`]) are used directly
//! as request bodies for `list`/`search` — already flat, serde-friendly
//! structs, no wrapper needed. The entity DTOs (`TypeEntity`, `Document`,
//! `Action`, `ActionExecResult`, ...) are already `Serialize + Deserialize`
//! and used directly as response bodies.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Body for `get`/`delete` on types/docs/actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefRequest {
    pub path: String,
    pub name: String,
}

/// Body for `post` (create-or-replace) on types/docs/actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostRequest<T> {
    pub path: String,
    pub name: String,
    pub input: T,
}

/// Body for `TypeManager::resolve`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveRequest {
    pub type_ref: String,
}

/// Body for `TypeManager::validate`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidateRequest {
    pub value: Value,
    pub type_ref: String,
}

/// Body for `ActionManager::exec`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecRequest {
    pub path: String,
    pub name: String,
    pub params: Value,
}

/// Body for `FileStore::put`. Bytes are base64-encoded — JSON has no
/// native binary type, and this matches how `solx-cli` already encodes
/// non-UTF8 file content (`main.rs`'s file get/post handling).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePutRequest {
    pub rel_path: String,
    pub bytes_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePutResponse {
    pub rel_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileGetRequest {
    pub rel_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileGetResponse {
    pub bytes_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDeleteRequest {
    pub rel_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileListRequest {
    pub prefix: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileListResponse {
    pub paths: Vec<String>,
}
