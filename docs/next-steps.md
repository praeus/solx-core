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

## 3. solx-js as a direct solx-server client (solx-js / solx-web)

**Status: not started, design agreed.** Today solx-web's Bun backend loads
`solx-js`'s native NAPI binding (`solx.node`, built from `solx-bindings`)
*and* the README says it expects a running `solx-server` — two hops for
what's conceptually one call.

**Agreed direction:** implement the `Solx` manager-trait interfaces
**directly in TypeScript** as an HTTP client against `solx-server` — the
same interface shape, just a TS-language implementation of the wire
contract that `solx-client`'s `Remote*` impls already speak in Rust. Keep
the NAPI/neon binding path as a separate, optional embed mode for a TS tool
that wants solx-core in-process with no server running at all.

This directly serves the packaged-single-app goal from point 7: the app can
choose in-process embed (NAPI) when it's the only consumer, or point at a
shared `solx-server` when other clients (Pi, CLI, MCP) need the same appdata
concurrently — same TS-level interface either way, caller doesn't care which
backend it got. It also removes the double-hop in solx-web specifically,
since its Bun server can just pick the HTTP-client implementation.

**Next action:** define the shared TS interface once (mirroring
`solx-surface`'s manager traits — `TypeManager`/`FileStore`/`DocManager`/
`ActionManager`/the `Solx` facade) in `solx-js/packages/surface` or
equivalent, then add an `Http*` implementation beside whatever the
NAPI-backed implementation is called today, selected by config exactly like
`solx-manager` does in Rust (local vs. `server_url`).

---

## 4. MCP Resources & Prompts (solx-core) — next after #1

**Status: still open**, unchanged from `design-and-progress.md` §10 item 5.
Worth scoping once the allowlist lands. Resources exposing
`solx://doc/{path}/{name}`-style URIs are also a good fit for Pi's
context-attachment model (point 7) and for the path-scoping idea in point 5
— reasonable to design these two together.

---

## 5. Do paths mitigate the "too many MCP tools" risk?

Partially, not fully, as things stand today:

- **Helps:** paths give a real, already-existing grouping axis. A client
  that only cares about `/builtin/*` plus its own team's `/research/ai/*`
  actions *could* filter the tool list by path prefix before presenting it
  to a model. That lever doesn't exist in a flat-namespace tool design.
- **Doesn't help yet:** `solx-mcp/src/tools.rs` flattens every `(path,
  name)` into one opaque tool name unconditionally — there's no MCP-level
  mechanism today to expose only a path subtree as a toolset, or for a
  client to request a prefix-scoped `tools/list`. The full catalogue is
  always the full catalogue, regardless of how the actions are namespaced
  underneath.

So: the data model supports the fix, the MCP surface doesn't use it yet.
This is a cheaper lever than a permissions system for the tool-bloat problem
specifically, and worth building once the catalogue is large enough to
matter — likely bundled with point 4, since Resources could be path-scoped
the same way.

**Next action:** none yet — flagged as a concrete, low-effort feature for
when the registry grows past comfortable single-list size.

---

## 6. Finish the solx-web port: Actions / Types / Files(-or-Artifacts) + a new ActionRunner

**Status: Documents tab only; Actions/Types/Files are placeholders.**

Old `sol` precedent to review in depth before porting (located, not yet
read in full):

- `sol-browser/src/entityEditors/actionEditorService.ts` — old action editor
  logic.
- `sol-browser/src/components/TypeEditor.tsx`, `TypeSchemaEditor.tsx`,
  `TypeSelector.tsx` — old type-editing UI.
- `sol-browser/src/components/ArtifactSelector.tsx`,
  `DataArtifactDialog.tsx`, `SharedArtifactDialog.tsx`,
  `TextArtifactDialog.tsx` — old file handling. Old sol called files
  "Artifacts," with an `ArtifactScope` (`Shared`/`Legacy`/etc.,
  `sol-manager/src/managers/artifact.rs`) and a `promote_to_shared`
  operation. solx-files' simpler always-shared-if-orphaned convention looks
  like a direct, simplified descendant of this — worth confirming there's no
  scenario `promote_to_shared` handled that solx-files' model doesn't cover,
  before assuming the port is a straight simplification.

**New work, not a port:** an **ActionRunner** view — run an action, show
request/response, live console output, and cancellation. The backend side
of all three already exists end-to-end and needs no new solx-core work
unless the UI surfaces a gap:

- Console output → `console_tail`/`console_read` (`solx-actions/src/console`).
- Cancellation → `action_start`/`action_stop`/`action_poll`
  (`docs/async-actions-plan.md`), already gated to long-lived hosts
  (`solx-server`, `solx-mcp` — solx-web's backend talks to `solx-server`, so
  this is satisfied).

**Next action:** read the seven old-sol files above in full plus solx-web's
current `ActionEditor.tsx`/`FileEditor.tsx`/`TypeEditor.tsx` stubs, then
propose a per-tab port plan and a design sketch for ActionRunner.

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
