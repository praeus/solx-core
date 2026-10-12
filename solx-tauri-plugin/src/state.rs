use std::sync::{Arc, RwLock};

use serde::Serialize;
use solx_config::ConfigService;
use solx_manager::App;
use solx_surface::error::{Result, SolxError};

/// Where the plugin's `App` comes from.
pub(crate) enum Backend {
    /// A fixed, host-built `App` (the server build's local managers). Never
    /// changes; `connect`/`disconnect` are refused.
    Fixed,
    /// A thin client: `App::remote` over a URL/token persisted in `config`,
    /// an app-private `ConfigService` (not the shared solx appdata one, so
    /// connecting here doesn't flip solx-cli/solx-mcp into remote mode).
    Client { config: Arc<ConfigService> },
}

/// An embedded `solx-server` the host started over the same `App`, reported
/// by `info` so the UI can show other clients how to reach it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedServer {
    pub url: String,
    pub token: String,
}

/// Managed Tauri state behind every command.
pub struct SolxState {
    app: RwLock<Option<Arc<App>>>,
    pub(crate) backend: Backend,
    embedded: RwLock<Option<EmbeddedServer>>,
}

impl SolxState {
    pub(crate) fn new(app: Option<Arc<App>>, backend: Backend) -> Self {
        SolxState { app: RwLock::new(app), backend, embedded: RwLock::new(None) }
    }

    /// The current `App`, or a `Config` error while a client is disconnected.
    pub fn app(&self) -> Result<Arc<App>> {
        self.app.read().unwrap().clone().ok_or_else(|| {
            SolxError::Config("not connected to a solx-server — set the server URL and token in Settings".into())
        })
    }

    pub(crate) fn current(&self) -> Option<Arc<App>> {
        self.app.read().unwrap().clone()
    }

    pub(crate) fn replace(&self, app: Option<Arc<App>>) {
        *self.app.write().unwrap() = app;
    }

    /// Record (or clear) the embedded server the host is running.
    pub fn set_embedded(&self, server: Option<EmbeddedServer>) {
        *self.embedded.write().unwrap() = server;
    }

    pub(crate) fn embedded(&self) -> Option<EmbeddedServer> {
        self.embedded.read().unwrap().clone()
    }
}
