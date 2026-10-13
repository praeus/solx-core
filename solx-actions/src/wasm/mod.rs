//! Wasmtime component-model host for *custom* (third-party) WASM action
//! components.
//!
//! Only one WIT world exists now (`solx-wasm/wit/custom-action.wit`):
//! `custom-action`, importing `action-exec` (recursively invoke any other
//! action by full reference — including every built-in, which are all
//! native `Internal` actions now, see `crate::internal`), `artifact-read`
//! (read-only, unrestricted file-store access), and `logger`.
//!
//! The WASM-hosted `backend-action` world (trusted, direct
//! `document-ops`/`entity-ops`/`system-ops`/`secrets` host access) and its
//! `solx-builtin-actions` component were retired: everything they provided
//! is now a native `Internal` action, reachable from custom WASM guests the
//! same way anyone else reaches it — via `action-exec`. Every WASM action
//! is therefore equally sandboxed by construction now; `Action.trusted` no
//! longer affects WASM execution (kept on the entity as an inert legacy
//! field to avoid a DB migration).
//!
//! Split into two units:
//!
//! * [`host`] — the wasmtime engine, component cache, WIT bindings, and
//!   [`host::HostState`] with its `Host` trait implementations. Wasmtime
//!   and bindgen! plumbing lives here and nowhere else.
//! * [`actions`] — [`actions::exec`], the public entry point that drives one
//!   guest invocation: builds a `HostState` from `host`, runs it under a
//!   timeout with panic-catching, and reports the result.
//!
//! ## Async execution — why nesting no longer costs threads
//!
//! Guests run on wasmtime *fibers*, not on blocking-pool threads. The
//! bindings are generated with `imports`/`exports: { default: async }`, so
//! the guest export is invoked through `TypedFunc::call_async`, which is
//! `store.on_fiber(..)`: the guest gets its own stack, and whenever a host
//! import's future returns `Pending` the fiber suspends and the OS thread
//! goes back to the executor.

mod host;
pub mod actions;

pub use actions::exec;
