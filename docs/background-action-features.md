# Background action features — logging, consoles, notifications, events

Status: **design discussion**. Nothing here is implemented. The audit in Part 1
is verified against the current working tree; everything after it is proposal.

The motivating question was "how do we add asynchronous actions so a caller can
poll a future ID through to completion". Part 2 argues that the valuable half
of that — *seeing what an action is doing while it does it* — is obtainable far
more cheaply with a console, and that true detached execution should be scoped
separately and later, because it solves a narrower problem than it first
appears to.

---

## Part 1 — What we do today (audit)

Short version: **almost nothing, and less than it looks.** Several layers
appear to log but their output is discarded before anyone can see it.

### 1.1 `tracing` is initialized in 2 of 4 binaries

| Binary | Subscriber | Effect |
|---|---|---|
| `solx-server` | `tracing_subscriber::fmt()` + `EnvFilter`, default writer → **stdout** (`main.rs:21`) | works |
| `solx-mcp` | same + explicit `.with_writer(stderr)`, `.with_ansi(false)` (`main.rs:20`) | works — stderr is required here, since stdout is the JSON-RPC channel |
| **`solx-cli`** | **none** — the crate does not even depend on `tracing` | every `tracing::*` call is silently dropped |
| `solx-client` | n/a (library) | — |

So the same action produces log output under `solx-server` and produces
*nothing* under `solx exec`. The CLI is the primary development surface, which
means the primary development surface is the one with no logging.

### 1.2 There is no file logging anywhere

`ConfigService::logs_dir()` exists (`solx-config/src/lib.rs:258`) and returns
`<appdata>/logs`. It is **never called** — a repo-wide search finds the
definition and no call sites. Dead code. Nothing in solx-core has ever written
a log file.

### 1.3 WASM guest logs go nowhere useful

The WIT world already gives every guest a `logger.log` import. The host
implementation was one line in `solx-actions/src/wasm/host.rs`
(historical snapshot below — since redirected to the console, see
`console-implementation-plan.md`):

```rust
impl sol::actions::logger::Host for HostState {
    async fn log(&mut self, message: String) {
        tracing::info!(target: "solx_wasm_guest", "{message}");
    }
}
```

Combined with 1.1: a guest's `log()` output is visible only under
`solx-server`/`solx-mcp`, with the right `RUST_LOG` filter, and is discarded
entirely under the CLI.

This matters immediately: **`solx-ollama` already calls `host.log()` on every
request** (`src/request.rs`, logging method, URL, and timeout). Those lines
exist today and are thrown away.

### 1.4 Command action stderr is discarded on success

`run_command` (`solx-actions/src/exec.rs:98-117`) uses `wait_with_output()` —
it collects stdout and stderr into buffers and only reads them after the child
exits. Then:

- **stdout** → parsed as the action result.
- **stderr** → used **only** in the error message when the exit status is
  non-zero (`exec.rs:109-116`). On success it is dropped on the floor.

So a Command action that runs for ten minutes and prints progress to stderr
shows the operator nothing while running, and nothing afterwards if it worked.

### 1.5 Package logging is inherited-env only, and effectively inert

Packages hand-rolled their own file logging against a `SOL_LOG_DIR` env var:

- `solx-omniparse/src/main.rs:43-90` — mirrors to stderr, appends to
  `$SOL_LOG_DIR/sol-omniparse.log` (note: still the old `sol-` filename).
- `solx-media/src/log_mirror.rs` + `config.rs:69` — same pattern,
  `log_dir: Option<PathBuf>`, `None` when the var is unset (no default).

The old system **set that variable for every command it spawned**:

```rust
// sol/sol-manager/src/dispatcher/commands.rs:49
cmd.env("SOL_LOG_DIR", &log_dir_str);
```

**solx-core's `run_command` sets no environment variables at all.** It sets
`current_dir` and nothing else. The child still inherits solx's own
environment, so `SOL_LOG_DIR` works *if an operator exported it before
launching solx* — but nothing in solx-core arranges that, and nothing
documents it. In practice both packages' file logging is dead, and their
`eprintln!` progress output falls into the hole from 1.4.

`solx-media` is worth noting separately: it also requires `SOLX_SERVER_URL`
and `SOLX_SERVER_TOKEN` from the environment (`config.rs:60-67`) and fails
hard without them, on the same inheritance-only basis.

### 1.6 Summary of the gap

| Layer | Emits? | Reaches an operator? |
|---|---|---|
| solx-core internals | barely (2 `tracing` calls outside setup) | only under server/mcp |
| WASM guests (`logger.log`) | yes, already wired | no, under CLI |
| Command packages (stderr) | yes (`eprintln!`) | **no, on success** |
| Command packages (file) | yes, gated on `SOL_LOG_DIR` | **no, var never set** |
| Script actions | no facility at all | — |

