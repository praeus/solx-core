use async_trait::async_trait;
use serde_json::Value;
use solx_surface::entities::{TypeEntity, TypeInput};
use solx_surface::error::Result;
use solx_surface::managers::TypeManager;
use solx_surface::query::{ListOptions, Page};
use solx_surface::wire::{PostRequest, RefRequest, ResolveRequest, ValidateRequest};

use crate::http::post_json;

/// HTTP-proxy [`TypeManager`] talking to a `solx-server`.
pub struct RemoteTypeManager {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl RemoteTypeManager {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        RemoteTypeManager {
            base_url: base_url.into(),
            token: token.into(),
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl TypeManager for RemoteTypeManager {
    async fn post(&self, path: &str, name: &str, input: TypeInput) -> Result<TypeEntity> {
        let req = PostRequest { path: path.to_string(), name: name.to_string(), input };
        post_json(&self.http, &self.base_url, &self.token, "/types/post", &req).await
    }

    async fn get(&self, path: &str, name: &str) -> Result<TypeEntity> {
        let req = RefRequest { path: path.to_string(), name: name.to_string() };
        post_json(&self.http, &self.base_url, &self.token, "/types/get", &req).await
    }

    async fn delete(&self, path: &str, name: &str) -> Result<()> {
        let req = RefRequest { path: path.to_string(), name: name.to_string() };
        post_json(&self.http, &self.base_url, &self.token, "/types/delete", &req).await
    }

    async fn list(&self, opts: ListOptions) -> Result<Page<TypeEntity>> {
        post_json(&self.http, &self.base_url, &self.token, "/types/list", &opts).await
    }

    async fn resolve(&self, type_ref: &str) -> Result<TypeEntity> {
        let req = ResolveRequest { type_ref: type_ref.to_string() };
        post_json(&self.http, &self.base_url, &self.token, "/types/resolve", &req).await
    }

    async fn validate(&self, value: &Value, type_ref: &str) -> Result<()> {
        let req = ValidateRequest { value: value.clone(), type_ref: type_ref.to_string() };
        post_json(&self.http, &self.base_url, &self.token, "/types/validate", &req).await
    }
}
