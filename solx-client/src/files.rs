use async_trait::async_trait;
use base64::Engine as _;
use solx_surface::error::{Result, SolxError};
use solx_surface::managers::FileStore;
use solx_surface::wire::{FileDeleteRequest, FileGetRequest, FileGetResponse, FileListRequest, FileListResponse, FilePutRequest, FilePutResponse};

use crate::http::post_json;

/// HTTP-proxy [`FileStore`] talking to a `solx-server`. Bytes are
/// base64-encoded over the wire (JSON has no native binary type).
pub struct RemoteFileStore {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl RemoteFileStore {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        RemoteFileStore {
            base_url: base_url.into(),
            token: token.into(),
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl FileStore for RemoteFileStore {
    async fn put(&self, rel_path: &str, bytes: Vec<u8>) -> Result<String> {
        let req = FilePutRequest {
            rel_path: rel_path.to_string(),
            bytes_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
        };
        let resp: FilePutResponse =
            post_json(&self.http, &self.base_url, &self.token, "/files/put", &req).await?;
        Ok(resp.rel_path)
    }

    async fn get(&self, rel_path: &str) -> Result<Vec<u8>> {
        let req = FileGetRequest { rel_path: rel_path.to_string() };
        let resp: FileGetResponse =
            post_json(&self.http, &self.base_url, &self.token, "/files/get", &req).await?;
        base64::engine::general_purpose::STANDARD
            .decode(resp.bytes_b64)
            .map_err(|e| SolxError::Other(format!("malformed base64 in response: {e}")))
    }

    async fn delete(&self, rel_path: &str) -> Result<()> {
        let req = FileDeleteRequest { rel_path: rel_path.to_string() };
        post_json(&self.http, &self.base_url, &self.token, "/files/delete", &req).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let req = FileListRequest { prefix: prefix.to_string() };
        let resp: FileListResponse =
            post_json(&self.http, &self.base_url, &self.token, "/files/list", &req).await?;
        Ok(resp.paths)
    }
}
