//! Tauri v2 plugin exposing the solx managers to a webview over IPC — the
//! in-process counterpart of `solx-server`'s HTTP routes, consumed by
//! `@solx/tauri` in solx-js.
//!
//! Commands mirror the manager traits one-to-one (`types_get`,
//! `docs_search`, `actions_exec`, `files_put`, ...; see `commands`), invoked
//! from JS as `plugin:solx|<command>`. They run against whatever `App` the
//! host hands in, so the same command set serves both desktop builds:
//!
//! - [`local`] — the server build: a host-built local `App` (shared with an
//!   embedded `solx-server`, if the host runs one; see
//!   [`SolxState::set_embedded`]).
//! - [`client`] — the thin client build: `App::remote` proxies to a
//!   `solx-server` whose URL/token the UI sets via `connect`, persisted in
//!   an app-private `ConfigService`.
//!
//! Hosts grant access with the `solx:default` capability permission.

mod commands;
mod state;

use std::sync::Arc;

use solx_config::ConfigService;
use solx_manager::App;
use tauri::plugin::{Builder, TauriPlugin};
use tauri::{Manager, Runtime};

pub use state::{EmbeddedServer, SolxState};

use state::Backend;

#[cfg(test)]
include!("command_names.rs");

/// Plugin over a fixed, host-built `App` — normally local managers.
pub fn local<R: Runtime>(app: Arc<App>) -> TauriPlugin<R> {
    build(SolxState::new(Some(app), Backend::Fixed))
}

/// Thin-client plugin. `config` should be an app-private `ConfigService`
/// (e.g. `ConfigService::open_in(<app config dir>)`); a URL/token persisted
/// there by an earlier `connect` is reconnected to at startup.
pub fn client<R: Runtime>(config: Arc<ConfigService>) -> TauriPlugin<R> {
    let app = commands::restore_client(&config);
    build(SolxState::new(app, Backend::Client { config }))
}

fn build<R: Runtime>(state: SolxState) -> TauriPlugin<R> {
    Builder::new("solx")
        .invoke_handler(tauri::generate_handler![
            commands::types_save,
            commands::types_get,
            commands::types_delete,
            commands::types_list,
            commands::types_paths,
            commands::types_resolve,
            commands::types_validate,
            commands::docs_save,
            commands::docs_get,
            commands::docs_delete,
            commands::docs_list,
            commands::docs_paths,
            commands::docs_search,
            commands::actions_save,
            commands::actions_get,
            commands::actions_delete,
            commands::actions_list,
            commands::actions_paths,
            commands::actions_search,
            commands::actions_exec,
            commands::files_put,
            commands::files_get,
            commands::files_delete,
            commands::files_list,
            commands::info,
            commands::connect,
            commands::disconnect,
        ])
        .setup(move |app, _api| {
            app.manage(state);
            Ok(())
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::COMMANDS;

    /// Every command must be granted by `solx:default` and registered in
    /// `generate_handler!` — a command missing from either fails only at
    /// runtime, as a permission or "command not found" error from `invoke`.
    #[test]
    fn command_list_matches_permissions_and_handler() {
        let perms = include_str!("../permissions/default.toml");
        let lib = include_str!("lib.rs");
        for cmd in COMMANDS {
            let perm = format!("\"allow-{}\"", cmd.replace('_', "-"));
            assert!(perms.contains(&perm), "permissions/default.toml is missing {perm}");
            assert!(lib.contains(&format!("commands::{cmd},")), "generate_handler! is missing {cmd}");
        }
        assert_eq!(perms.matches("\"allow-").count(), COMMANDS.len());
    }
}