There is currently **no supported way to observe a running action.**

---

## Part 2 — The reframe: console, not futures

The original framing was: give each invocation a future ID, poll it to
completion. That is a real feature, but it conflates two things:

1. **Observability** — "what is this action doing right now?"
2. **Detachment** — "let this action outlive my connection."

(1) is where nearly all the value is, and it does **not** require asynchronous
actions. An action can stay a synchronous request/response call and still write
to a console as it runs, because the console is read by a *different* caller.
Terminal A blocks on `solx exec`; terminal B tails the console. The web UI
issues `POST /actions/exec` on one connection and polls the console on another.
The server is concurrent, so this simply works.

That is the user's own observation and it is the right one. It collapses a
large, invasive feature (execution lifecycle, future registry, reaping,
cancellation semantics, result storage) into a much smaller one (an append-only
log with a read API).

### What the console does *not* cover

Being honest about the boundary, because it is a real one:

- **The caller still blocks for the full duration.** A 40 GB `pull_model` still
  occupies the calling connection for an hour.
- **If the caller dies, the action dies.** Under the CLI, exiting kills
  everything. Under `solx-server`, axum drops the handler future on client
  disconnect, which cancels the action mid-flight.
- **No result retrieval after the fact.** The result exists only as the return
  value of the blocked call.

So detached execution remains a genuine, separate feature for the "start it and
come back tomorrow" case. The recommendation is to build the console first,
learn from it, and treat detachment as a later decision informed by whether
that case actually shows up in practice. Notably, the console makes detachment
*cheaper* later: a detached action's output has somewhere to go by construction.

---

## Part 3 — Console design

### 3.1 The key cost insight: console operations should be internal actions

This is the single most important design decision, because it determines
whether this feature costs one week or one month.

If the console gets a new manager trait (`ConsoleManager`) alongside
`TypeManager`/`DocManager`/`ActionManager`/`FileStore`, the cost is:

new trait in `solx-surface` → add to the `Solx` facade → local impl → remote
proxy in `solx-client` → routes in `solx-server` → commands in `solx-cli` →
tools in `solx-mcp` → Neon bindings in `solx-js` → TS wrapper in `solx-js`
packages. That is nine surfaces, and `solx-js` is the expensive one (its
`actions` binding is still a stub that throws —
`solx-js/crates/solx-bindings/src/actions.rs`).

If instead the console is exposed as **internal actions** under `/builtin`,
following the exact pattern of `internal/http.rs`, `internal/file.rs`,
`internal/secrets.rs`, then it inherits *every one of those surfaces for free*:

- CLI: `solx exec /builtin/console_tail --json '{...}'` — already works.
- REST: `POST /actions/exec` — already works, no new routes.
- MCP: every action is already surfaced as a tool — free.
- WASM guests: `action-exec` already reaches `/builtin/*` — **no WIT change,
  no guest rebuilds.**
- Scripts: `exec /builtin/console_print` — already works.
- solx-js: works the moment `actions.exec` is implemented; needs nothing
  console-specific.

Only a dedicated storage layer is genuinely new. Everything else is dispatch
table entries and seed rows. This is the same trick that made `http_request`
and `get_secret` cheap.

### 3.2 Storage

A purpose-built table is justified. The alternatives lose:

| Option | Why not |
|---|---|
| Documents under a reserved path | A document is one JSON blob; appending is read-modify-write — O(n) per line and racy under concurrent writers. Tantivy would index every console line. |
| File store append | `FileStore` has `put` (full overwrite), no append. Same read-modify-write problem. |
| Reuse the actions DB | Fine physically, but console writes are hot and unrelated to action CRUD; keep them separable. |

Proposed schema (sketch):

```sql
CREATE TABLE consoles (
  id          TEXT PRIMARY KEY,   -- invocation id
  parent_id   TEXT,               -- nesting; NULL for a root invocation
  action_ref  TEXT NOT NULL,
  created_at  TEXT NOT NULL,
  status      TEXT NOT NULL       -- running | ok | failed | timeout
);

CREATE TABLE console_entries (
  console_id  TEXT NOT NULL,
  seq         INTEGER NOT NULL,   -- monotonic per console; this is the cursor
  ts          TEXT NOT NULL,
  level       TEXT NOT NULL,      -- debug | info | warn | error | chunk
  message     TEXT,
  data        TEXT,               -- optional JSON payload
  PRIMARY KEY (console_id, seq)
);
```

`seq` doubles as the read cursor, which is what makes `tail` cheap and
replayable — the same property the `solx-ollama` streaming design already
argued for, and for the same reason (a `.solx` loop can thread a cursor back
without doing arithmetic; multiple readers can attach independently).

