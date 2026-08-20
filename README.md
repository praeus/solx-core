# solx

**solx turns a database of actions into a live MCP server.** One searchable catalogue of tools — for you and your AI agents. Add a tool (WASM, CLI, REST) and it's live everywhere, no redeployment.

Register a WASM
component, a shell command, a REST endpoint, or a script — it becomes a tool
your LLM client can call immediately.

```sh
# Register an action. It's a row in a database, not a line of code.
solx save action /weather/forecast --json '{
  "action_type": "webhook",
  "fn_name": "https://api.weather.gov/points/{lat},{lon}",
  "description": "Get the forecast for a coordinate pair.",
  "param_type_ref": "/weather/ForecastParams"
}'

# It is now a tool named act__weather__forecast in every connected MCP client.
```

That's the whole loop. `tools/list` is a live query against the actions
database — there is no hand-written tool layer to keep in sync, and no
separate CRUD tool surface, because entity CRUD, search, files, and secrets
are themselves just actions in the `/builtin` catalogue.

## Actions

An action is a row with a dispatch kind. Everything above the dispatcher sees
one uniform call: `exec(path, name, params) -> result`.

| `action_type` | What `fn_name` means | Use it for |
|---|---|---|
| `wasm`     | An export of a `wasm32-wasip2` component | Sandboxed third-party logic |
| `webhook`  | The URL to call (with OAuth/bearer auth resolution) | REST APIs |
| `command`  | A **key** into the `command_actions` allowlist | Local tools and binaries |
| `script`   | A `.solx` script artifact | Composing other actions |
| `internal` | A native handler | The `/builtin` catalogue |

Because the kind is data, one registry backs all five, and every surface —
CLI, HTTP, MCP, JS, web UI — gets all of them at once.

## Paths

Documents, actions, and types share a single directory-style namespace: a
flat store keyed by `path` + `name`. `/builtin/document/search_documents`,
`/packages/solx-google/gmail-send`, `/weather/forecast`.

The namespace is also the access control for tool catalogues. Set
`SOLX_MCP_PATH_PREFIX=/packages/solx-google` and that MCP instance exposes
only Google's actions — so you can run several narrowly-scoped servers
instead of flooding one model's context with every action you own.

## Security posture

Executing shell commands and remote code on request is the core function
here, so the boundaries are explicit:

- **Deny-by-default command allowlist.** A `command` action's `fn_name` is
  never a literal command — it's a key into `command_actions` in
  `solx-config.json`. The actions database can never hold an executable
  string, only a pointer to an out-of-band, admin-approved definition. An
  unregistered key is refused, including when the allowlist is entirely
  unset.
- **Deny-by-default webhook allowlist.** A `webhook` action's URL must match
  a prefix in `allowed_webhook_base_urls`.
- **Guests cannot self-grant.** `entity_save_action` refuses to create,
  modify, or delete `command` and `webhook` actions. That built-in is the
  only route to action creation available to an MCP client, a WASM guest, or
  a `.solx` script — so a model or a package's code cannot grant itself shell
  or outbound HTTP. It checks the *stored* row, not just the payload, since
  `save` is a merge-upsert.
- **Packages declare their grants.** A package's `package.json` lists the
  allowlist entries it needs; install grants exactly those and records them,
  uninstall revokes exactly those (unless another installed package also
  declares them).
- **Secrets are scoped and masked.** `get_secret`/`set_secret` resolve only
  against the *calling* action's own `action_config.secrets`. Reads through
  `ActionManager::get`/`list` redact `action_config.secrets` and `auth`, so
  keys never cross the HTTP boundary or reach an MCP client — and writes run
  the inverse merge, so a fetch → edit → save round trip can't silently
  destroy a key.
- **WASM guests are sandboxed by construction.** The `custom-action` world
  imports only action dispatch, artifact reads, and logging.
  No direct database, filesystem, or system access.

**What is not enforced yet:** anything saved to the actions database is
executable, and callers are undifferentiated beyond "is / isn't another
action." There is no package signing and no general permission system.

Treat a solx instance as running with your own privileges, and scope what you
install accordingly. [SECURITY.md](SECURITY.md) has the full threat model,
the complete list of known gaps, and guidance on running solx safely;
[docs/future-security-enhancements.md](docs/future-security-enhancements.md)
has the reasoning behind each gap and the intended direction.

## Surfaces

Every surface is built on the same four traits in `solx-surface`
(`TypeManager`, `FileStore`, `DocManager`, `ActionManager`), used through
`Arc<dyn _>`:

