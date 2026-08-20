//! `127.0.0.1`-only HTTP listeners solx-actions runs on its own account —
//! not part of the CLI/MCP/HTTP-route surface, just infrastructure this
//! crate stands up for itself.
//!
//! Two, with different lifecycles:
//!
//! * [`oauth`] — one per `oauth_start`/`oauth_stop` pair, RFC 6749/8252
//!   authorization-code capture for a single browser sign-in.
//! * [`console`] — started once, lives for the process's lifetime; lets a
//!   spawned Command action's child process write to its own console (see
//!   `docs/console-implementation-plan.md` §8a).

pub mod console;
pub mod oauth;
