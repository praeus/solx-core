use async_trait::async_trait;
use serde_json::Value;
use solx_surface::entities::{Action, ActionExecResult, ActionInput};
use solx_surface::error::Result;
use solx_surface::managers::ActionManager;
use solx_surface::query::{ListOptions, Page};
use solx_surface::wire::{ExecRequest, PostRequest, RefRequest};

use crate::http::post_json;

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
    async fn post(&self, path: &str, name: &str, input: ActionInput) -> Result<Action> {
        let req = PostRequest { path: path.to_string(), name: name.to_string(), input };
        post_json(&self.http, &self.base_url, &self.token, "/actions/post", &req).await
    }

    async fn get(&self, path: &str, name: &str) -> Result<Action> {
        let req = RefRequest { path: path.to_string(), name: name.to_string() };
        post_json(&self.http, &self.base_url, &self.token, "/actions/get", &req).await
    }

    async fn delete(&self, path: &str, name: &str) -> Result<()> {
        let req = RefRequest { path: path.to_string(), name: name.to_string() };
        post_json(&self.http, &self.base_url, &self.token, "/actions/delete", &req).await
    }

    async fn list(&self, opts: ListOptions) -> Result<Page<Action>> {
        post_json(&self.http, &self.base_url, &self.token, "/actions/list", &opts).await
    }

    async fn exec(&self, path: &str, name: &str, params: Value) -> Result<ActionExecResult> {
        let req = ExecRequest { path: path.to_string(), name: name.to_string(), params };
        post_json(&self.http, &self.base_url, &self.token, "/actions/exec", &req).await
    }
}