- **`solx`** — the CLI: `save`, `get`, `delete`, `exec`, `list`, `search`,
  `script`, `install-package`. A running action's console renders live to
  stderr, so `solx exec … | jq` still gets one clean JSON result on stdout.
- **`solx-server`** — HTTP over `127.0.0.1`, bearer-token gated, CORS-enabled,
  with an MCP endpoint at `/mcp`.
- **`solx-mcp`** — the same MCP server over stdio, with path-prefix scoping.
  Long-running tool calls stream their console back as
  `notifications/progress`, so a model sees log lines while the call is still
  in flight.
- **`solx-client`** — a Rust HTTP implementation of those same traits, so any
  local caller becomes a remote one without touching call sites.

## Crates

| Crate | Purpose |
|-------|---------|
| `solx-surface`  | Foundation: entity DTOs, `SolxError`/`Result`, wire types, path helpers, and the manager **traits** (the client/server seam). Dependency-light. |
| `solx-config`   | `solx-config.json` with cross-process-safe read-modify-write (mtime-guarded reads, advisory lock on write, unknown-field preservation). |
| `solx-types`    | Type registry: JSON-schema types by path, type groups, validation. |
| `solx-files`    | On-disk byte store for files attached to docs and actions. |
| `solx-docs`     | Document store: links, file refs, type validation, Tantivy full-text + path-faceted search. |
| `solx-actions`  | Action store and execution: all five dispatch kinds, plus consoles, detached invocations, streaming HTTP, OAuth loopback and scoped secrets. |
| `solx-scripts`  | The solx shell pipeline language (`;`, `\|`, `$var`), decoupled from the CLI via a `CommandRunner` trait. |
| `solx-packages` | Install/uninstall packages by running their `install.solx` and recording their allowlist grants. |
| `solx-manager`  | Wires the local implementations into the `Solx` facade. |
| `solx-cli`      | The `solx` binary. |
| `solx-server`   | Axum HTTP server + `/mcp` endpoint. |
| `solx-mcp`      | MCP server over stdio. |
| `solx-client`   | HTTP implementations of the manager traits (remote mode). |

A sibling `solx-wasm/` workspace (separate build target) holds
`solx-custom-actions-lib`, the SDK for authoring WASM actions.

## Quick start

```sh
cargo build --release

# A custom type, then a document validated against it
solx save type /types/custom/Person --json '{"schema":{"type":"object","required":["name"],"properties":{"name":{"type":"string"}}}}'
solx save doc /research/ai/note --type /types/custom/Person --json '{"contents":{"name":"Ada"},"title":"AI note"}'
solx search Ada --path /research

# Register a command action — the allowlist entry comes first, by design
solx save action /demo/echo --json '{"action_type":"command","fn_name":"echo-number"}'
solx exec /demo/echo
```

Then point an MCP client at it:

```sh
cargo run -p solx-server         # http://127.0.0.1:8766, /mcp
cargo run -p solx-mcp            # or stdio
```

`solx-server`'s bearer token lives in `solx-config.json` as `server_token`,
generated on first start. See [solx-cli/examples/](solx-cli/examples/) for
runnable end-to-end scripts covering each action kind.

## Storage

Docs, actions, and types each own a **separate** libsql/SQLite database
(`db/solx-docs.db`, `db/solx-actions.db`, `db/solx-types.db`); cross-entity
references use full path strings resolved at write time. Data lives under
`%APPDATA%/praeus/solx` (Windows) or `~/.praeus/solx`, overridable with
`SOLX_APPDATA_DIR`.

## Companion repos

- **solx-packages** — installable action bundles: Google Workspace, Ollama,
  MCP-server import, Firefox control, media extraction, and more.
- **solx-js** — TypeScript SDK: in-process Neon bindings plus a browser-safe
  HTTP client implementing the same manager interfaces.
- **solx-web** — React UI for documents, actions, types, and files, talking
  directly to `solx-server`.

## Status

Early but real: 323 tests pass across the workspace, including in-process MCP
client/server integration tests. Interfaces may still shift.

Design notes and roadmaps live in [docs/](docs/).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for build, test, and layout notes.
Small fixes are welcome as PRs; changes to a `solx-surface` trait, the
`/builtin` catalogue, or the security posture are worth an issue first.

Security issues should be reported privately — see [SECURITY.md](SECURITY.md).

## License

MIT OR Apache-2.0.
