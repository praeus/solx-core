//! HTTP-proxy implementations of the `solx_surface::managers` traits,
//! talking to a `solx-server`. Deliberately thin: no `solx-manager`,
//! `solx-types`, `solx-docs`, `solx-actions`, `solx-files`, no
//! `wasmtime`/`libsql`/`tantivy` — the direct payoff of routing everything
//! through `solx-surface`'s trait seam.

mod actions;
mod docs;
mod error;
mod files;
mod http;
mod types;

pub use actions::RemoteActionManager;
pub use docs::RemoteDocManager;
pub use files::RemoteFileStore;
pub use types::RemoteTypeManager;