### 3.3 Operations

| Internal action | Params | Notes |
|---|---|---|
| `console_print` | `{level?, message, data?}` | writes to *the caller's own* console |
| `console_read` | `{console_id, from_seq?, limit?}` | range read |
| `console_tail` | `{console_id, cursor?, wait_secs?}` | `{entries, next_cursor, done}`; optional long-poll |
| `console_clear` | `{console_id, before_seq?}` | drop from the front |
| `console_list` | `{action_ref?, status?, limit?}` | find recent invocations |

`console_print` deliberately takes no `console_id`: an action writes to its own
console, resolved from the caller frame. Writing to *someone else's* console
should not be casually available.

### 3.4 Wiring existing emitters in

The high-value, low-cost moves, in order:

1. **Redirect `logger.log` → `console_print`.** ~10 lines in `wasm/host.rs`.
   Every existing guest immediately gains console output with **zero package
   changes and no rebuild** — including `solx-ollama`'s already-present log
   calls. This is the single best ratio in the whole design.
2. **Stream Command stderr → console.** Requires replacing
   `wait_with_output()` with a concurrent line-reader. This is the fiddliest
   piece: the current code's `feed`/`join!` structure exists specifically to
   avoid a pipe-buffer deadlock (`exec.rs:78-96`), and a naive rewrite will
   reintroduce it. Budget care here, not cleverness. Payoff: `solx-omniparse`
   and `solx-media` progress becomes visible with no package changes.
3. **Set `SOL_LOG_DIR`** (or a new `SOLX_LOG_DIR`) explicitly in
   `run_command`, restoring parity with old sol. One line, independent of the
   console, worth doing regardless.

### 3.5 Nesting

`Caller` is minted fresh per level in `exec_as` and deliberately drops the
incoming caller so an inner action cannot reach an outer action's secret keys
(`solx-actions/src/caller.rs`, `lib.rs`). A console id can be threaded through
*alongside* that without weakening the isolation — it is not a credential.

Recommendation: **store `parent_id` from day one** (one nullable column, near
zero cost) but ship only flat reads plus an optional
`include_descendants: true`. A full tree API — recursive walks, per-node
collapse, ordering across siblings — is speculative until there is a UI
consuming it. The column preserves the option; the API can wait.

### 3.6 Retention and write volume

Two things that will bite if ignored:

- **Unbounded growth.** Needs a cap (per-console entry/byte limit, oldest
  dropped, with a `dropped` counter so a late reader knows it missed data) plus
  a TTL sweep for consoles of completed invocations.
- **Write amplification on token streaming.** This deserves emphasis because
  streaming is the stated motivation: a chat response at ~50 tokens/sec writing
  one row per token is ~50 DB writes/sec per active stream, and the UI cannot
  usefully render at that granularity anyway. **Coalesce** — flush on an
  interval (~100-250 ms) or a character threshold, whichever first.
  Progress-style streaming (`pull_model` emitting every ~1 s) needs no such
  care. Design the `chunk` level with batching in mind from the start; retrofitting
  it after the fact means changing the read contract.

---

## Part 4 — Streaming use cases beyond updating the display

The user asked what else streaming buys. Ordered by how well they justify the
work on their own:

1. **Partial-result salvage on timeout.** Today a wasm action that hits its
   300 s ceiling returns `result: null` and *everything it computed is lost*.
   With a console, the work it reported survives the timeout. This is valuable
   independently of any UI and is arguably the strongest non-display argument.
2. **Post-mortem debugging / audit.** "What did this action actually do?" is
   currently unanswerable after the fact. A console is a durable trace.
3. **Long-running Command actions.** `solx-omniparse` OCR over a large PDF,
   `solx-media` whisper transcription — these already emit progress that is
   currently discarded (1.4/1.5).
4. **Multi-step orchestration visibility.** A `.solx` script or an instruction
   plan can report step-by-step progress, which is also how you debug a script
   that stalls on step 7 of 12.
5. **Cross-action observation.** An orchestrating action reads a child's
   console to make decisions — e.g. detect "downloading" vs "verifying" and
   adjust its own timeout. (The user raised this; it works naturally with
   cursors.)
6. **Test assertions.** Tests can assert on emitted console lines instead of
   mocking internals — cheaper and less brittle than the alternative.
7. **Incremental document authoring.** An LLM writing a long document streams
   into the console; a final step commits the assembled result as a document.
   The console is the transport, not the storage of record.
8. **Backpressure / early cancel.** Weaker: it needs a signal *back* to the
   producer, which the console does not provide. Listed only so it is not
   mistaken for something the console gives us for free.

---

## Part 5 — Cost vs value

### Cost (Phase 1, console only, as internal actions)

