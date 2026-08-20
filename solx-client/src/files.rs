use async_trait::async_trait;
use solx_surface::error::Result;
use solx_surface::managers::FileStore;
use solx_surface::wire::{FileListResponse, FilePutResponse};

use crate::http::{collection_url, delete, get_bytes, get_json_query, nested_url, put_bytes};

/// HTTP-proxy [`FileStore`] talking to a `solx-server`. File content is
/// transferred as raw bytes on the file's own URL, so nothing is base64'd
/// and a `GET` is a plain download with a guessed `Content-Type`.
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
        let url = nested_url(&self.base_url, "files", rel_path)?;
        let resp: FilePutResponse = put_bytes(&self.http, &self.token, url, bytes).await?;
        Ok(resp.rel_path)
    }

    async fn get(&self, rel_path: &str) -> Result<Vec<u8>> {
        let url = nested_url(&self.base_url, "files", rel_path)?;
        get_bytes(&self.http, &self.token, url).await
    }

    async fn delete(&self, rel_path: &str) -> Result<()> {
        let url = nested_url(&self.base_url, "files", rel_path)?;
        delete(&self.http, &self.token, url).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let url = collection_url(&self.base_url, "files")?;
        let resp: FileListResponse =
            get_json_query(&self.http, &self.token, url, &[("prefix", prefix)]).await?;
        Ok(resp.paths)
    }
}
