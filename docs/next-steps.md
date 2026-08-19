# Next steps — cross-repo roadmap

_Written 2026-08-15, capturing a planning discussion covering `solx-core`,
`solx-web`, `solx-js`, and the future Pi integration. Companion to
[design-and-progress.md](design-and-progress.md); update or retire sections
here as they land instead of letting this drift like a changelog._

Seven points, roughly in priority order. Each has a status, the design
grounding (old `sol` precedent where one exists), and a next action.

---

## 1. Command & Webhook allowlist (solx-core) — done

**Status: implemented** (`solx-config`: `command_actions`/`allowed_webhook_base_urls`
+ `CommandDef`; `solx-actions/src/exec.rs`: resolution/validation;
`solx-actions/src/lib.rs`, `solx-actions/tests/command_nonblocking.rs`,
`solx-server/tests/integration.rs`, `solx-cli/examples/{02,03}-*.sh`: updated
to register allowlist entries). Decided posture, both confirmed by the user
over the recommended old-sol defaults:

- **Deny-by-default**, not old sol's allow-all-when-unconfigured. An
  unregistered Command key or a Webhook URL matching no configured prefix is
  refused — including when the allowlist itself is entirely unset. This is a
  breaking change for any existing Command/Webhook action; there is no
  migration path other than registering it.
- **Global-only allowlist**, no per-action grant (old sol's `ActionLink`
  equivalent). Can be added later without breaking anything already built.

Below is the original research this was built from. `solx-actions/src/exec.rs`'s
doc comment used to be explicit that there was no gate at all: *"Actions are
trusted by virtue of being `post`ed into the actions database — there is no
separate config-level allowlist for either kind."* `fn_name` was the literal
shell command or URL, run/POSTed directly — that line is gone now.

**Old `sol` precedent** (`sol-manager/src/lib.rs`, `dispatcher/{commands,webhooks,mod}.rs`)
had two independent, already-shipped mechanisms worth porting almost as-is:

- **Command — indirection, not validation.** A Command action's `fn_name`
  was never a literal string; it was a *key* into
  `command_actions: HashMap<String, CommandActionDef>` in `sol-config.json`
  (`CommandActionDef { command, description?, cwd? }`).
  `resolve_command_key` looked the key up and hard-errored if absent:
  *"command key '{key}' is not registered in sol-config.json
  'command_actions'; add an entry to allow this command."* This means the
  actions DB itself can **never** hold an executable string — only a pointer
  to an admin-approved definition kept out-of-band in config. Stronger than
  an allowlist check on a string that's already stored as data.
- **Webhook — prefix allowlist, unioned per-action.**
  `allowed_webhook_base_urls: Vec<String>` in `sol-config.json`, checked by
  `validate_webhook_url` against the (possibly path-substituted) request URL
  — must start with one of the listed prefixes. Unioned per-request with
  base URLs granted via that action's `ActionLink` entities (an entity type
  solx doesn't have). Empty/absent allowlist = **allow all**, an explicit
  documented default (not a silent fallback), surfaced in a Settings ›
  Actions UI tab in old sol-browser.

**solx already has the identical shape**, just not for this: `env_mappings`
(`solx-config/src/types.rs:53-58`) is the same allowlist-by-indirection
idea — "only keys listed here are ever visible via `get_env`" — already
proven and tested in this codebase. The Command design above is a direct
extension of a pattern solx already trusts.

**As built:**

- `SolxConfig.command_actions: Option<HashMap<String, CommandDef>>` —
  `CommandDef { command, description?, cwd? }`. `run_command` resolves
  `fn_name` through this map before spawning; an unregistered key (or an
  entirely unset allowlist) is a hard error naming the fix, same tone as the
  existing `action_start` long-lived-host error. `CommandDef.cwd`, when set,
  takes priority over the action's own `action_config.cwd` — the allowlist
  author decides where an approved command runs, not the action invoking it.
