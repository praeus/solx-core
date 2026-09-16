# Asynchronous actions — implementation plan

Companion to [background-action-features.md](background-action-features.md),
which framed the original question ("how do we add asynchronous actions so a
caller can poll a future ID through to completion"), argued that the valuable
half of it was observability, and deferred the other half. This document takes
up that other half — **detachment** — now that the console exists.
[console-implementation-plan.md](console-implementation-plan.md) §10 lists it
as the deferred item; this is its "how".

Status: **Done.** Everything in §1–§8 is implemented and tested — the
`invocations` table/store, `exec_as_with`, `start`/`stop`/`poll_invocation`,
both cancellation paths (loopback `/cancelled` and `action-cancelled`),
`solx-package-log::cancelled()`, the long-lived-host gate, and the four seeded
`/builtin/action/*` actions. §11's list is the only remaining loose thread,
none of it blocking.

One caveat surfaced during implementation, not anticipated above:
**force-abort is process-local.** `cancel_requested` lives in the shared
`invocations` table, so cooperative cancellation reaches the running task
regardless of which process calls `stop`. The `AbortHandle` that force-abort
uses, though, exists only in the memory of whichever process's `start`
actually spawned the task. The documented usage pattern — a CLI proxied to
`solx-server` via `server_url`, so `stop`/`poll` execute inside the same
process that ran `start` — is unaffected; two independent
`LocalActionManager`s (e.g. a CLI run locally *without* `server_url`,
sharing an appdata dir with a separately-running `solx-server`) sharing one
database file are not. Documented inline on
`LocalActionManager::stop_invocation`.

---

## 0. Why now, and why it is cheap

The reframe in the companion doc was right: the console delivered the
observability half without any of the lifecycle machinery a future registry
implies. What was not obvious at the time is how much of the *remaining* half
the console incidentally built.

Four things detachment needs already exist and need no design work:

| Need | Already have |
|---|---|
| A per-run identity | `invocation_id`, minted per invocation, stamped on every console entry (`solx-actions/src/caller.rs:42`) |
| Somewhere for a detached run's output to go | The console, by construction — the companion doc predicted this at `:151-153` |
| A channel a spawned child already talks to | `crate::loopback::console` — bound to `127.0.0.1`, one-shot token per invocation, RAII deregistration |
| A client library every Command package links | `solx-package-log`, which already reads `SOLX_CONSOLE_TOKEN` |

What the console does **not** provide is the thing this plan adds: invocation
*state*. `consoles` is keyed by `action_ref`, and `invocation_id` is only a
column on `console_entries` — so there is nowhere to hang a status or a cancel
flag. The implemented schema deliberately dropped the `status` column that the
companion doc's sketch had (`background-action-features.md:204-210`), because
phase 1 had no use for it.

So: one new table, one new store beside `ConsoleStore`, and everything else is
reuse.

The companion doc also called out, at `:317-319`, the one thing the console
explicitly does not give us:

> **Backpressure / early cancel.** Weaker: it needs a signal *back* to the
> producer, which the console does not provide.

That signal is what §4 adds, and it travels the same two paths the console's
writes already travel — the loopback for Commands, internal-action dispatch for
everything else.

---

## 1. Settled decisions

| Decision | Choice |
|---|---|
| Action surface | `/builtin/action/*` — internal actions, not a new manager trait. Same reasoning as the console (companion doc §3.1) |
| Host lifetime | **Long-lived hosts only** — `solx-server`, `solx-mcp`. `solx exec` refuses, loudly |
| `stop` semantics | Cooperative flag first, force-abort after a grace period |
| `poll` payload | Status and result/error only. Output stays with `console-read`/`console-tail` |
| Result persistence | Yes — `ActionExecResult`'s value is stored on the invocation row. This is new; `exec` has never persisted a result |
| Survive process restart | **No.** An interrupted run is marked `interrupted`, not relaunched |
| `run_id` propagation | **Future.** Still `NULL`, as in phase 1 |

### On the host-lifetime decision

Worth being explicit, because it is the sharpest limitation.

A `tokio::spawn`'d task is not cancelled when an axum handler future drops —
which is exactly why detachment works under `solx-server` and survives client
disconnect. But under `solx exec` the whole process exits the moment the
command returns, so a spawned task dies instantly. `start` would appear to
succeed and the invocation would be dead before the caller could poll it.

Rather than have `start` mean different things on different hosts, it refuses
unless the host has declared itself long-lived (§5). A CLI user gets an error
naming the actual fix — point the CLI at a running server, or use `exec`.

---

## 2. Invocation state

New module `solx-actions/src/invocations.rs`, a sibling of
`solx-actions/src/console/mod.rs` and modelled directly on it: same `Db` handle
(the same file as `actions` and `consoles`), same `ensure_schema` /
`sweep_expired` shape, same `''`-sentinel-plus-`crate::opt` convention for
nullable text.

```sql
CREATE TABLE IF NOT EXISTS invocations (
    invocation_id     TEXT PRIMARY KEY,
    action_ref        TEXT NOT NULL,
    status            TEXT NOT NULL,
    cancel_requested  INTEGER NOT NULL DEFAULT 0,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL,
    finished_at       TEXT NOT NULL DEFAULT '',
    console_seq_start INTEGER NOT NULL DEFAULT 0,
    result            TEXT NOT NULL DEFAULT '',
    error             TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_invocations_action
    ON invocations(action_ref, created_at DESC);
```

`status` is one of `running | cancelling | ok | failed | cancelled | timeout |
interrupted`.

`console_seq_start` is captured at start from
`ConsoleStore::list(Some(action_ref), 1).next_seq`. That field exists for
precisely this purpose — `console/mod.rs:115-117` describes it as letting a
caller "start a `tail` from 'now' without an O(history) read just to find the
current tip". Returning it from `start` and `poll` is what lets `poll` stay
lean without making the client hunt for its own run's output.

`InvocationStore` API, mirroring `ConsoleStore`'s: `new`, `ensure_schema`,
`create`, `get`, `is_cancelled`, `request_cancel`, `finish`, `list`,
`sweep_expired`, and `mark_orphans`.

`mark_orphans` is the one with no console analogue: a crashed or killed process
leaves rows stuck at `running` forever, so a startup pass flips them to
`interrupted`. Wire it and `sweep_expired` next to the existing console sweep at
`solx-actions/src/lib.rs:111-116`, best-effort with a `tracing::warn!` on
failure — a sweep must not block startup.

---

## 3. The one refactor: hoist `invocation_id` into `exec_as`

Today the id is minted independently in three places:

- `solx-actions/src/exec.rs:93` — Command
- `solx-actions/src/exec.rs:186` — Webhook
- `solx-actions/src/caller.rs:64` — `Caller::from_action`, for Wasm and Script

That was fine when the id only ever had to reach the console writer next to it.
A detached run needs *one* id, known before execution begins, so `stop` and
`poll` have something to address.

Add `exec_as_with(path, name, params, caller, invocation_id: Option<&str>)`
beside `exec_as` (`solx-actions/src/lib.rs:526`), with `exec_as` delegating as
`None`. **Every existing call site is untouched.** Inside, mint once if not
supplied, then:

- **Command** — pass `&invocation_id` into `run_command`; delete the mint at
  `exec.rs:93`
- **Webhook** — same into `run_webhook`; delete the mint at `exec.rs:186`
- **Wasm / Script** — `Caller::with_invocation(&action_ref, cfg, &id)`, a new
  constructor beside `from_action`, which keeps minting for everyone else. The
  fresh-frame semantics are unchanged: the incoming caller is still dropped,
  not forwarded, so a guest still cannot reach an outer action's secret keys
- **Internal** — `InternalCtx` gains `invocations: Arc<InvocationStore>` and a
  `Weak<LocalActionManager>` for the start/stop handlers

Roughly 40 lines across four files, each diff a few lines. This is the only
structural change to the existing execution path.

A side benefit worth noting: it also makes the id available to return from a
plain `exec`, which would let a synchronous caller read back exactly its own
run's console lines. Not in scope here, but no longer blocked.

---

## 4. Runner

`self_arc()` (`solx-actions/src/lib.rs:148`) already yields an
`Arc<LocalActionManager>` suitable to move into a spawned task. That is the
piece that makes detachment cheap.

### `start(path, name, params) -> invocation_id`

1. Refuse unless the host is long-lived (§5)
2. `get_unmasked` to resolve `action_ref` and fail fast on a missing or
   typeless action — the caller gets a real error rather than a status row that
   immediately goes to `failed`
3. Mint the id, capture `console_seq_start`, `invocations.create(..)`
4. `tokio::spawn` on `self_arc()`, calling `exec_as_with(.., Some(&id))`, then
   `finish(..)` with the outcome
5. Record the task's `AbortHandle` in a new
   `running: Arc<Mutex<HashMap<String, AbortHandle>>>` field on the manager

The abort registry is a **field, not a process global**, so tests stay
isolated. The loopback needs a global for a specific reason — see the comment
at `loopback/console.rs:130-140` about `#[tokio::test]` runtimes — and that
reason does not apply here.

**Timeout.** A detached run of a multi-hour job must not silently inherit
`DEFAULT_TIMEOUT_SECS` (300). Honor `action_config.timeout_secs` when set;
otherwise use a new `background_timeout_secs` config default (~24 h) in place
of the synchronous default.

### `stop(invocation_id, force?, grace_secs?)`

Returns immediately with `{status: "cancelling"}`. It does **not** block for the
grace period.

Sets `cancel_requested`, then spawns a watcher that waits `grace_secs` (config
default ~10 s) and, if the row is still non-terminal, calls `abort()`. Dropping
the future is what reaps the child, via the existing `.kill_on_drop(true)` at
`exec.rs:114`, and what unwinds a WASM fiber — `wasm/actions.rs:62-81` already
documents that cancellation propagates into the guest and that the guest does
not keep running in the background.

`force: true` skips the wait. A terminal row is a no-op returning current
status.

> **Carried-over caveat, unchanged by this work.** Killing a Command kills the
> *shell* we spawned, not its descendants (`exec.rs:104-113`). Cooperative exit
> is the reliable path; force is the backstop, not a guarantee.

### `poll(invocation_id, wait_secs?)`

Returns `{invocation_id, action_ref, status, result?, error?, created_at,
updated_at, finished_at?, console_seq_start}`.

Optional long-poll until terminal, reusing the 250 ms poll interval and 60 s
clamp already defined at `console/mod.rs:76-79` rather than introducing a
second set of constants.

---

## 5. The cancellation signal

Two paths, both riding channels the console already established.

### 5a. Command children — one more loopback route

Extend `solx-actions/src/loopback/console.rs` rather than standing up a second
listener. `Target` (line 55) gains `invocations: Option<Arc<InvocationStore>>`;
`register` takes it, and passes `None` for a plain synchronous `exec`, whose
runs are simply never cancellable.

```
GET /cancelled    Authorization: Bearer <token>    ->  {"cancelled": bool}
```

Same bearer lookup, same one-token-resolves-to-one-invocation property, same
RAII `Drop` deregistration — a token cannot be replayed after its invocation
ends, and cannot report on any invocation but its own.

Add `Registration::control_url()` returning `http://127.0.0.1:{port}/cancelled`
and set `SOLX_CONTROL_URL` alongside the two existing env vars at
`exec.rs:115-117`. A second variable, deliberately, rather than having the
child string-munge `/print` into `/cancelled` — the URL contract stays explicit
on the host side.

### 5b. Wasm, Script, Internal — one more internal action

`action-cancelled`, no params, caller-scoped exactly as `console-print` is
(`internal/console.rs:18-27`): it resolves `ctx.caller.invocation_id()` and
hard-errors with the same message shape when there is no caller. Returns
`{"cancelled": bool}`.

**No WIT change and no guest rebuilds.** Guests already reach `/builtin/*`
through `action-exec`; `.solx` scripts reach it through `exec`. This is the
same "best ratio in the whole design" property the companion doc identified for
the `logger.log` redirect (`:246-249`).

### 5c. The shared package

`solx-packages/solx-package-log/src/lib.rs` gains, beside `info` / `warn` /
`error` / `with_data`:

```rust
pub async fn cancelled() -> bool
pub mod blocking { pub fn cancelled() -> bool }
```

Reads `SOLX_CONTROL_URL` and the existing `SOLX_CONSOLE_TOKEN`, GET with bearer
auth, 2 s timeout.

**Fails closed to `false`.** A dead or slow loopback must never spuriously
abort real work — the inverse of `post_to_console`'s contract, where a failure
costs a log line, but the same "swallow everything" discipline. Add a short TTL
cache (~500 ms, `OnceLock<Mutex<(Instant, bool)>>`) so a tight progress loop
does not hammer the listener.

Existing consumers — `solx-firefox`, `solx-mcp-actions`, `solx-media`,
`solx-omniparse`, `solx-quickjs` — need no change until they opt in.

> This sharpens a loose thread already noted in
> `console-implementation-plan.md` §11: the name `solx-package-log` is a
> placeholder, and after this it is plainly the host-integration crate rather
> than the logging crate. A rename (`solx-package-host`?) is reasonable, but it
> is a separate mechanical change across five packages and is **not** in scope
> here.

---

## 6. Long-lived-host gate

A `static LONG_LIVED: AtomicBool` in `solx-actions` with a public
`set_long_lived_host(bool)`, called once in each of `solx-server/src/main.rs`
and `solx-mcp/src/main.rs`.

`action-start` refuses otherwise, with a message naming the fix rather than
just the symptom:

```
action-start requires a long-lived host (solx-server or solx-mcp).
This process exits when exec returns, which would kill the invocation
before it could be polled. Point the CLI at a running server, or use exec.
```

An `AtomicBool` rather than plumbing a flag through `LocalActionManager::open`
because the fact is about the *process*, not about any one manager instance,
and every other construction site (tests included) would otherwise have to
carry it.

---

## 7. Action surface

New `solx-actions/src/internal/invocation.rs`, dispatched from the table at
`internal/mod.rs:97-168`:

```rust
"action-start"     => invocation::start(params, ctx).await,
"action-stop"      => invocation::stop(params, ctx).await,
"action-poll"      => invocation::poll(params, ctx).await,
"action-cancelled" => invocation::cancelled(ctx.caller.as_ref(), &ctx.invocations).await,
```

Seed rows in `solx-actions/src/seed.rs` under a new
`ACTION_PATH = "/builtin/action"`, via the existing `a_at(path, name, fn_name,
..)` helper — entity names `start` / `stop` / `poll` / `cancelled`, dispatch
keys as above. This is the second `/builtin/<area>/*` subdivision, after
`/builtin/console`. Param schemas beside the console ones at
`solx-types/src/seed.rs:509-580`.

Everything downstream is free, exactly as it was for the console: CLI
(`solx exec /builtin/action/start --json '{...}'`), REST (`POST /actions/exec`),
MCP (every action is already a tool), WASM guests, `.solx` scripts.

### Access model

Deliberately mirrors the console's.

**`start` grants no new capability.** A guest can already `exec` a Command
action through `action-exec`; `guard_executable_action` guards *creating and
modifying* executable actions, not invoking them. So `start` needs no new
guard — it is `exec` with a different return shape.

**`stop` and `poll` take an arbitrary `invocation_id` and are unrestricted**,
the same call that makes `console-read`/`console-tail` unrestricted so an
orchestrator can watch a child (`internal/console.rs:5-9`).

One obligation is inherited and worth restating, because nothing enforces it
mechanically: the `result` and `error` columns now persist an action's output
where an unrestricted `poll` can read it. That is the same exposure that makes
"never log bodies, headers, or resolved auth" load-bearing in `run_webhook`
(`exec.rs:171-174`) and `stage_summary` (`script.rs:36-39`) rather than
stylistic.

---

## 8. Config

Three keys in `solx-config/src/lib.rs` and `types.rs`, following the
`console_max_entries` / `console_ttl_days` pattern — read live per use, not
snapshotted, so an edit takes effect without a restart:

| Key | Default | Purpose |
|---|---|---|
| `background_timeout_secs` | 86400 | Ceiling for a detached run with no explicit `timeout_secs` |
| `stop_grace_secs` | 10 | Cooperative window before `stop` force-aborts |
| `invocation_ttl_days` | 7 | Retention for terminal invocation rows |

---

## 9. Files touched

| File | Change |
|---|---|
| `solx-actions/src/invocations.rs` | **new** — store, schema, sweep, orphan marking |
| `solx-actions/src/internal/invocation.rs` | **new** — the four handlers |
| `solx-actions/src/lib.rs` | `exec_as_with`; `start`/`stop`/`poll`; abort registry; store construction and startup sweep |
| `solx-actions/src/exec.rs` | accept `invocation_id`; set `SOLX_CONTROL_URL`; background timeout |
| `solx-actions/src/caller.rs` | `with_invocation` constructor |
| `solx-actions/src/loopback/console.rs` | `GET /cancelled`; `Target.invocations`; `control_url()` |
| `solx-actions/src/internal/mod.rs` | four dispatch arms; two `InternalCtx` fields |
| `solx-actions/src/seed.rs`, `solx-types/src/seed.rs` | seed rows and param schemas |
| `solx-config/src/lib.rs`, `solx-config/src/types.rs` | three keys |
| `solx-server/src/main.rs`, `solx-mcp/src/main.rs` | `set_long_lived_host(true)` |
| `solx-packages/solx-package-log/src/lib.rs` | `cancelled()` and `blocking::cancelled()` |

**Roughly 1000 lines including tests — 2–4 focused days.**

Explicitly *not* needed, the same list the console avoided: no new manager
trait, no `Solx` facade change, no REST routes, no MCP tools, no WIT change, no
guest rebuilds, no solx-js bindings.

---

## 10. Verification

### Unit

Mirror `console/mod.rs`'s test style — tempdir, `Db::open`,
`ConfigService::open_in`.

- **`invocations.rs`** — create, then `is_cancelled` false, `request_cancel`,
  true; `finish` is terminal and idempotent; `mark_orphans` flips `running` to
  `interrupted` and leaves terminal rows alone; TTL sweep drops only stale rows
- **`loopback/console.rs`** — `GET /cancelled` reflects the flag; missing and
  unknown tokens are 401; a registration built with `invocations: None` reports
  `false`; a dropped registration's token stops working (extend the existing
  test)
