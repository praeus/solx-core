# solx — Design, Structure & Progress

_Last updated: 2026-07-27_

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
knowledge bases (replaced by paths), action-scripts, and (for now) in-process
IPC/HTTP servers.

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
  solx-cli/        the `solx` binary
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
solx-cli      →  all of the above
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

Config holds directory locations, the per-entity DB filenames, the
`command_actions` allowlist (shell execution), the `allowed_webhook_base_urls`
allowlist (web actions), and the installed-package registry.

---

## 6. Actions & execution

Actions are stored with their parameter/result type references, an
`action_type`, and an `action_config`. On `exec`, parameters are validated
against the parameter type (if declared), then dispatched:

- **Command** — runs a shell command whose key is in the `command_actions`
  allowlist; parameters are passed as JSON via stdin and the `SOLX_PARAMS` env
  var; stdout is parsed as JSON (or returned as a string).
- **Webhook** — HTTP POST to a URL permitted by `allowed_webhook_base_urls`,
  with optional bearer-token / custom headers from `action_config`.

Both are gated by config allowlists — the "whitelist" mechanism.

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

## 8. Client/server readiness

Every manager is an object-safe `#[async_trait]` defined in `solx-surface`
(`TypeManager`, `FileStore`, `DocManager`, `ActionManager`, plus an aggregate
`Solx` facade), using only DTOs and `serde_json::Value`. Phase 1 ships only the
local libsql/Tantivy impls, and the CLI codes against `Arc<dyn _>` — never the
concrete structs.

This mirrors the old repo's `sol-surface`/`sol-client`/`sol-server` split, so a
future `solx-client` (HTTP proxy implementing the same traits) and `solx-server`
(hosting a local impl) can be added **without changing callers**. `async` was
chosen specifically to keep these traits transport-agnostic.

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
| `solx-surface` | DTOs, error, wire types, path helpers, manager traits | path normalize / full_ref / split_ref |
| `solx-config` | cross-process RMW, mtime reload, package registry | unknown-field preservation, cross-instance reload, registry |
| `solx-files` | put/get/delete/list, traversal rejection, conventional paths | roundtrip, traversal rejected |
| `solx-types` | own DB, schema validation + enrichment, seed incl. `BlogPostWithComments` | seed, post/get/validate, list+delete, enrich |
| `solx-docs` | own DB, cross-DB type validation, Tantivy full-text + path facet | post/get/list/search/delete, invalid-contents rejection |
| `solx-actions` | own DB, `Command` + `Webhook` execution with allowlists | CRUD, command exec via allowlist, wasm-unsupported |
| `solx-scripts` | pipeline language over `CommandRunner` | assign+substitute, quote-aware tokenize |
| `solx-packages` | install/uninstall via script runner + config registry | (exercised via CLI) |
| `solx-cli` | wires local impls; `post/get/delete/exec/list/search/script` | end-to-end smoke test |

An end-to-end CLI run verified: custom type → document validated against it →
`list --path` → path-faceted `search` → action CRUD → file put/get/list → a
`script` pipeline resolving `$d.type_ref` across a captured variable, and
confirmed the three separate database files are created.

### Deferred (flagged, not silently dropped)

- **WASM action execution** — returns a clear "not implemented" error. Porting
  the wasmtime component host and the built-in action ABI is a sizeable
  follow-up.
- **Full OAuth loopback** — webhook actions support bearer-token/header auth
  from `action_config`, but the interactive RFC 8252 loopback flow is not yet
  ported.
- **Type-group facet in document search** — groups are stored on types; doc
  search currently facets on `path` and `type_ref` only.
- **Client/server crates** — `solx-client` / `solx-server` are not built yet;
  the trait seam is in place for them.

### Suggested next steps

1. Aggregate `Solx` facade implementation in the CLI (or a small `solx-core`
   assembly crate) to simplify wiring and the future server.
2. WASM component host for built-in actions.
3. `solx-server` (HTTP) + `solx-client` (proxy) once a transport is chosen.
4. Optionally, an MCP surface exposing the same tools to models.