- `SolxConfig.allowed_webhook_base_urls: Option<Vec<String>>` — prefix
  allowlist checked in `run_webhook` before the console's "POST ..." start
  log is even written, so a denied attempt leaves no trace beyond the
  returned error. No per-action grant list (solx has no `ActionLink`-equivalent
  entity) — global-only, per the second decision above.
- Both read live via `ConfigService` per call, matching `console_max_entries`
  etc. — no snapshot-at-startup staleness.
- New `ConfigService` methods: `command_actions()`, `register_command(key, def)`,
  `allowed_webhook_base_urls()`, `set_allowed_webhook_base_urls(list)` —
  usable today from a test or a future `solx config` CLI subcommand (none
  exists yet; the CLI examples write `solx-config.json` directly via a small
  `merge_config.py` helper in the meantime).

**Test fallout from flipping to deny-by-default:** every existing test/example
that saved a Command action with a literal `fn_name` (e.g. `"echo 42"`) or a
Webhook with an unregistered URL now fails until it registers one first. Fixed
across `solx-actions/src/lib.rs` (unit tests), `solx-actions/tests/command_nonblocking.rs`,
`solx-server/tests/integration.rs`, and `solx-cli/examples/{02,03}-*.sh`. Two
new tests assert the deny path itself: `exec_command_with_unregistered_key_is_denied`
and `exec_webhook_with_unlisted_url_is_denied_before_any_request`. Full
workspace build + `cargo test` across `solx-config`/`solx-actions`/`solx-server`
+ the CLI example suite all green as of this write-up. This is a real breaking
change for anyone with existing Command/Webhook actions saved from before this
landed — there is no migration, only registering entries in `solx-config.json`.

**Next action:** none — done. Revisit only if a real need for per-action grants
or a `solx config allow-command`/`allow-webhook` CLI surface shows up.

### 1a. Package-declared grants — the actual migration path for existing packages

Deny-by-default breaks every package that registers Command/Webhook actions
in its `install.solx` (`solx-omniparse`, `solx-media`, `solx-firefox`,
`solx-quickjs`, `solx-mcp-actions` — Command; `solx-google` — Webhook;
`solx-ollama`/`solx-livejournal` are Wasm-based and unaffected). Rather than
leave that as a manual `solx-config.json` edit per user, `package.json` can
now declare `command_actions`/`allowed_webhook_base_urls` in the exact same
shape `solx-config.json` itself uses (literally the same
`solx_config::CommandDef` type, deserialized straight from the manifest).
**Reinstalling a package is how its actions get whitelisted** — exactly the
workflow requested.

- `solx_packages::install_package` now grants every declared entry into the
  global allowlist *before* running `install.solx` (so the script, or a
  later `verify.solx`, can exec the package's own actions), then records
  exactly what it granted on `InstalledPackage.granted_commands`/
  `granted_webhook_prefixes`.
- `uninstall_package` revokes exactly that recorded set, after running
  `uninstall.solx`, **unless another currently-installed package's own
  recorded grants still claim the same key/prefix** — checked by scanning
  every other installed package's `granted_*`, not by re-parsing
  `package.json` (which could have drifted since install).
- **Collision policy** (confirmed by the user, second decision point): a key
  or prefix already granted by a *different* installed package is not a
  hard error — the new value wins (last-write-wins, same as reinstalling a
  package over itself), but `install_package` returns the collision as a
  warning (`InstallOutcome.warnings`, plus a `tracing::warn!`) naming both
  packages, so it's visible rather than silent.
- New `ConfigService` methods for this: `deregister_command`,
  `add_allowed_webhook_base_url` (idempotent), `remove_allowed_webhook_base_url`.
- Covered by `solx-packages/tests/allowlist_grants.rs` (5 tests: grant on
  install, no-op on a manifest with no grants, revoke-on-uninstall,
  collision-warns-and-overwrites-but-the-losing-package's-uninstall-doesn't-
  break-the-winner, and reinstalling your own package is never a collision).
- All six affected packages in the sibling `solx-packages` repo were
  migrated: each Command action's `fn_name` changed from the literal
  command to a key matching the action's own local name (e.g.
  `solx-omniparse-process-file`), with the real command moved into that
  package's new `package.json` `command_actions` entry; `solx-google`
  needed no `install.solx` change at all, just five API-host prefixes added
  to `allowed_webhook_base_urls` (Webhook `fn_name` stays the literal URL —
  only Command indirects through a key). Verified end-to-end against a real
  build: installing `solx-omniparse` in an isolated appdata dir granted
  both keys, `get action` showed the key-based `fn_name`, and `exec` against
  the actual (previously-built) `solx-omniparse-process-file.exe` returned
  `success: true`.

