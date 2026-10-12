//! One command per manager-trait method. Each mirrors a `solx-server` route
//! (noted on the command) and behaves identically, since both just call the
//! same `Arc<dyn _>` manager: local in the server build, a `solx-client`
//! `Remote*` proxy in the client build. Errors are the tagged `SolxError`
//! (`{"kind":"...","message":"..."}`) — the same body an HTTP error carries,
//! so a JS client maps both the same way.

use std::sync::Arc;

use serde::Serialize;
use solx_config::ConfigService;
use serde_json::{json, Value};
use solx_surface::entities::{
    Action, ActionExecResult, ActionInput, Document, DocumentInput, TypeEntity, TypeInput,
};
use solx_surface::error::{Result, SolxError};
use solx_surface::managers::Solx;
use solx_surface::path::split_ref;
use solx_surface::query::{ActionSearchQuery, ListOptions, Page, PathFacet, SearchQuery};
use tauri::ipc::{InvokeBody, Request, Response};
use tauri::{command, State};

use crate::state::{Backend, EmbeddedServer, SolxState};

/// Header carrying `files_put`'s relative path (percent-encoded, since
/// header values must be ASCII), as the body is the raw file bytes.
const FILE_PATH_HEADER: &str = "x-solx-path";

/// Normalize `(path, name)` exactly as `solx-server` does for a URL ref
/// (`routes/refs.rs`): join, then `split_ref`. Guarding the name first
/// matches `@solx/http`'s `entityPath`, where an empty name or one with a
/// `/` would otherwise silently address a different entity.
fn entity(path: &str, name: &str) -> Result<(String, String)> {
    if name.is_empty() || name.contains('/') {
        return Err(SolxError::Invalid(format!("invalid entity name: '{name}'")));
    }
    split_ref(&format!("{}/{}", path.trim_end_matches('/'), name))
}

// ── types ── (`/types`, `/types/{*ref}`, `/types-paths`, `/validate`)

/// `PUT /types/{*ref}`
#[command]
pub(crate) async fn types_save(
    state: State<'_, SolxState>,
    path: String,
    name: String,
    input: TypeInput,
) -> Result<TypeEntity> {
    let (path, name) = entity(&path, &name)?;
    state.app()?.types().save(&path, &name, input).await
}

/// `GET /types/{*ref}`
#[command]
pub(crate) async fn types_get(state: State<'_, SolxState>, path: String, name: String) -> Result<TypeEntity> {
    let (path, name) = entity(&path, &name)?;
    state.app()?.types().get(&path, &name).await
}

/// `DELETE /types/{*ref}`
#[command]
pub(crate) async fn types_delete(state: State<'_, SolxState>, path: String, name: String) -> Result<()> {
    let (path, name) = entity(&path, &name)?;
    state.app()?.types().delete(&path, &name).await
}

/// `GET /types`
#[command]
pub(crate) async fn types_list(
    state: State<'_, SolxState>,
    opts: Option<ListOptions>,
) -> Result<Page<TypeEntity>> {
    state.app()?.types().list(opts.unwrap_or_default()).await
}

/// `GET /types-paths`
#[command]
pub(crate) async fn types_paths(
    state: State<'_, SolxState>,
    opts: Option<ListOptions>,
) -> Result<Page<PathFacet>> {
    state.app()?.types().paths(opts.unwrap_or_default()).await
}

/// No route of its own — `@solx/http` resolves via `GET /types/{*ref}`.
#[command]
pub(crate) async fn types_resolve(state: State<'_, SolxState>, type_ref: String) -> Result<TypeEntity> {
    state.app()?.types().resolve(&type_ref).await
}

/// `POST /validate`
#[command]
pub(crate) async fn types_validate(
    state: State<'_, SolxState>,
    value: Value,
    type_ref: String,
) -> Result<()> {
    state.app()?.types().validate(&value, &type_ref).await
}

// ── docs ── (`/docs`, `/docs/{*ref}`, `/docs-paths`, `/search`)

/// `PUT /docs/{*ref}`
#[command]
pub(crate) async fn docs_save(
    state: State<'_, SolxState>,
    path: String,
    name: String,
    input: DocumentInput,
) -> Result<Document> {
    let (path, name) = entity(&path, &name)?;
    state.app()?.docs().save(&path, &name, input).await
}

/// `GET /docs/{*ref}`
#[command]
pub(crate) async fn docs_get(state: State<'_, SolxState>, path: String, name: String) -> Result<Document> {
    let (path, name) = entity(&path, &name)?;
    state.app()?.docs().get(&path, &name).await
}

