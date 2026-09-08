use async_trait::async_trait;
use serde_json::Value;
use solx_surface::entities::{Action, ActionExecResult, ActionInput};
use solx_surface::error::Result;
use solx_surface::managers::ActionManager;
use solx_surface::query::{ActionSearchQuery, ListOptions, Page, PathFacet};

use crate::http::{collection_url, delete, entity_url, get_json, get_json_query, post_json, put_json};

/// HTTP-proxy [`ActionManager`] talking to a `solx-server`. `exec()` runs
/// entirely server-side — including WASM execution and every `Internal`
/// built-in (entity CRUD, search, secrets, OAuth loopback) — since that's
/// where the one `LocalActionManager` this proxies to actually lives.
pub struct RemoteActionManager {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl RemoteActionManager {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        RemoteActionManager {
            base_url: base_url.into(),
            token: token.into(),
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl ActionManager for RemoteActionManager {
    async fn save(&self, path: &str, name: &str, input: ActionInput) -> Result<Action> {
        let url = entity_url(&self.base_url, "actions", path, name)?;
        put_json(&self.http, &self.token, url, &input).await
    }

    async fn get(&self, path: &str, name: &str) -> Result<Action> {
        let url = entity_url(&self.base_url, "actions", path, name)?;
        get_json(&self.http, &self.token, url).await
    }

    async fn delete(&self, path: &str, name: &str) -> Result<()> {
        let url = entity_url(&self.base_url, "actions", path, name)?;
        delete(&self.http, &self.token, url).await
    }

    async fn list(&self, opts: ListOptions) -> Result<Page<Action>> {
        let url = collection_url(&self.base_url, "actions")?;
        get_json_query(&self.http, &self.token, url, &opts).await
    }

    async fn paths(&self, opts: ListOptions) -> Result<Page<PathFacet>> {
        let url = collection_url(&self.base_url, "actions-paths")?;
        get_json_query(&self.http, &self.token, url, &opts).await
    }

    /// `GET /actions-search` — a top-level route rather than
    /// `/actions/search`, for the same reason document search lives at
    /// `/search` rather than `/docs/search`; see `solx_client::docs`.
    async fn search(&self, query: ActionSearchQuery) -> Result<Page<Action>> {
        let url = collection_url(&self.base_url, "actions-search")?;
        get_json_query(&self.http, &self.token, url, &query).await
    }

    /// `POST` on the action's own URL, with the params as the body.
    async fn exec(&self, path: &str, name: &str, params: Value) -> Result<ActionExecResult> {
        let url = entity_url(&self.base_url, "actions", path, name)?;
        post_json(&self.http, &self.token, url, &params).await
    }
}