- **`internal/invocation.rs`** — `action-cancelled` errors with no caller;
  `poll` of an unknown id is `NotFound`; `start` errors when the host is not
  long-lived
- **`solx-package-log`** — `cancelled()` returns `false` against a dead URL
  without hanging (mirror `a_dead_console_url_does_not_hang_or_panic`); the
  cache suppresses a repeat request inside the TTL

### End-to-end, cooperative

With `solx-server` running:

1. Register a Command action that loops, calling `solx_package_log::cancelled()`
   each iteration and logging progress
2. `POST /actions/exec` → `/builtin/action/start`; note `invocation_id` and
   `console_seq_start`
3. `console-read` from `console_seq_start` shows progress accruing while the
   HTTP request that started it has long since returned — this is the actual
   detachment assertion
4. `/builtin/action/stop` → `poll` shows `cancelling`, then `cancelled` within a
   second or two, and the console's last line is the action's own clean-exit
   message, not a kill

### End-to-end, force

Same, with an action that never polls: `stop` returns immediately, the row goes
terminal after `stop_grace_secs`, and the shell process is gone.

### WASM path

A guest calling `/builtin/action/cancelled` in a loop, started detached, stops
the same way — with no rebuild and no WIT change, which is the claim in §5b.

### Regression

`cargo test -p solx-actions` — the `exec_as` delegation must leave every
existing test green — plus `solx-mcp/tests/mcp_integration.rs` for the new
tools appearing and round-tripping.

---

## 11. Deliberately out of scope

- **Relaunching interrupted invocations on startup.** Genuinely detached
  execution across restarts needs idempotency, re-entrancy, and orphan
  detection. `mark_orphans` records the fact; acting on it is a separate
  decision.
- **`run_id` propagation.** Still `NULL`. Grouping a whole nested call tree
  under one root remains what `console-implementation-plan.md` §2 reserved it
  for.
- **A `start`/`stop`/`poll` trait on `ActionManager`.** Promoting these from
  internal actions to first-class trait methods costs the nine surfaces the
  companion doc enumerates at §3.1. Revisit only if a consumer appears that the
  internal-action form genuinely cannot serve.
- **CLI sugar** (`solx start` / `solx poll` / `solx stop`). `solx exec
  /builtin/action/start --json '{...}'` works from day one; a nicer spelling is
  cosmetic.
- **Renaming `solx-package-log`.** See §5c.
- **Push instead of poll.** The companion doc's WebSocket note
  (`console-implementation-plan.md` §10) applies unchanged: a socket is a
  `tail`/`poll` that pushes. Nothing here forecloses it.
