# solx — Design, Structure & Progress

_Last updated: 2026-08-05_

This document describes the new `solx` system: why it exists, the architecture
and key design decisions, and the current implementation status. It is the
canonical overview for the `solx-core` workspace.

---

## 1. Background & motivation

`solx` is a lean reimplementation of the core of the older `sol` system. `sol`
had grown into a tightly-coupled backend that bundled document/action/type/file
storage together with instruction planning, deterministic extraction, an Ollama
LLM integration, browser automation, permissions, PGP trust, and a desktop app,
all behind one ~150-method manager trait. Identity was global-by-name and
"knowledge bases" were a bolt-on scoping filter.

`solx` keeps only the durable primitives — **documents, actions, types, and
files** — plus the supporting **config, packages, and scripting**, and
reorganizes them around a directory-style **path** namespace. The commands are
meant to be driven equally well by a human or a model.

Explicitly **out of scope**: LLM/Ollama, instruction planning & execution,
deterministic extraction & media, browser actions, permissions & trust,
knowledge bases (replaced by paths), and action-scripts.

This is a **fresh, standalone repository** with **no dependency on any `sol-*`
crate**. Logic was ported/rewritten rather than shared; the old repo is
untouched. Some duplication (e.g. a tiny per-crate libsql helper) is accepted in
exchange for clean boundaries.

---

## 2. Core model: paths instead of knowledge bases

Every document, action, and type carries:

- a **`path`** — a directory-like string, leading slash canonical, no trailing
  slash (e.g. `/research/ai`); the root is `/`.
