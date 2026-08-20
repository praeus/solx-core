use async_trait::async_trait;
use solx_surface::entities::{Document, DocumentInput};
use solx_surface::error::Result;
use solx_surface::managers::DocManager;
use solx_surface::query::{ListOptions, Page, SearchQuery, SearchResults};

use crate::http::{collection_url, delete, entity_url, get_json, get_json_query, put_json};

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
    async fn save(&self, path: &str, name: &str, input: DocumentInput) -> Result<Document> {
        let url = entity_url(&self.base_url, "docs", path, name)?;
        put_json(&self.http, &self.token, url, &input).await
    }

    async fn get(&self, path: &str, name: &str) -> Result<Document> {
        let url = entity_url(&self.base_url, "docs", path, name)?;
        get_json(&self.http, &self.token, url).await
    }

    async fn delete(&self, path: &str, name: &str) -> Result<()> {
        let url = entity_url(&self.base_url, "docs", path, name)?;
        delete(&self.http, &self.token, url).await
    }

    async fn list(&self, opts: ListOptions) -> Result<Page<Document>> {
        let url = collection_url(&self.base_url, "docs")?;
        get_json_query(&self.http, &self.token, url, &opts).await
    }

    /// `GET /search` — a top-level route rather than `/docs/search`, which
    /// as a static sibling of the `/docs/{*ref}` catch-all would shadow any
    /// document named `search` at the root.
    async fn search(&self, query: SearchQuery) -> Result<SearchResults> {
        let url = collection_url(&self.base_url, "search")?;
        get_json_query(&self.http, &self.token, url, &query).await
    }
}
