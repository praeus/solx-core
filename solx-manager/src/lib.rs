//! Wires the solx manager implementations together — the same sequence
//! `solx-cli`'s `App::build()` used to do inline. Extracted so `solx-mcp`
//! (and any other future long-lived consumer) gets identical, correct
//! wiring for free: seeding the built-in `get_env`/`set_env` environment
//! store before any `get_env` call, and the `set_self_ref` dance
//! `LocalActionManager` needs for recursive WASM/internal action-exec
//! calls, are both easy to silently get wrong in a second, duplicated
//! copy, and neither failure mode shows up until the corresponding feature
//! (env access, OAuth loopback, recursive `action-exec`) is actually
//! exercised.
//!
//! `App` transparently builds either **local** managers (`Local*` from
//! `solx-types`/`solx-docs`/`solx-actions`/`solx-files`, today's exact
//! behavior — the default for every existing user) or **remote** ones
//! (`Remote*` from `solx-client`, HTTP-proxying to a `solx-server`),
//! depending on whether `server_url` is set in config. `solx-cli`/
//! `solx-mcp` never need to know or care which mode they're in — they only
//! ever see `Arc<dyn TypeManager/DocManager/ActionManager/FileStore>` via
//! the `Solx` trait.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};

use solx_actions::LocalActionManager;
use solx_config::ConfigService;
use solx_docs::LocalDocManager;
use solx_files::LocalFileStore;
use solx_surface::managers::{ActionManager, DocManager, FileStore, Solx, TypeManager};
use solx_types::LocalTypeManager;

/// The wired-together backend (local or remote). All fields are `Arc`, so
/// cloning `App` shares the same underlying managers.
#[derive(Clone)]
pub struct App {
    pub config: Arc<ConfigService>,
    types: Arc<dyn TypeManager>,
    docs: Arc<dyn DocManager>,
    actions: Arc<dyn ActionManager>,
    files: Arc<dyn FileStore>,
}

impl App {
    /// Build the app against the default appdata directory (`solx-config`'s
    /// `SOLX_APPDATA_DIR` override, else the platform default). Builds
    /// local or remote managers depending on config's `server_url`.
    pub async fn build() -> Result<Arc<Self>> {
        let config = Arc::new(ConfigService::open().context("open config")?);
        Self::build_with_config(config).await
    }

    /// Build the app against an explicit appdata directory — for isolated
    /// testing (mirrors `ConfigService::open_in`). Still local-or-remote
    /// per config, like `build()`.
    pub async fn build_in(appdata: impl Into<PathBuf>) -> Result<Arc<Self>> {
        let config = Arc::new(ConfigService::open_in(appdata).context("open config")?);
        Self::build_with_config(config).await
    }

    /// Build the app against the default appdata directory, **always**
    /// local — never proxies, regardless of what `server_url` might say.
    /// This is what `solx-server` itself must use: it must never
    /// accidentally proxy to itself if its own config happens to contain a
    /// stray `server_url`.
    pub async fn build_local() -> Result<Arc<Self>> {
        let config = Arc::new(ConfigService::open().context("open config")?);
        Self::wire_local(config).await
    }

    /// Like [`Self::build_local`], against an explicit appdata directory.
    pub async fn build_local_in(appdata: impl Into<PathBuf>) -> Result<Arc<Self>> {
        let config = Arc::new(ConfigService::open_in(appdata).context("open config")?);
        Self::wire_local(config).await
    }