---

## 2. Secrets masking review (solx-core) — do alongside #1

**Status: already implemented, and it's solid.** `solx-actions/src/mask.rs`:

- `mask_action_config` redacts every string under `action_config.secrets`
  (the AES key blobs) and every string under `action_config.auth` *except*
  a small explicit visible-allowlist (`type`, `auth_url`, `token_url`,
  `redirect_uri`, `scope`, `keyring_service`, `keyring_account`, `*_env`).
  New auth fields are masked by default — allowlist-shaped, same philosophy
  proposed for #1.
- `unmask_merge` restores the real value wherever a caller echoes back the
  `"***"` sentinel, so a `get` → edit → `post` round trip doesn't destroy
  keys it was never shown. A literal `"***"` typed by a caller (not echoed
  from a prior read) is a hard error, not silent corruption.
- Wired correctly: `ActionManager::get`/`list` mask after calling
  `get_unmasked`; execution (`exec_as`) always reads through
  `get_unmasked` so real values still reach dispatch.
- 8 unit tests, including the round-trip case and the deliberately-tricky
  `client_secret` vs `client_id_secret` suffix ambiguity.

`design-and-progress.md` §10 still lists this as a "suggested next step" —
that's the doc being stale, not the code.

**Next action:**
- Update `design-and-progress.md` §10 to move this to done.
- Spot-check that every read surface (`solx-cli get action`, `solx-mcp` tool
  responses, the HTTP `GET /actions/:ref` route) actually routes through
  `ActionManager::get` and not `get_unmasked` — confirm no surface bypasses
  masking. (Nothing found in this pass suggests a bypass, but it wasn't
  exhaustively traced end-to-end through every transport.)

---

## 3. solx-js as a direct solx-server client (solx-js / solx-web) — done

**Status: implemented 2026-08-16.** solx-web's Bun backend (`solx-web/server/`)
is gone; the React frontend now talks to `solx-server` directly.

**As built:**

