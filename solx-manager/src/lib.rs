//! Wires the local solx manager implementations together — the same
//! sequence `solx-cli`'s `App::build()` used to do inline. Extracted so
//! `solx-mcp` (and any other future long-lived consumer) gets identical,
//! correct wiring for free: seeding the built-in `get_env`/`set_env`
//! environment store before any `get_env` call, and the `set_self_ref`
//! dance `LocalActionManager` needs for recursive WASM/internal
//! action-exec calls, are both easy to silently get wrong in a second,
//! duplicated copy, and neither failure mode shows up until the
//! corresponding feature (env access, OAuth loopback, recursive
//! `action-exec`) is actually exercised.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};

use solx_actions::LocalActionManager;
use solx_config::ConfigService;
use solx_docs::LocalDocManager;
use solx_files::LocalFileStore;
use solx_surface::managers::{ActionManager, DocManager, FileStore, Solx, TypeManager};
use solx_types::LocalTypeManager;

/// The wired-together local backend. All fields are `Arc`, so cloning `App`
/// shares the same underlying managers.
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
    /// `SOLX_APPDATA_DIR` override, else the platform default).
    pub async fn build() -> Result<Arc<Self>> {
        let config = Arc::new(ConfigService::open().context("open config")?);
        Self::build_with_config(config).await
    }

    /// Build the app against an explicit appdata directory — for isolated
    /// testing (mirrors `ConfigService::open_in`).
    pub async fn build_in(appdata: impl Into<PathBuf>) -> Result<Arc<Self>> {
        let config = Arc::new(ConfigService::open_in(appdata).context("open config")?);
        Self::build_with_config(config).await
    }

    async fn build_with_config(config: Arc<ConfigService>) -> Result<Arc<Self>> {
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

        // Built as a concrete Arc first so `set_self_ref` can hand out a
        // `Weak<dyn ActionManager>` to itself (needed for WASM/internal
        // actions' recursive entity_exec/action-exec calls), then stored as
        // the trait object like the other managers.
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
        let actions: Arc<dyn ActionManager> = actions_concrete.clone();
        actions_concrete.set_self_ref(Arc::downgrade(&actions));

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
