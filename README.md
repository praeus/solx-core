# solx

Modules for managing structured documents and extensible actions through the
`solx` CLI. A lean reimplementation of the sol core: documents, actions, types,
and files organized by a directory-style **path** namespace (a flat store with
`path` + `name` identity), with config, packages, and a shell-style scripting
language. No LLM, extraction, browser, permissions, or knowledge bases.

## Crates

| Crate | Purpose |
|-------|---------|
| `solx-surface`  | Foundation: entity DTOs, `SolxError`/`Result`, list/search wire types, path helpers, and the manager **traits** (the client/server seam). Dependency-light. |
| `solx-config`   | `solx-config.json` service with cross-process-safe read-modify-write (mtime-guarded reads, advisory file lock on write, unknown-field preservation). |
| `solx-types`    | Type registry (own DB): JSON-schema types by path + type groups; validation; seeds primitives + `BlogPostWithComments`. |
| `solx-files`    | On-disk byte store for files attached to docs/actions (no DB). |
| `solx-docs`     | Document store (own DB): links, file refs, type validation via `solx-types`, Tantivy full-text + path-faceted search. |
| `solx-actions`  | Action store (own DB): execution of `Command` (config allowlist) and `Webhook` (URL allowlist) actions. WASM + full OAuth loopback are deferred. |
| `solx-scripts`  | The solx shell pipeline language (`;` / `|` / `$var`), decoupled from the CLI via a `CommandRunner` trait. |
| `solx-packages` | Install/uninstall packages by running their `install.solx` script and recording them in config. |
| `solx-cli`      | The `solx` binary. Wires the local manager impls and dispatches `post`/`get`/`delete`/`exec`/`list`/`search`/`script`. |

Each of docs, actions, and types owns a **separate** libsql/SQLite database
(`db/solx-docs.db`, `db/solx-actions.db`, `db/solx-types.db`); cross-entity
references (e.g. a document's type) use full path strings resolved at write time.

## Client/server readiness

Every manager is an `async_trait` defined in `solx-surface` and used through
`Arc<dyn _>`, so a future `solx-client` (HTTP proxy) and `solx-server` can be
added without changing callers.

## Quick start

```sh
cargo build
# custom type, then a document validated against it
solx post type /types/custom/Person --json '{"schema":{"type":"object","required":["name"],"properties":{"name":{"type":"string"}}}}'
solx post doc /research/ai/note --type /types/custom/Person --json '{"contents":{"name":"Ada"},"title":"AI note"}'
solx get doc /research/ai/note
solx list doc --path /research
solx search Ada --path /research
```

Data lives under `%APPDATA%/praeus/solx` (Windows) / `~/.praeus/solx`
(override with `SOLX_APPDATA_DIR`).
