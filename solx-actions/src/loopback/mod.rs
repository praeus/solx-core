//! `127.0.0.1`-only HTTP listeners solx-actions runs on its own account —
//! not part of the CLI/MCP/HTTP-route surface, just infrastructure this
//! crate stands up for itself.
//!
//! One per `oauth_start`/`oauth_stop` pair, RFC 6749/8252
//! authorization-code capture for a single browser sign-in. The equivalent
//! listener for action consoles (started once, lives for the process's
//! lifetime) now lives in `solx-console` — see `solx_console::loopback`.

pub mod oauth;
