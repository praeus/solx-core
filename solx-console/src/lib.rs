//! `solx-console` — action consoles, detached-invocation state, and the
//! console loopback listener, split out of `solx-actions` into their own
//! crate and database so a plugin's storage isn't tied to the actions
//! catalogue's own physical file.
//!
//! This crate is also the reference consumer of
//! `solx_surface::internal_actions`' plugin API: [`actions::plugin`] builds
//! the `/builtin/console/*` and `/builtin/action/{start,stop,poll,cancelled}`
//! handlers and seed entries that `solx-actions` merges into its own
//! dispatcher at `LocalActionManager::open()` time, rather than hard-coding
//! them there.

pub mod actions;
pub mod console;
mod db;
pub mod invocations;
pub mod loopback;

use std::sync::Arc;

use solx_config::ConfigService;
use solx_surface::error::Result;

pub use console::ConsoleStore;
pub use invocations::InvocationStore;

/// Open this crate's own database file (`ConfigService::console_db_path`)
/// and construct both stores against it. Encapsulates the connection type
/// (`db::Db`, private — every entity crate has its own copy by design) so
/// `solx-actions` never needs to name it.
pub async fn open(config: Arc<ConfigService>) -> Result<(Arc<ConsoleStore>, Arc<InvocationStore>)> {
    let db = db::Db::open(&config.console_db_path()).await?;
    let console = Arc::new(ConsoleStore::new(db.clone(), config.clone()));
    console.ensure_schema().await?;
    let invocations = Arc::new(InvocationStore::new(db, config));
    invocations.ensure_schema().await?;
    Ok((console, invocations))
}