- **New package `@solx/http`** (`solx-js/packages/http/`) — a pure-`fetch`,
  zero-runtime-dependency (beyond `@solx/surface`) implementation of
  `TypeManager`/`FileStore`/`DocManager`/`ActionManager`, mirroring
  `solx-client`'s Rust `Remote*Manager`s route-for-route (`solx-client/src/
  {types,files,docs,actions}.rs`): same routes, same snake_case DTOs
  (`solx-surface::wire`), same base64 file encoding. Wire errors
  (`{"kind":"not_found",...}`) are reconstructed into `@solx/surface`'s
  `SolxError` via a small kind-mapping table. `connectHttp(serverUrl,
  token)` returns `{ types, files, docs, actions }` — no `config`/`scripts`
  members, since neither has an HTTP surface. Works unmodified in a browser
  or in Node (global `fetch`, no native code) — the browser-capable
  counterpart to the Neon-backed managers' `.connect()` mode, which needs a
  native `.node` binary and is Node-only. The NAPI/embed path
  (`solx/sdk`'s `createSolx`) is untouched, per the original plan.
- **`solx-server` CORS** — `tower-http`'s `CorsLayer::permissive()`, layered
  outermost in `build_router` (`solx-server/src/lib.rs`) so preflight
  `OPTIONS` is answered before the bearer-auth middleware ever sees it.
  Deliberate widening of the trust boundary (until now only Rust/Node
  clients could reach `solx-server`) — judged consistent with the existing
  posture (127.0.0.1-only bind + shared bearer token, no multi-tenant
  story anywhere yet), and the Bun backend it replaces already ran CORS
  `*` in front of the same server.
- **solx-web/web rewired**: `web/src/api/api.ts` keeps its exact original
  function names/signatures/snake_case shapes (near-zero churn to
  components/hooks) but now calls `@solx/http` directly instead of
  `fetch('/api/...')`; `web/src/api/wire.ts` is the old Bun backend's
  `server/src/wire.ts` conversion logic, moved client-side unchanged.
  `@solx/http`/`@solx/surface` aren't npm dependencies of `web/` — bun's
  `file:`-dependency copy step hit a reproducible EPERM on Windows for
  this cross-repo layout, so they're aliased straight to TS source instead
  (`web/vite.config.ts`'s `resolve.alias` + `web/tsconfig.json`'s
  `compilerOptions.paths`) — both packages are zero-dependency and
  browser-safe as-is, so there's nothing to build first anyway.
- **Connection/config split**: the browser now holds `serverUrl`/
  `serverToken` itself (`web/src/api/connection.ts`, `localStorage`,
  editable via a slimmed-down `SettingsPanel`). Everything else
  `solx.config.*` used to expose (`dataDirectory`, `filesDirectory`,
  package registry) has **no HTTP equivalent and never will** — config is
  local-only to a running `solx-server`/`solx-cli` process by design, in
  both the Rust and TS impls — so that functionality is gone, not moved.
- **File previews**: `solx-server` has no raw-byte-serving route (files are
  base64 JSON only) and the bearer token can't ride along on a plain
  `<img src>` URL, so the old `/api/files/raw?relPath=…` route is replaced
  by a ref-counted `blob:` URL cache (`web/src/api/objectUrlCache.ts` +
  `useFileObjectUrl`). The rich-text editor's persisted image nodes needed
  a custom Tiptap NodeView (`web/src/components/richtext/ResolvedImage.tsx`)
  since a node's `src` attribute is a static value, not something a
  `Promise` can resolve into — it now stores the bare `relPath` and
  resolves it to a `blob:` URL at render time (with back-compat for
  documents that already persisted the old `/api/files/raw?relPath=…`
  URL shape).
- Verified end-to-end against a live `solx-server`: CORS preflight,
  full types/docs/files CRUD + search round-trip, `SolxError` kind/detail
  reconstruction, builtin-action dispatch, and bearer-token rejection —
  plus clean `cargo build --workspace`, `bun run typecheck` (both repos),
  and `vite build`.

**Next action:** none — done. `solx-web/server/` and
`solx-web/scripts/vendor-solx.mjs` are deleted; `solx-web`'s root
`package.json` scripts (`dev`/`build`/`lint`/`typecheck`) now target
`web/` only.

---

## 4. MCP Resources & Prompts (solx-core) — next after #1

**Status: still open**, unchanged from `design-and-progress.md` §10 item 5.
Worth scoping once the allowlist lands. Resources exposing
`solx://doc/{path}/{name}`-style URIs are also a good fit for Pi's
context-attachment model (point 7) and for the path-scoping idea in point 5
— reasonable to design these two together.

---

## 5. Do paths mitigate the "too many MCP tools" risk? — done

**Status: implemented.** Both halves this section used to flag as missing
now exist:

