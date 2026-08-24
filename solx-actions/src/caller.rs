//! `Caller` moved to `solx_surface::internal_actions` so it can be shared
//! with internal-action plugin crates (e.g. `solx-console`) without them
//! depending on `solx-actions`. Re-exported here so every existing
//! `crate::caller::Caller` reference in this crate keeps compiling.

pub use solx_surface::internal_actions::Caller;