    async fn build_with_config(config: Arc<ConfigService>) -> Result<Arc<Self>> {
        let snap = config.snapshot();
        // SOLX_SERVER_URL must be able to *enable* remote mode on its own
        // (mirroring SOLX_APPDATA_DIR's independent-of-config precedence),
        // not merely override an already-configured `server_url` — a
        // config with no `server_url` at all is the common case for every
        // client pointed at a server via env var alone.
        let server_url = std::env::var("SOLX_SERVER_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| snap.server_url.clone().filter(|s| !s.trim().is_empty()));
        match server_url {
            Some(server_url) => {
                let token = std::env::var("SOLX_SERVER_TOKEN")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .or(snap.server_token)
                    .context(
                        "server_url is configured but no server_token is available \
                         (start solx-server against this appdata dir first, or set \
                         SOLX_SERVER_TOKEN)",
                    )?;
                Self::wire_remote(config, server_url, token)
            }
            None => Self::wire_local(config).await,
        }
    }

    /// Today's exact wiring, unchanged: open the three local databases and
    /// the local file store, wire `LocalActionManager`'s self-reference.
    async fn wire_local(config: Arc<ConfigService>) -> Result<Arc<Self>> {
        // Seed the built-in `get_env`/`set_env` environment store once at
        // startup (decoupled from any single exec() call — see
        // internal::init_env_mappings).
        solx_actions::internal::init_env_mappings(
            config.snapshot().env_mappings.unwrap_or_default(),
        );

        let types: Arc<dyn TypeManager> = Arc::new(
            LocalTypeManager::open(&config.types_db_path())
                .await
                .context("open types db")?,
        );
        let docs: Arc<dyn DocManager> = Arc::new(
            LocalDocManager::open(
                &config.docs_db_path(),
                &config.search_index_dir().join("docs"),
                types.clone(),
            )
            .await
            .context("open docs db")?,
        );
        let files: Arc<dyn FileStore> = Arc::new(LocalFileStore::from_config(&config));

        // Built as a concrete Arc first so `set_self_ref` can hand it a
        // `Weak<LocalActionManager>` to itself (needed for WASM guests'
        // recursive action-exec calls, which go through the concrete
        // `exec_as` so they can carry the calling action's identity), then
        // stored as the trait object like the other managers.
        let actions_concrete = Arc::new(
            LocalActionManager::open(
                &config.actions_db_path(),
                config.clone(),
                types.clone(),
                docs.clone(),
                files.clone(),
            )
            .await
            .context("open actions db")?,
        );
        actions_concrete.set_self_ref(Arc::downgrade(&actions_concrete));
        let actions: Arc<dyn ActionManager> = actions_concrete;

        Ok(Arc::new(App {
            config,
            types,
            docs,
            actions,
            files,
        }))
    }

    /// Proxy every manager over HTTP to a running `solx-server`. `config`
    /// itself stays local — packages/config are always local-file
    /// concerns, never proxied, in either mode. None of `wire_local`'s
    /// local-only setup (env mappings, opening local DBs, `set_self_ref`)
    /// applies here — there's no local `LocalActionManager`/
    /// `LocalDocManager` in this process; all of that lives in whichever
    /// process is running `solx-server`.
    fn wire_remote(config: Arc<ConfigService>, server_url: String, token: String) -> Result<Arc<Self>> {
        let types: Arc<dyn TypeManager> =
            Arc::new(solx_client::RemoteTypeManager::new(server_url.clone(), token.clone()));
        let docs: Arc<dyn DocManager> =
            Arc::new(solx_client::RemoteDocManager::new(server_url.clone(), token.clone()));
        let actions: Arc<dyn ActionManager> =
            Arc::new(solx_client::RemoteActionManager::new(server_url.clone(), token.clone()));
        let files: Arc<dyn FileStore> = Arc::new(solx_client::RemoteFileStore::new(server_url, token));

        Ok(Arc::new(App {
            config,
            types,
            docs,
            actions,
            files,
        }))
    }
}

impl Solx for App {
    fn types(&self) -> Arc<dyn TypeManager> {
        self.types.clone()
    }
    fn files(&self) -> Arc<dyn FileStore> {
        self.files.clone()
    }
    fn docs(&self) -> Arc<dyn DocManager> {
        self.docs.clone()
    }
    fn actions(&self) -> Arc<dyn ActionManager> {
        self.actions.clone()
    }
}