- **MCP-level scoping:** `solx-mcp` reads an optional `SOLX_MCP_PATH_PREFIX`
  env var at startup and threads it into `ListOptions.path_prefix` for every
  `tools/list` call (`solx-mcp/src/server.rs`, `SolxMcpServer::path_prefix`).
  Unset behaves exactly as before (full catalogue); set, a client sees only
  that path and everything under it. Since the MCP `tools/list` request
  itself has no filter param (`rmcp`'s `PaginatedRequestParams` is
  cursor-only), scoping is a launch-time server config — run several
  `solx-mcp` instances, each with a different prefix, to present narrower
  toolsets to different clients/teams (e.g. one for `/builtin`, one for
  `/packages/solx-google`).
- **A real grouping to scope by:** the flat `/builtin` catalogue has been
  split into `/builtin/<area>/*` subpaths (`document`, `type`, `file`,
  `env`, `secrets`, `oauth`, `web` (+ `web/stream`), and the existing
  `console`/`action`) — see `solx-actions/src/seed.rs`. Only `random_string`
  stays flat, too small a group on its own to warrant a subpath. This was a
  breaking path rename for anything hardcoding an old flat `/builtin/<name>`
  reference; `db/solx-actions.db` needs deleting once after upgrading past
  it, same as the earlier `console`/`action` subdivision required (see
  `seed.rs`'s header comment).

**Next action:** none — revisit only if a client actually wants multiple
concurrently-running scoped `solx-mcp` instances wired up (today it's just
the mechanism + a grouping to use it with, not a deployed multi-instance
setup).

---

## 6. Finish the solx-web port: Actions / Types / Files parity + a new ActionRunner

**Status: revised 2026-08-16** after reading all seven old-sol files
(`sol-browser/src/entityEditors/actionEditorService.ts`;
`sol-browser/src/components/{TypeEditor,TypeSchemaEditor,TypeSelector,
ArtifactSelector,DataArtifactDialog,SharedArtifactDialog,TextArtifactDialog}.tsx`)
plus solx-web's current state in full. The previous "Documents tab only;
Actions/Types/Files are placeholders" status was stale — all three tabs
already have working list/browse/search/sort/paginate + create/edit/delete,
and Actions already has a synchronous run panel. This replaces that status
with the actual gap list.

**Already done, no work needed:**

- `ActionList`/`ActionEditor`, `TypeList`/`TypeEditor`, `FileList`/`FileEditor`
  — full CRUD, all wired through solx-server's generic REST routes.
  `ChipTagInput`/`JsonSchemaEditor`/`FilesField` are shared components reused
  across tabs already.
- The Files tab is a flat rel_path store browser (view/download/upload/delete)
  — this **is** the right architecture, not a stand-in for one. Old sol's
  "Artifact" system (`ArtifactScope`, `promote_to_shared`,
  owner-scoped naming like `actions::name::file`,
  `sol-manager/src/managers/artifact.rs`) has no equivalent in solx and
  doesn't need one: solx's `Document`/`Action.files: Vec<FileRef>` field plus
  the already-built `FilesField.tsx` picker is the simplified descendant of
  that whole subsystem. **Confirmed — closing the open question the previous
  version of this doc raised.** `ArtifactSelector`/`DataArtifactDialog`/
  `SharedArtifactDialog` have no port target.
- Old-sol concepts that don't exist in solx at all, so are out of scope
  outright (not deferred — there's no entity to port them onto): Permission
  entity (`permission_name`/`allowed_permission_names`), Schedule entity,
  custom-action project scaffolding (superseded by the `solx-quickjs`
  package's `build-javascript-action`), and the run `trace` array
  (`ActionExecResult` has no trace field — a backend gap, not a UI one).

**Real, portable gaps found:**

1. `ActionEditor` — typed Webhook fields (header rows, `timeout_secs`) and a
   typed Command `cwd` field, overlaid onto `action_config` the way old
   sol's `buildActionConfig` did — today these are only reachable via the
   raw-JSON "Advanced" textarea, for the two most common action types.
2. `ActionEditor` — wire up `files: FileRef[]`. Confirmed present on
   `Action`/`ActionInput` in `solx-surface/src/entities.rs` and already
   passed end-to-end by the Bun backend (`server/src/wire.ts`'s
   `actionToWire`/`actionInputFromWire` both handle it) — it's just never
   declared in `web/src/api/api.ts`'s `ActionSummary`/`createAction` types
   or rendered in the component. Reuse the same `FilesField` component
   `DocumentEditor` already uses. This also gives the current bare-text
   "Exec Artifact (bin name)" field something real to point at — a
   Wasm/Script/Widget action's `bin_name` names one of its own attached
   files.
3. `TypeSelector`-style combobox. Old sol's param/result type pickers were a
   searchable combobox with inline "Edit"/"+ New" buttons that opened a
   `TypeEditor` without leaving the form; `ActionEditor` currently uses a
   plain `<select>`. No backend changes needed to port it.
4. `FileEditor`/`FileList` — add an inline "New Text File" mode (Monaco +
   language picker, mirroring old sol's `TextArtifactDialog`) alongside
   today's disk-upload-only flow. Useful for quick `.solx` scripts, notes,
   JSON fixtures.
5. `TypeEditor` — lower priority: old sol's `TypeSchemaEditor` had a visual
   Fields-table ⇄ raw-JSON toggle for building a schema without hand-writing
   it. Current `TypeEditor` is JSON-only via `JsonSchemaEditor`. Nice-to-have.
6. Explicitly excluded: AI-assisted schema/description generation (old
   sol's `AiFieldHelper`) — solx-web has no LLM-helper wiring today and
   nobody's asked for one.

**ActionRunner — new dialog, design confirmed, no backend work needed.**
`action_start`/`action_stop`/`action_poll` and `console_read`/`console_tail`
are ordinary seeded Internal actions under `/builtin/action/*` and
`/builtin/console/*` (`solx-actions/src/seed.rs`), dispatched through the
exact same generic `POST /api/actions/:path/:name/exec` route every other
action already uses — and `solx-server/src/main.rs` already calls
`set_long_lived_host(true)` at startup, so the gate that would otherwise
block `action_start` is already satisfied. This is a frontend-only build.

Confirmed shapes (`solx-actions/src/invocations.rs` +
`internal/{invocation,console}.rs`):

- start: `execAction("/builtin/action", "start", { name, path, params })` →
  `Invocation` JSON (`invocation_id`, `status`, `result`, `error`,
  `console_seq_start`, …).
- poll: `execAction("/builtin/action", "poll", { invocation_id, wait_secs? })`
  → same shape; long-polls when `wait_secs` is given.
- stop: `execAction("/builtin/action", "stop", { invocation_id, force?,
  grace_secs? })` → same shape, moves to `cancelling`/terminal.
- console tail: `execAction("/builtin/console", "tail", { action_ref,
  cursor?, limit?, wait_secs? })` → `{ entries[], next_cursor, first_seq,
  dropped }`; long-polls when nothing new is buffered yet.

Component plan (new `web/src/components/ActionRunner.tsx`, opened from a
"Run" affordance on `ActionEditor`/`ActionList`):

- Params editor (JSON) + Run → calls `action/start`, stores `invocation_id`.
- Status line, driven by a long-poll loop against `action/poll` (not a
  fixed-interval timer — matches the primitive's own long-poll design).
- Console pane, driven by a parallel long-poll loop against `console/tail`,
  seeded from the `console_seq_start` the start call returned so it never
  replays a previous run's output.
- Cancel button (enabled while `running`) calling `action/stop`.
- Result/error panel once a terminal status arrives.
- Both poll loops stop on dialog close or terminal status — plain
  `useEffect` cleanup, no new infra.

**Progress (solx-web repo):**

1. **Done.** `ActionEditor` — typed Webhook headers/timeout + Command `cwd`
   fields overlaid onto `action_config`; `files: FileRef[]` wired through
   (`api.ts`'s `createAction` input type + the shared `FilesField`
   component); `bin_name` now has a datalist sourced from attached files.
2. **Done.** `ActionRunner` dialog (`web/src/components/ActionRunner.tsx`),
   opened from a new "Run with console…" button next to the existing
   synchronous "Run Action" in `ActionEditor`'s edit-mode Run panel. Calls
   `action/start`, then runs two independent long-poll loops (`action/poll`
   for status, `console/tail` for output, both `AbortController`-cancelled
   on dialog close/unmount) and a Cancel/Force-stop button wired to
   `action/stop`. New `api.ts` section (`startInvocation`/`pollInvocation`/
   `stopInvocation`/`tailConsole`/`isTerminalInvocationStatus`) added below
   `execAction`, calling the `/builtin/action/*` and `/builtin/console/*`
   routes directly (bypassing `execAction`'s fixed signature only to thread
   an `AbortSignal` through). Confirmed no backend changes were needed, as
   predicted above. Closing the dialog stops watching, not the invocation
   itself — it's detached, so it keeps running server-side regardless.

3. **Done.** `TypeSelector` combobox (`web/src/components/TypeSelector.tsx`)
   — searchable, keyboard-navigable, with inline "Edit"/"+ New" reusing the
   existing `TypeEditor` dialog. Wired into `ActionEditor`'s Parameter/Result
   Type fields in place of the old plain `<select>`. One deliberate behavior
   change from old sol's version: since a value here is a `path`+`name`
   pair rather than sol's bare name, typing only filters the dropdown —
   committing happens by picking an existing type or via "+ New" — rather
   than letting free-typed text commit directly (which old sol allowed,
   having no path to resolve).

   **Fixed in passing, applies beyond this port:** nesting `TypeEditor`
   inside `TypeSelector` inside `ActionEditor`'s own modal exposed a real
   CSS bug — `.dialog` centers itself via `transform: translate(-50%,
   -50%)`, and per the CSS Transforms spec a `transform` on an ancestor
   becomes the *containing block* for `position: fixed` descendants. A
   `TypeEditor` opened from inside another modal was therefore centering
   itself inside the parent dialog's box instead of the viewport. Fixed by
   portalling `TypeEditor` to `document.body` via `ReactDOM.createPortal`
   (`solx-web` had no portal usage anywhere before this). Only `TypeEditor`
   was changed — it's the only dialog currently nested inside another
   transformed dialog; apply the same fix to any other dialog component if
   it's ever nested the same way.

4. **Done.** `FileEditor` create mode now offers a "From disk" / "New text
   file" toggle. The new path (`web/src/components/FileEditor.tsx`) is a
   relPath field + language picker + Monaco editor (mirroring old sol's
   `TextArtifactDialog`), UTF‑8-encoded and sent through the same
   `uploadFile` call the disk-upload path already used — no API changes
   needed, since `solx-files` has no per-file content-type field to set;
   the language picker only drives Monaco's syntax highlighting while
   authoring.

**Remaining:** (5) `TypeSchemaEditor` visual mode — a Fields-table ⇄
raw-JSON toggle for `TypeEditor`'s schema, if still wanted; nice-to-have,
not blocking anything.

---

## 7. Pi extension — deferred

**Status: intentionally not started.** Revisit once points 1–4 and the
solx-web port (point 6) are further along — a Pi extension is more valuable
once there's an allowlist story to point a chat-driven harness at, and once
Resources exist for context-attachment. When it's time: default to an MCP
subprocess integration (Pi spawns `solx-mcp` over stdio like any other MCP
server) over a direct SDK embed, so the tool surface has one source of
truth (`solx-mcp`/`solx-types` schema generation) rather than two — revisit
only if MCP's tool-call shape turns out to be a poor fit for something Pi
specifically needs.
