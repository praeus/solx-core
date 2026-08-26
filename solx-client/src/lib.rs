//! HTTP-proxy implementations of the `solx_surface::managers` traits,
//! talking to a `solx-server`. Deliberately thin: no `solx-manager`,
//! `solx-types`, `solx-docs`, `solx-actions`, `solx-files`, no
//! `wasmtime`/`libsql` — the direct payoff of routing everything
//! through `solx-surface`'s trait seam.
//!
//! The routes these call are documented in `solx-core/docs/http-api.md`.
//! Two trait methods have no route of their own and are satisfied here:
//! `TypeManager::resolve` (a ref split plus a `get`) and, on the server
//! side, the `list`/`search` split — see `types.rs` and `docs.rs`.

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