/// `DELETE /docs/{*ref}`
#[command]
pub(crate) async fn docs_delete(state: State<'_, SolxState>, path: String, name: String) -> Result<()> {
    let (path, name) = entity(&path, &name)?;
    state.app()?.docs().delete(&path, &name).await
}

/// `GET /docs`
#[command]
pub(crate) async fn docs_list(state: State<'_, SolxState>, opts: Option<ListOptions>) -> Result<Page<Document>> {
    state.app()?.docs().list(opts.unwrap_or_default()).await
}

/// `GET /docs-paths`
#[command]
pub(crate) async fn docs_paths(
    state: State<'_, SolxState>,
    opts: Option<ListOptions>,
) -> Result<Page<PathFacet>> {
    state.app()?.docs().paths(opts.unwrap_or_default()).await
}

/// `GET /search`
#[command]
pub(crate) async fn docs_search(
    state: State<'_, SolxState>,
    query: Option<SearchQuery>,
) -> Result<Page<Document>> {
    state.app()?.docs().search(query.unwrap_or_default()).await
}

// ── actions ── (`/actions`, `/actions/{*ref}`, `/actions-paths`, `/actions-search`)

/// `PUT /actions/{*ref}`
#[command]
pub(crate) async fn actions_save(
    state: State<'_, SolxState>,
    path: String,
    name: String,
    input: ActionInput,
) -> Result<Action> {
    let (path, name) = entity(&path, &name)?;
    state.app()?.actions().save(&path, &name, input).await
}

/// `GET /actions/{*ref}`
#[command]
pub(crate) async fn actions_get(state: State<'_, SolxState>, path: String, name: String) -> Result<Action> {
    let (path, name) = entity(&path, &name)?;
    state.app()?.actions().get(&path, &name).await
}

/// `DELETE /actions/{*ref}`
#[command]
pub(crate) async fn actions_delete(state: State<'_, SolxState>, path: String, name: String) -> Result<()> {
    let (path, name) = entity(&path, &name)?;
    state.app()?.actions().delete(&path, &name).await
}

/// `GET /actions`
#[command]
pub(crate) async fn actions_list(state: State<'_, SolxState>, opts: Option<ListOptions>) -> Result<Page<Action>> {
    state.app()?.actions().list(opts.unwrap_or_default()).await
}

/// `GET /actions-paths`
#[command]
pub(crate) async fn actions_paths(
    state: State<'_, SolxState>,
    opts: Option<ListOptions>,
) -> Result<Page<PathFacet>> {
    state.app()?.actions().paths(opts.unwrap_or_default()).await
}

/// `GET /actions-search`
#[command]
pub(crate) async fn actions_search(
    state: State<'_, SolxState>,
    query: Option<ActionSearchQuery>,
) -> Result<Page<Action>> {
    state.app()?.actions().search(query.unwrap_or_default()).await
}

/// `POST /actions/{*ref}` — `params` defaults to `{}`, like an empty POST body.
#[command]
pub(crate) async fn actions_exec(
    state: State<'_, SolxState>,
    path: String,
    name: String,
    params: Option<Value>,
) -> Result<ActionExecResult> {
    let (path, name) = entity(&path, &name)?;
    let params = params.unwrap_or_else(|| json!({}));
    state.app()?.actions().exec(&path, &name, params).await
}

// ── files ── (`/files`, `/files/{*rel_path}`) — content is raw bytes both ways

/// `PUT /files/{*rel_path}`. The body is the raw bytes (a `Uint8Array`/
/// `ArrayBuffer` passed as `invoke`'s args); the path rides in the
/// `x-solx-path` header. Returns the stored (normalized) relative path.
#[command]
pub(crate) async fn files_put(state: State<'_, SolxState>, request: Request<'_>) -> Result<String> {
    let rel_path = request
        .headers()
        .get(FILE_PATH_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| SolxError::Invalid(format!("files_put: missing {FILE_PATH_HEADER} header")))?;
    let rel_path = percent_encoding::percent_decode_str(rel_path)
        .decode_utf8()
        .map_err(|e| SolxError::Invalid(format!("files_put: {FILE_PATH_HEADER} is not UTF-8: {e}")))?
        .into_owned();
    let bytes = match request.body() {
        InvokeBody::Raw(bytes) => bytes.clone(),
        InvokeBody::Json(_) => {
            return Err(SolxError::Invalid("files_put: expected a raw byte body".into()));
        }
    };
    state.app()?.files().put(&rel_path, bytes).await
}

