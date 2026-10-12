// The plugin's command names — the single list `build.rs` generates the
// `allow-*` permissions from, and that `permissions/default.toml` and
// `tauri::generate_handler!` in `lib.rs` must match (both checked by unit
// tests in `lib.rs`). Included textually by both, so it can't depend on
// anything.
const COMMANDS: &[&str] = &[
    "types_save",
    "types_get",
    "types_delete",
    "types_list",
    "types_paths",
    "types_resolve",
    "types_validate",
    "docs_save",
    "docs_get",
    "docs_delete",
    "docs_list",
    "docs_paths",
    "docs_search",
    "actions_save",
    "actions_get",
    "actions_delete",
    "actions_list",
    "actions_paths",
    "actions_search",
    "actions_exec",
    "files_put",
    "files_get",
    "files_delete",
    "files_list",
    "info",
    "connect",
    "disconnect",
];