- a **`name`** — a single segment (no `/`, `\`, or `:`).

**Identity = `(path, name)` uniqueness**, scoped to that entity's own database.
The path *is* the namespace — there are no knowledge bases. A "full reference"
is the canonical string `"<path>/<name>"` (e.g. `/research/ai/note`, or `/note`
at the root).

**Cross-entity references are by full path string, never a SQL foreign key**,
because the entities live in separate databases:

- A document's or action's type is a `type_ref` like `/types/custom/Person`.
  It is resolved against the types database and validated at write time; a
  dangling reference is a validation error.
- Documents may link to other documents (by full reference) or to URLs (by
  string). Actions are intentionally not linkable.

Path/name normalization and `full_ref`/`split_ref` live in
`solx-surface::path` and are unit-tested.

---

## 3. Workspace structure

```
solx-core/
  solx-surface/    foundation: DTOs, error, wire types, path helpers, manager traits
  solx-config/     solx-config.json service (cross-process safe)
  solx-types/      type registry (own DB) + JSON-schema validation
  solx-files/      on-disk byte store (no DB)
  solx-docs/       document store (own DB) + Tantivy search
  solx-actions/    action store (own DB) + execution
  solx-scripts/    shell pipeline language over a CommandRunner trait
  solx-packages/   package install/uninstall
  solx-manager/    wires the local manager impls together (the App/Solx assembly)
  solx-cli/        the `solx` binary
  solx-mcp/        MCP server exposing actions/docs/types/files as tools (stdio)
  solx-client/    HTTP proxy implementations of the manager traits (talks to solx-server)
  solx-server/    HTTP server hosting the local solx managers, so multiple clients can share one appdata dir
solx-wasm/         sibling workspace: SDK for third-party custom WASM actions
```

### Dependency graph (acyclic)

```
solx-surface  →  (serde / uuid / chrono / async-trait only)
solx-config   →  solx-surface
solx-files    →  solx-surface, solx-config
solx-types    →  solx-surface, solx-config
solx-docs     →  solx-surface, solx-config          (+ TypeManager injected)
solx-actions  →  solx-surface, solx-config          (+ TypeManager injected)
solx-scripts  →  solx-surface
solx-packages →  solx-surface, solx-config, solx-scripts
solx-manager  →  solx-surface, solx-config, solx-types, solx-files, solx-docs, solx-actions
solx-client   →  solx-surface, reqwest               (HTTP proxy impls of the manager traits)
solx-server   →  solx-manager, solx-surface, solx-config, axum  (hosts the local managers over HTTP)
solx-cli      →  solx-manager, solx-actions (constants), solx-scripts, solx-packages, solx-client
solx-mcp      →  solx-manager, solx-surface, solx-config, rmcp
```

Note that `solx-docs` and `solx-actions` do **not** depend on the `solx-types`
crate directly — they receive an `Arc<dyn TypeManager>` at construction, so the
type registry is swappable and the crates stay decoupled.

---

## 4. Storage

Each of docs, actions, and types owns a **separate libsql/SQLite database**:

```
<appdata>/db/solx-docs.db
<appdata>/db/solx-actions.db
<appdata>/db/solx-types.db
```

- Uniqueness is enforced by a SQL `UNIQUE(path, name)` per table.
- Document links and file references are stored as JSON columns (the file
  *bytes* live on disk, not in the DB).
- **Files need no database.** `solx-files` reads/writes bytes under a configured
  files root with conventional relative paths
  (`files/docs/<id>/<name>`, `files/docs/shared/<name>`,
  `files/actions/<id>/<name>`, …). A file with no matching row is simply
  loose/shared, which is allowed. Path traversal (`..`, absolute paths) is
  rejected.
- Documents are additionally indexed in **Tantivy** for full-text search with
  faceting over `path` (via indexed ancestor terms) and `type_ref`. Bare query
  terms are prefix-matched.

### Appdata layout

`%APPDATA%/praeus/solx` (Windows) / `~/.praeus/solx` (other), overridable with
`SOLX_APPDATA_DIR`.

```
<appdata>/
  solx-config.json
  db/            solx-docs.db, solx-actions.db, solx-types.db
  files/         file byte store
  search_index/  docs/  (Tantivy index)
  logs/
```

All locations are read from `solx-config.json` with these as defaults.

---

## 5. Config service & cross-process concurrency

`solx-config` backs `solx-config.json`. Because multiple `solx` processes may
run at once, it is designed for cross-process safety:

- **Reads are lock-free.** An in-memory cache is refreshed by comparing the file
  mtime; if another process wrote the file, the next read reloads it.
- **Writes take an OS advisory exclusive lock** (`fs2`) over the config file for
  the whole read-modify-write, serializing writers across processes.
- The write path edits the file as a raw `serde_json::Value`, so **unknown
  fields** written by other tools or newer versions survive.

Config holds directory locations, the per-entity DB filenames, and the
installed-package registry. There is currently **no allowlist for shell or
webhook execution** — see §6 and §10's "Suggested next steps".

---

## 6. Actions & execution

Actions are stored with their parameter/result type references, an
`action_type`, and an `action_config`. On `exec`, parameters are validated
against the parameter type (if declared), then dispatched:

- **Command** — `fn_name` is the literal shell command solx-core runs (via
  `cmd /C` on Windows, `sh -c` elsewhere); parameters are passed as JSON on
  **stdin only** (no env var — see §10's gotchas); stdout is parsed as JSON
  (or returned as a string). A bare filename in `fn_name` is **not** resolved
  against `action_config.cwd` by cmd.exe/sh — use an explicit `.\name.exe` /
  `./name` prefix for a binary that lives in `cwd`.
- **Webhook** — HTTP POST to `fn_name` (the URL), with optional bearer-token /
  custom headers from `action_config`, plus
  `oauth_refresh`/`oauth_service_account`/`oauth_authorization_code` flows.
  There is no URL allowlist — any action posted to the DB can POST anywhere
  (see §10).
- **Internal** — native Rust dispatch, no shell/HTTP/WASM (`solx-actions/src/internal.rs`).
  This is where the entire `/builtin` catalogue lives: entity CRUD
  (`entity_post/get/delete/list_{document,type,action}`), document field ops,
  full-text/faceted search, general-purpose file-store access, an
  in-process environment store, HTML fetch, scoped secrets, and the OAuth
  2.0 authorization-code loopback (see [`built-in-actions.md`](built-in-actions.md)
  for the full reference).
- **Wasm** — a *custom*, third-party component executed under wasmtime
  (`solx-actions/src/wasm_host.rs`), sandboxed to `action-exec` (recurse
  into any other action, including every built-in above) and `artifact-read`
  (unrestricted file reads). There is no first-party/"trusted" WASM world
  anymore — every built-in operation moved to `Internal` dispatch instead
  (native dispatch has no sync/async host-function bridging and needs no
  separate guest build/packaging step), so `Action.trusted` no longer
  affects WASM execution.

### 6.1 Execution is non-blocking end to end

Every backend above runs without parking a thread for the duration of the
work it waits on. This was not originally true and the failure modes were
severe, so the specifics are worth recording:

- **WASM runs on wasmtime fibers.** The bindings are generated with
  `imports`/`exports: { default: async }`, so the guest export is invoked via
  `TypedFunc::call_async` → `Store::on_fiber`: the guest gets its own stack,
  and whenever a host import's future is pending the fiber suspends and the
  OS thread returns to the executor. Previously each guest ran inside
  `spawn_blocking` with host imports bridging back via `Handle::block_on`, so
  **every `action-exec` nesting level pinned one blocking-pool thread** while
  awaiting the level beneath it — a hard ceiling at tokio's
  `max_blocking_threads` (512, shared across concurrent requests), and a hang
  rather than an error once exhausted.

  This needed **nothing from guests**. `call_async` runs an ordinary sync-ABI
  component; only the host side is async. Native component-model async — the
  `call_concurrent` path, opted into by adding the `store` flag to the bindgen
  config — *would* require guests rebuilt against the async ABI, and is
  deliberately not used. componentize-qjs's `Runtime::OptSizeSync` output
  continues to run unmodified.
- **Guest preemption.** Fibers only help a guest that *awaits*; a
  compute-bound one never yields. The engine enables `epoch_interruption`
  with a dedicated ticker thread, and each store uses
  `epoch_deadline_async_yield_and_update` so a busy guest hands the worker
  back. Yielding never terminates, so a wall-clock `tokio::time::timeout`
  bounds the whole invocation (default 300s, per-action override via
  `action_config.timeout_secs`).
- **Components are compiled once.** `Component::from_binary` is a full
  Cranelift compile that used to run on *every* execution. It is now cached
  process-wide by artifact hash, and compiled on the blocking pool on a miss.
- **Command actions** use `tokio::process` with `kill_on_drop`, feeding stdin
  concurrently with draining stdout (writing it all up front deadlocked on
  payloads past the pipe buffer) and bounded by the same `timeout_secs`.
  Caveat: killing the shell does not kill its descendants on either platform,
  so a long-running grandchild can outlive the timeout.
- **Tantivy** sits behind a dedicated writer thread fed by a channel, which
  batches queued mutations behind a **single** commit; reads clone the
  Arc-backed searcher and run on the blocking pool. It was previously one
  `std::sync::Mutex<DocIndex>` that committed — segment serialize, `fsync`,
  reader reload — on every individual document write, with all searches
  serialized behind the same lock. `submit` still awaits its batch's commit,
  so read-your-writes is preserved; batching only merges concurrent writers.
- **Keyring** calls (blocking OS credential-manager IPC — a synchronous D-Bus
  round trip on Linux) run on the blocking pool, as does RS256 assertion
  signing. Compiled `jsonschema` validators are cached rather than rebuilt per
  validation.

Known remaining case: libsql local drives embedded SQLite synchronously
despite its async surface, so `PRAGMA busy_timeout=10000` can park a worker
inside SQLite's busy handler for up to 10s under write contention.

---

## 7. Scripting & packages

`solx-scripts` is the solx shell pipeline language, lifted out of the CLI into a
reusable library:

- statements separated by `;`, pipeline stages by `|` (each stage's JSON output
  feeds the next), and `$name = <pipeline>` capture with `$name.field.sub`
  substitution;
- quote-aware tokenization (including escaped quotes inside JSON arguments);
- execution is transport-agnostic via a `CommandRunner` trait. The CLI
  implements it by re-parsing each stage with clap and dispatching through its
  own handlers.

`solx-packages` installs a package directory (`package.json` + `install.solx`)
by running the script through that runner and recording the package in config;
uninstall runs `uninstall.solx` if present.

---

## 8. Client/server (HTTP) transport

Every manager is an object-safe `#[async_trait]` defined in `solx-surface`
(`TypeManager`, `FileStore`, `DocManager`, `ActionManager`, plus an aggregate
`Solx` facade), using only DTOs and `serde_json::Value`. Two parallel impls
back every manager:

- **Local** (`Local*` in `solx-types`/`solx-docs`/`solx-actions`/`solx-files`)
  — the in-process libsql/Tantivy impls wired by `solx-manager::App::wire_local`.
- **Remote** (`Remote*` in `solx-client`) — `reqwest`-backed HTTP proxies
  that hit a running `solx-server`. `solx-server` itself always uses the
  local impl (built via `App::build_local` so it cannot proxy to itself),
  binds `127.0.0.1:<port>` (default from config's `server_port`), and
  gates every data route behind a bearer token read from config's
  `server_token` (auto-generated on first start). A `SOLX_SERVER_URL` env
  var or a config `server_url` flips `solx-manager` into remote mode for
  any `solx-cli`/`solx-mcp` process pointed at the server.

`async` was chosen specifically to keep these traits transport-agnostic —
local and remote impls are interchangeable behind `Arc<dyn _>`, so callers
(`solx-cli`, `solx-mcp`) never see the difference.

---

## 9. CLI surface

The `solx` binary exposes:

- `post <entity> <ref>` — upsert (create-or-replace); body from `--json`,
  piped input, or stdin; `--type` sets a document's type; `--file` supplies a
  file's bytes.
- `get <entity> <ref>`, `delete <entity> <ref>`
- `exec <ref>` — run an action (params from `--json`/piped input)
- `list <entity>` — pagination (`--limit`/`--offset`) and an optional `--path`
  facet
- `search <query>` — full-text + faceted search over documents
- `script -e/-f/stdin`, `install-package`, `uninstall-package`, `list-packages`
- `json <value>` — emit a JSON literal (useful as a pipeline source)

Entities: `doc`, `action`, `type`, `file`.

---

## 10. Progress & status

### Implemented and tested (all crates compile; full suite green)

| Crate | Highlights | Tests |
|-------|-----------|-------|
| `solx-surface` | DTOs, error, wire types, path helpers, manager traits, `Solx` facade | path normalize / full_ref / split_ref |
| `solx-config` | cross-process RMW, mtime reload, package registry | unknown-field preservation, cross-instance reload, registry |
| `solx-files` | put/get/delete/list, traversal rejection, conventional paths | roundtrip, traversal rejected |
| `solx-types` | own DB, schema validation + enrichment, seed incl. `BlogPostWithComments` + `/builtin/types/*` param schemas | seed, post/get/validate, list+delete, enrich |
| `solx-docs` | own DB, cross-DB type validation, Tantivy full-text + path facet | post/get/list/search/delete, invalid-contents rejection |
| `solx-actions` | own DB, `Command`/`Webhook`/`Internal`/`Wasm` execution; the entire `/builtin` catalogue (30 actions) is native `Internal` dispatch | CRUD, command exec, internal dispatch (entity CRUD/search/files/env/secrets/OAuth), wasm host trait impls |
| `solx-scripts` | pipeline language over `CommandRunner` | assign+substitute, quote-aware tokenize |
| `solx-packages` | install/uninstall via script runner + config registry | (exercised via CLI) |
| `solx-manager` | implements `Solx`; shared manager wiring for `solx-cli`, `solx-mcp`, and `solx-server`; builds local or `solx-client` remote impls per config | (exercised via all three consumers' tests) |
| `solx-client` | HTTP proxy impls of the manager traits (`reqwest`); flips `solx-manager` into remote mode when `server_url`/`SOLX_SERVER_URL` is set | roundtrip against a real `solx-server` |
| `solx-server` | axum HTTP server hosting the local managers; bearer-token auth; `127.0.0.1`-only bind; always uses `App::build_local` (never proxies to itself) | end-to-end CLI/server/client roundtrip |
| `solx-cli` | wires `solx-manager`; `post/get/delete/exec/list/search/script`; works in both local and remote mode (no flag needed — picked up from config/env) | end-to-end smoke test + `examples/*.sh` (local and against `solx-server`) |
| `solx-mcp` | MCP server (stdio, `rmcp`); every action is a dynamic tool, no separate CRUD tool layer | in-process client/server integration test |

An end-to-end CLI run verified: custom type → document validated against it →
`list --path` → path-faceted `search` → action CRUD → file put/get/list → a
`script` pipeline resolving `$d.type_ref` across a captured variable, and
confirmed the three separate database files are created. The
`solx-cli/examples/*.sh` suite additionally exercises command/webhook/builtin/
OAuth actions end-to-end against a real compiled binary.

### Deferred (flagged, not silently dropped)

- **Type-group facet in document search** — groups are stored on types; doc
  search currently facets on `path` and `type_ref` only.
- **`param_type_ref` schemas for custom actions** — only the built-in
  catalogue's actions have real JSON-Schema `param_type_ref`s seeded; a
  user-created action gets the permissive fallback in `solx-mcp`'s tool list
  unless it sets its own.
- **`result_type_ref`** — present on the `Action` entity but not enforced
  anywhere; metadata only.

### Suggested next steps

1. ~~Aggregate `Solx` facade implementation~~ — done (`solx-manager`).
2. ~~WASM component host~~ — done, then narrowed to custom-only actions once
   the built-in catalogue moved to native `Internal` dispatch.
3. ~~An MCP surface exposing the same tools to models~~ — done (`solx-mcp`).
4. ~~HTTP client/server transport~~ — done (`solx-server` + `solx-client`,
   axum/reqwest); `solx-manager` picks local vs remote impls from
   `server_url`/`SOLX_SERVER_URL` per process.
5. MCP Resources/Prompts (e.g. `solx://doc/{path}/{name}` URI templates) for
   GUI/context-attachment MCP clients — `solx-mcp` is tools-only today.
6. Real config-level allowlists for `Command`/`Webhook` actions — `exec.rs`
   currently runs `fn_name` directly with no allowlist check. Deliberately
   deferred rather than designed now, to avoid adding permission-system
   complexity while solx-core's core surface is still being built out;
   everything running through it today is trusted by construction
   ("posted to the DB = trusted"). Recorded here so the gap isn't lost:
   - **Package signing and verification at install time**, so
     `install-package` can refuse a package whose signature doesn't check
     out rather than trusting any local directory unconditionally.
   - **A permissions module gated on caller identity** — flagged as
     difficult/extensive, since it requires solx-core to track *who* (which
     human/model/action) is invoking a given action, which nothing today
     does.
   - **File-operation sandboxing to the files directory** — already true
     today (`solx-files` rejects path traversal / absolute paths); keep this
     true as the rest of this hardening is built out.
   - **Secrets masking in `action_config`** — action configs can embed
     credentials; these should be redacted when read back by anything other
     than the action that owns them.

### Known gotchas (found integrating `solx-omniparse`/`solx-quickjs` from
`solx-packages`, verified against real installs/builds/execs, not just code
reading)

- **`.solx` scripts have no comment syntax at all.** The whole file is split
  purely on `;` (quote-aware); there's no `#`/`//`/`;`-at-line-start comment
  concept like old sol's dialect had. A `; some note` line with no closing
  `;` silently glues onto the next real statement and corrupts it. Keep
  package/operator documentation in the package's `README.md`, not inline in
  `install.solx`/`uninstall.solx`.
- **A `Command` action's `fn_name` needs an explicit relative-path prefix**
  (`.\name.exe` on Windows, `./name` on POSIX) to resolve a binary that lives
  in `action_config.cwd` — `cmd /C name.exe` does **not** search the child
  process's current directory for a bare filename the way an interactively
  typed command does.
- **`componentize-qjs` (used by `solx-quickjs`) requires the JS entry file to
  export a `runner` object** matching the WIT interface identifier —
  `export const runner = { run(actionName, params) { ... } }` — not a bare
  top-level `export function run(...)`. The latter compiles fine but fails at
  *runtime* with `interface 'sol:actions/runner@0.1.0' not found: FromJs {
  from: "undefined", to: "object" }`. This was the actual root cause of an
  initially-mysterious content-independent `unreachable` wasm trap that
  looked like a wasmtime version mismatch (see next point) but wasn't.
- **`wasmtime`/`wasmtime-wasi` were bumped from 28 to 47** in `solx-actions`
  (`wasm_host.rs`) to test the above trap's original hypothesis. The version
  bump was not actually the fix (pinning the exact version `componentize-qjs`
  itself uses, 45.0.3, reproduced the identical trap), but it's still a good
  idea to have taken — solx-core was 17 major versions behind, and 47 is now
  verified working end-to-end with a real `componentize-qjs`-built component.
  Required API updates: `WasiView::ctx` now returns `WasiCtxView<'_>` instead
  of separate `ctx()`/`table()` methods; `wasmtime_wasi::add_to_linker_sync`
  moved to `wasmtime_wasi::p2::add_to_linker_sync`; the bindgen-generated
  `add_to_linker` now needs an explicit `HasSelf<HostState>` type argument.
- **Going async on wasmtime 47 has its own API shape.** `bindgen!`'s old
  `async: true` is gone, replaced by per-function config —
  `imports: { default: async }, exports: { default: async }`. Async imports
  are generated as `-> impl Future<Output = ..> + Send` (RPITIT), so the host
  impls are plain `async fn` with no `async_trait`. `Config::async_support` is
  **deprecated as a no-op**: async is always available and the choice is made
  per call site (`instantiate_async`/`call_async` vs. their sync twins), so
  the engine needs no flag for it. Adding the `store` flag alongside `async`
  is what switches codegen to `call_concurrent` (native component-model
  async, guests must use the async ABI) — do not add it unless that is
  actually intended.