/// `GET /files/{*rel_path}` — answered as a raw `ArrayBuffer`, not JSON.
#[command]
pub(crate) async fn files_get(state: State<'_, SolxState>, rel_path: String) -> Result<Response> {
    let bytes = state.app()?.files().get(&rel_path).await?;
    Ok(Response::new(bytes))
}

/// `DELETE /files/{*rel_path}`
#[command]
pub(crate) async fn files_delete(state: State<'_, SolxState>, rel_path: String) -> Result<()> {
    state.app()?.files().delete(&rel_path).await
}

/// `GET /files?prefix=` — the bare path list (HTTP wraps it as `{paths}`).
#[command]
pub(crate) async fn files_list(state: State<'_, SolxState>, prefix: Option<String>) -> Result<Vec<String>> {
    state.app()?.files().list(prefix.as_deref().unwrap_or("")).await
}

// ── host ── (no HTTP equivalent, beyond `/health`)

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HostInfo {
    /// `local` (managers in-process), `remote` (proxying to `serverUrl`) or
    /// `disconnected` (a client with no server configured yet).
    mode: &'static str,
    /// `true` for a client build, which can `connect`/`disconnect`.
    client: bool,
    version: &'static str,
    server_url: Option<String>,
    embedded_server: Option<EmbeddedServer>,
}

/// Stands in for `GET /health`: which mode the host is in, and where.
#[command]
pub(crate) fn info(state: State<'_, SolxState>) -> HostInfo {
    let app = state.current();
    let mode = match &app {
        None => "disconnected",
        Some(app) if app.remote_url().is_some() => "remote",
        Some(_) => "local",
    };
    HostInfo {
        mode,
        client: matches!(state.backend, Backend::Client { .. }),
        version: env!("CARGO_PKG_VERSION"),
        server_url: app.as_ref().and_then(|a| a.remote_url().map(str::to_string)),
        embedded_server: state.embedded(),
    }
}

/// Client builds only: point at a `solx-server`. The connection is checked
/// (one cheap authenticated call) before it's persisted, so a wrong URL or
/// token is reported here rather than on the next unrelated request.
#[command]
pub(crate) async fn connect(state: State<'_, SolxState>, server_url: String, token: String) -> Result<()> {
    let Backend::Client { config } = &state.backend else {
        return Err(SolxError::Invalid("this build runs solx locally; there is nothing to connect to".into()));
    };
    let server_url = server_url.trim().trim_end_matches('/').to_string();
    let token = token.trim().to_string();
    if server_url.is_empty() || token.is_empty() {
        return Err(SolxError::Invalid("both a server URL and a token are required".into()));
    }
    let app = solx_manager::App::remote(config.clone(), server_url.clone(), token.clone())
        .map_err(|e| SolxError::Config(format!("{e:#}")))?;
    app.docs().list(ListOptions { limit: Some(1), ..Default::default() }).await?;

    config.patch(json!({ "server_url": server_url, "server_token": token }))?;
    state.replace(Some(app));
    Ok(())
}

/// Client builds only: forget the server URL/token.
#[command]
pub(crate) fn disconnect(state: State<'_, SolxState>) -> Result<()> {
    let Backend::Client { config } = &state.backend else {
        return Err(SolxError::Invalid("this build runs solx locally; there is nothing to disconnect".into()));
    };
    config.mutate(|obj| {
        obj.remove("server_url");
        obj.remove("server_token");
        Ok(())
    })?;
    state.replace(None);
    Ok(())
}

/// Build the client-mode `App` from a previously persisted URL/token, if any.
pub(crate) fn restore_client(config: &Arc<ConfigService>) -> Option<Arc<solx_manager::App>> {
    let snap = config.snapshot();
    let url = snap.server_url.filter(|s| !s.trim().is_empty())?;
    let token = snap.server_token.filter(|s| !s.trim().is_empty())?;
    solx_manager::App::remote(config.clone(), url, token).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_normalizes_like_a_url_ref() {
        assert_eq!(entity("/", "note").unwrap(), ("/".into(), "note".into()));
        assert_eq!(entity("/research/ai/", "note").unwrap(), ("/research/ai".into(), "note".into()));
        assert_eq!(entity("research/ai", "note").unwrap(), ("/research/ai".into(), "note".into()));
        assert!(entity("/a", "").is_err());
        assert!(entity("/a", "b/c").is_err());
        assert!(entity("/a/../b", "c").is_err());
    }
}
