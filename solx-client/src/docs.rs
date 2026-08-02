use async_trait::async_trait;
use solx_surface::entities::{Document, DocumentInput};
use solx_surface::error::Result;
use solx_surface::managers::DocManager;
use solx_surface::query::{ListOptions, Page, SearchQuery, SearchResults};
use solx_surface::wire::{PostRequest, RefRequest};

use crate::http::post_json;

/// HTTP-proxy [`DocManager`] talking to a `solx-server`.
pub struct RemoteDocManager {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl RemoteDocManager {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        RemoteDocManager {
            base_url: base_url.into(),
            token: token.into(),
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl DocManager for RemoteDocManager {
    async fn post(&self, path: &str, name: &str, input: DocumentInput) -> Result<Document> {
        let req = PostRequest { path: path.to_string(), name: name.to_string(), input };
        post_json(&self.http, &self.base_url, &self.token, "/docs/post", &req).await
    }

    async fn get(&self, path: &str, name: &str) -> Result<Document> {
        let req = RefRequest { path: path.to_string(), name: name.to_string() };
        post_json(&self.http, &self.base_url, &self.token, "/docs/get", &req).await
    }

    async fn delete(&self, path: &str, name: &str) -> Result<()> {
        let req = RefRequest { path: path.to_string(), name: name.to_string() };
        post_json(&self.http, &self.base_url, &self.token, "/docs/delete", &req).await
    }

    async fn list(&self, opts: ListOptions) -> Result<Page<Document>> {
        post_json(&self.http, &self.base_url, &self.token, "/docs/list", &opts).await
    }

    async fn search(&self, query: SearchQuery) -> Result<SearchResults> {
        post_json(&self.http, &self.base_url, &self.token, "/docs/search", &query).await
    }
}
