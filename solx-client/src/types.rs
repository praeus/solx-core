use async_trait::async_trait;
use serde_json::Value;
use solx_surface::entities::{TypeEntity, TypeInput};
use solx_surface::error::Result;
use solx_surface::managers::TypeManager;
use solx_surface::path::split_ref;
use solx_surface::query::{ListOptions, Page, PathFacet};
use solx_surface::wire::ValidateRequest;

use crate::http::{collection_url, delete, entity_url, get_json, get_json_query, post_json, put_json};

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
    async fn save(&self, path: &str, name: &str, input: TypeInput) -> Result<TypeEntity> {
        let url = entity_url(&self.base_url, "types", path, name)?;
        put_json(&self.http, &self.token, url, &input).await
    }

    async fn get(&self, path: &str, name: &str) -> Result<TypeEntity> {
        let url = entity_url(&self.base_url, "types", path, name)?;
        get_json(&self.http, &self.token, url).await
    }

    async fn delete(&self, path: &str, name: &str) -> Result<()> {
        let url = entity_url(&self.base_url, "types", path, name)?;
        delete(&self.http, &self.token, url).await
    }

    async fn list(&self, opts: ListOptions) -> Result<Page<TypeEntity>> {
        let url = collection_url(&self.base_url, "types")?;
        get_json_query(&self.http, &self.token, url, &opts).await
    }

    async fn paths(&self, opts: ListOptions) -> Result<Page<PathFacet>> {
        let url = collection_url(&self.base_url, "types-paths")?;
        get_json_query(&self.http, &self.token, url, &opts).await
    }

    /// Resolved client-side: `resolve` is defined as `split_ref` + `get`
    /// (see `LocalTypeManager`), which `GET /types/{*ref}` already is — so
    /// there's no separate route for it.
    async fn resolve(&self, type_ref: &str) -> Result<TypeEntity> {
        let (path, name) = split_ref(type_ref)?;
        self.get(&path, &name).await
    }

    async fn validate(&self, value: &Value, type_ref: &str) -> Result<()> {
        let url = collection_url(&self.base_url, "validate")?;
        let req = ValidateRequest { value: value.clone(), type_ref: type_ref.to_string() };
        post_json(&self.http, &self.token, url, &req).await
    }
}