| Work | Rough size |
|---|---|
| Storage crate/module + schema + queries | ~400 lines |
| 5 internal actions + dispatch + seed rows + param schemas | ~250 lines |
| `console_id` threading through `Caller`/`exec_as` | ~50 lines |
| `logger.log` → console | ~10 lines |
| Command stderr streaming (the risky one) | ~80 lines, high care |
| Retention cap + TTL sweep | ~80 lines |
| Tests | ~300 lines |

**Estimate: 3–5 focused days.** Explicitly *not* included, and not needed:
new manager trait, `Solx` facade change, REST routes, MCP tools, WIT changes,
guest rebuilds, solx-js bindings.

### Value

**High, and higher than it looks**, because it is not a new capability so much
as switching on capability that already exists and is being discarded. Three
emitters (`logger.log`, package stderr, package file logs) are already writing
output that nothing can read. The console is mostly *plumbing that already-written
code is waiting for*.

It also fixes a genuine regression: package logging worked under old sol
(`SOL_LOG_DIR` was set for every spawned command) and silently stopped working
under solx-core.

### Verdict

**Worth it.** The ratio is good, the risk is contained to one function
(`run_command`), and nothing about it is speculative — every consumer already
exists. The parts to defer are the tree API (§3.5) and detached execution
(Part 2), both of which are speculative until a UI is actually consuming this.

The one thing to get right up front rather than retrofit is **chunk
coalescing** (§3.6), since it shapes the read contract.

---

## Part 6 — Future features

### 6.1 Notifications

A notification is a console entry that (a) escapes its invocation's lifetime
and (b) is addressed to a user rather than a caller. If the console lands
first, notifications are plausibly a thin layer: a `notify` level plus a
query that spans consoles, rather than a separate subsystem.

Worth *not* designing now — but worth keeping the console entry model general
enough (a `level` column and a `data` JSON payload) that it does not preclude
this. The sketch in §3.2 does not.

### 6.2 Action events

"Actions invoke other actions indirectly with a pre-set payload."

There is prior art: old sol had `EventHooksConfig` with `event_hooks_get` /
`event_hooks_set` on the manager trait (`sol/sol-surface/src/rpc.rs:221`), a
settings UI for editing hooks, and explicit routing through the manager in the
client/server split (`sol/docs/client-server-split.md:84-90`). **solx-core has
not replicated any of it** — a repo-wide search finds no event concept at all.

The interesting design question is whether events and the console are the same
mechanism seen from two sides: an event is a console write that something else
subscribes to. If so, building the console with that in mind (durable, ordered,
cursor-addressable, queryable across invocations) is most of the substrate.
That is an argument for the console-first sequencing, not for designing events
now.

The security question to answer *before* implementing events, given
`guard_executable_action` exists specifically to stop a guest from granting
itself shell/webhook access: an event hook is an indirection that could be used
to trigger a Command action that the triggering context could not invoke
directly. Hook registration needs the same guard, or events become a way around
it.

---

## Part 7 — Recommended sequencing

1. **Free wins, independent of everything else** (hours, not days):
   - initialize a `tracing` subscriber in `solx-cli`
   - set `SOL_LOG_DIR`/`SOLX_LOG_DIR` in `run_command`
   - either use `logs_dir()` or delete it
2. **Console Phase 1**: storage + 5 internal actions + `logger.log` redirect +
   retention. Do *not* touch `run_command` yet.
3. **Command stderr streaming.** Separate step, separate review — it is the
   only piece that can break existing behaviour.
4. **`solx-ollama` streaming variants** writing coalesced chunks to the
   console. This is where the original motivation gets paid off, and by then
   it is a package-level change with no core work left.
5. Reassess: tree API, notifications, events, detached execution — each only
   if a real consumer has appeared.

Steps 1–2 are worth doing on their own merits even if streaming never ships.

## Open questions

- **Console lifetime for the CLI.** Under `solx exec`, the process exits when
  the action returns. Is the console still readable afterwards (persisted), or
  is CLI console output only useful to a concurrent observer? Persisted is more
  useful (post-mortem, §4.2) and is the assumption above — worth confirming
  it is wanted, since it implies retention policy matters from day one.
- **Should the CLI render its own console live?** `solx exec` could spawn a
  poller and print entries as they arrive, which would make streaming visible
  in the terminal rather than only to a second observer. Cheap to add, changes
  `exec`'s output contract (currently exactly one JSON blob on stdout) — so
  probably an opt-in flag rather than the default.
- **Where does the storage live** — a new `solx-console` crate, or a module in
  `solx-actions` (which already owns invocation lifecycle and `Caller`)? The
  latter is less ceremony; the former is cleaner if consoles ever outgrow
  actions (notifications would push that way).
