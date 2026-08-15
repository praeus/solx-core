# Action consoles — implementation plan

Companion to [background-action-features.md](background-action-features.md),
which holds the audit of what logging reaches an operator today (short answer:
almost nothing) and the cost/value argument. **This document is the "how".**

Status: **Done — core and package-side both.** All ten core items in §5, plus
the shared crate and every package migration listed in §8/§8a's tail. Nothing
in this plan is outstanding; §11's "still open" items are the only remaining
loose threads, and none of them block anything.

---

## 1. Settled decisions

| Decision | Choice |
|---|---|
| Console identity | **The action ref.** A console belongs to an action, and every layer of that action logs to it. |
| Action surface | `/builtin/console/*` — begin subdividing the builtin namespace |
| CLI persistence | Consoles **survive process exit**; retention/rollover is therefore in scope from day one |
| `solx exec` rendering | Renders live **by default**, with a flag to suppress |
| Console operations | Internal actions, not a new manager trait (see §5 of the companion doc) |
| Caller-supplied console names | **Future.** Not phase 1. |
| WebSocket push to web UI | **Future.** Not phase 1. |

## 2. Console identity — and the one gap in it

### The model

Console name = action ref. `/packages/solx-ollama/ollama-chat` has one console;
every part of that action's execution writes to it.

This is a genuine simplification and it is the right call. `Caller` already
carries the action ref (`caller.action_ref()`), and `exec_as` already mints a
fresh `Caller` per nesting level — so **nothing new has to be threaded through
the execution path at all.** `console/print` resolves its target from the
caller frame that is already there. That removes the largest piece of work the
earlier sketch implied.

It also makes the name human-friendly for free, which is what the `names` crate
was going to buy.

### The gap: concurrent invocations interleave

If two `ollama-chat` calls run at once, both write to the same console and
their lines interleave with nothing to separate them. For status messages
that is untidy; for streamed chat chunks it is **actively wrong** — you would
get two responses shuffled together and no way to unpick them.

### Recommended fix, which preserves the simplicity

Keep console identity = action ref, and **stamp each entry with an
`invocation_id`** minted in `exec_as` at the moment the action starts.

- Nothing is propagated — the id is generated where the entry is written, and
  written into a column. Console identity stays derived from the caller frame.
- Concurrent runs stay separable: `console/read` and `console/tail` take an
  optional `invocation_id` filter.
- Default read behaviour can stay "everything, newest last", which is what you
  want for an action that is only ever run one at a time.

This is one extra column and one `Uuid::new_v4()`, and it is the difference
between the console being usable under concurrency and not.

**As implemented:** `invocation_id` is stamped on every entry, but `read`/
`tail` do **not** take an `invocation_id` filter param — reads return
everything, in order, unfiltered, by explicit decision (a client that needs
to separate two concurrent runs can still do it itself using each entry's own
`invocation_id` field; nothing forces that filtering to live server-side).
`§4`'s param table below reflects this.

### Deferred: cross-console run grouping

"Show me everything that happened under this top-level invocation, across all
the child actions it called" needs a `run_id` inherited down the nesting chain
— the *only* thing in this design that requires touching `Caller`. It is a
non-credential field so it does not weaken the secret isolation `Caller`
exists to enforce, but it is genuinely optional.

**Recommendation: add the `run_id` column in phase 1, leave it NULL, and wire
the propagation later.** Adding a column later is a migration; leaving one
unused is free.

## 3. Schema

```sql
CREATE TABLE consoles (
  action_ref   TEXT PRIMARY KEY,   -- the console name, e.g. /packages/solx-ollama/ollama-chat
  created_at   TEXT NOT NULL,
  last_write   TEXT NOT NULL,      -- drives TTL sweep
  next_seq     INTEGER NOT NULL,   -- monotonic allocator
  first_seq    INTEGER NOT NULL,   -- oldest retained; > 0 means entries were dropped
  dropped      INTEGER NOT NULL    -- count evicted by the cap, so readers can detect gaps
);

CREATE TABLE console_entries (
  action_ref    TEXT NOT NULL,
  seq           INTEGER NOT NULL,  -- per-console monotonic; this is the read cursor
  ts            TEXT NOT NULL,
  level         TEXT NOT NULL,     -- debug|info|warn|error|chunk
  invocation_id TEXT NOT NULL,
  run_id        TEXT,              -- reserved; NULL in phase 1
  source        TEXT NOT NULL,     -- guest|command|webhook|script|host
  message       TEXT,
  data          TEXT,              -- optional JSON
  PRIMARY KEY (action_ref, seq)
);

CREATE INDEX idx_entries_invocation ON console_entries(action_ref, invocation_id, seq);
```

`seq` doubles as the read cursor. That is what makes `tail` cheap, replayable,
and safe for multiple concurrent readers — the same property the
[solx-ollama streaming design](../../solx-packages/solx-ollama/docs/streaming-design.md)
argued for, and it is what a future WebSocket push would sit on.

`source` is worth having from the start: it is how you tell a guest's
deliberate `log()` from scraped Command stderr, which matters for filtering
noise in a UI.

## 4. Action surface

`/builtin/console/*`. Verified safe: `split_ref` uses `rfind('/')`, so
`/builtin/console/print` → `("/builtin/console", "print")`, and MCP's
`encode_tool_name` handles arbitrary depth (`act__builtin__console__print`,
reversibly). No changes needed in either.

| Action | Params | Returns |
|---|---|---|
| `/builtin/console/print` | `{level?, message?, data?}` | `{seq}` |
| `/builtin/console/read` | `{action_ref, from_seq?, limit?}` | `{entries, next_cursor, first_seq, dropped}` |
| `/builtin/console/tail` | `{action_ref, cursor?, limit?, wait_secs?}` | same shape, `wait_secs` long-polls (clamped to 60s) |
| `/builtin/console/clear` | `{action_ref, before_seq?}` | `{removed}` |
| `/builtin/console/list` | `{prefix?, limit?}` | `{consoles}` — each with `action_ref, created_at, last_write, entry_count, dropped, next_seq` |

`print` deliberately takes **no** `action_ref` — an action writes to its own
console, resolved from the caller frame. Writing to someone else's console is
not casually available. (Reading someone else's *is* allowed, which is what
enables the "orchestrator watches its child" case.)

### Required core change: `SeedAction` must split name from fn_name

This is the one thing the nested path breaks, and it is easy to miss.

Today `SeedAction` has a single `name`, seeded under a hardcoded
`BUILTIN_PATH`, and the SQL binds **the same `?3` to both `name` and
`fn_name`** (`solx-actions/src/seed.rs`). `run_internal` then dispatches on a
flat `fn_name` match.

With nesting, the entity name becomes `print` — which is far too generic to be
a globally unique internal dispatch key. So:

```rust
pub struct SeedAction {
    pub path: &'static str,        // NEW: "/builtin" or "/builtin/console"
    pub name: &'static str,        // "print"
    pub fn_name: &'static str,     // "console_print"  <- the run_internal key
    pub description: &'static str,
    pub param_type: Option<&'static str>,
}
```

and the seed SQL binds `name` and `fn_name` separately. Existing builtins keep
`path = "/builtin"` and `fn_name == name`, so nothing else moves.

## 5. Core changes, file by file

| # | Change | Where | Status |
|---|---|---|---|
| 1 | Storage: schema, insert, cursor read, cap eviction, TTL sweep | `solx-actions/src/console/mod.rs` (single module, ~450 lines incl. tests) | **Done.** `print` is a single atomic `INSERT ... ON CONFLICT DO UPDATE ... RETURNING` that both upserts the console row and claims the next `seq` in one round trip — no separate read-then-write race between concurrent printers. |
| 2 | `SeedAction` gains `path` + `fn_name`; SQL binds separately | `solx-actions/src/seed.rs` | **Done.** |
| 3 | 5 internal actions + dispatch arms | `solx-actions/src/internal/console.rs`, `internal/mod.rs` | **Done.** |
| 4 | Param schemas for the 5 actions | `solx-types/src/seed.rs` | **Done.** |
| 5 | Mint `invocation_id` per exec | `solx-actions/src/caller.rs` (`Caller::from_action`) | **Done** — minted inside `Caller::from_action` itself rather than threaded from `exec_as`, since that's the only place a `Caller` is constructed (the `Wasm` and `Script` arms), so nothing else needed to change. |
| 6 | `logger.log` → `console/print` | `solx-actions/src/wasm/host.rs` | **Done** — calls `ConsoleStore::print` directly (via `LocalActionManager::console()`), not through a synthetic `action-exec` round trip; see the resolved open item below. |
| 7 | Give Command actions console access | `solx-actions/src/loopback/console.rs` (new), `exec.rs` (`run_command`) | **Done — but not as stderr capture.** Rebuilt as a loopback HTTP listener the child POSTs to directly; see §8a, which documents why and supersedes this row's original plan. |
| 8 | Log webhook request/response | `solx-actions/src/exec.rs` (`run_webhook`) | **Done** — logs method+URL at start and status/duration (or the error) at end, deliberately never body/headers/resolved auth. `run_webhook_inner` holds the original logic unchanged; the public `run_webhook` is a thin logging wrapper around it. |
| 9 | Log each script stage | `solx-actions/src/script.rs` (`ActionCommandRunner`) | **Done** — logs a stage summary (`exec <ref>`, params excluded) before each stage runs and a warning on failure. Uses the caller's existing `action_ref`/`invocation_id` directly — no new identity to mint, unlike items 7/8. |
| 10 | Live console rendering | `solx-cli/src/main.rs` | **Done** — `--no-console` flag, rendering to stderr, `console/list`'s `next_seq` used to skip prior history so a repeat run doesn't replay old lines. Verified live against a real `solx-ollama` call. |

### Why #7 was originally flagged risky, and why it was rebuilt instead

`run_command` uses `wait_with_output()`, which buffers both pipes and reads
them only after the child exits. Streaming stderr concurrently would have
meant reworking that — and the existing `feed`/`tokio::join!` structure
(`exec.rs`) exists *specifically* to avoid a pipe-buffer deadlock: if you
write stdin up front while the child is blocked writing stdout, neither side
moves. A naive rewrite would have reintroduced that deadlock, only showing up
on payloads over ~64 KB — the reason this row was marked HIGH risk.

That risk is why item 7 was rebuilt as the §8a loopback instead: it doesn't
touch `wait_with_output()` or the stdin/stdout pipes at all, so the deadlock
risk this section originally warned about doesn't apply to what actually got
built. `run_command`'s only change is two extra `.env(...)` calls before
`spawn()`.

## 6. Retention, rollover, and the logging-library question

> *Is there a good standard library for logging like this?*

Two different concerns, and only one of them has a library answer.

### For file-based `tracing` logs — yes: `tracing-appender`

Confirmed against current docs. `RollingFileAppender::builder()` supports
`.rotation()` (`NEVER` / `HOURLY` / `DAILY` / `MINUTELY`), `.filename_prefix()`,
`.filename_suffix()`, `.max_log_files(n)` ("keeps the last n log files on
disk", deleting oldest), and `.latest_symlink()`. It also provides a
`non_blocking` writer. It is the same ecosystem as the `tracing-subscriber`
already used by `solx-server` and `solx-mcp`, so it drops straight in.

That is the right tool for the Part-1 "free wins" — giving the CLI a subscriber
and writing rolled files under the long-dead `ConfigService::logs_dir()`.

### For the console — no, and that is fine

The console is **not a log file**. It is a queryable, cursor-addressable store
that must support:

- reads by cursor while writes are still happening
- concurrent readers from other processes
- filtering by invocation and by level
- being read *by another action*
- retention by entry count, not by file age

A file-rotation crate gives none of that. Rotating files would actively fight
the cursor model, since `seq` has to stay monotonic across a rollover.

The equivalent of "rollover" here is **ring-buffer eviction on a table**, which
is genuinely small:

- On insert, if `next_seq - first_seq > MAX_ENTRIES`, delete the oldest batch,
  advance `first_seq`, add to `dropped`.
- Evict in batches (e.g. 10% of the cap) rather than one row per insert —
  per-row deletes on every write are a needless cost.
- A periodic sweep drops whole consoles whose `last_write` is older than a TTL.
- Readers see `first_seq` and `dropped`, so a slow reader can detect it missed
  data rather than silently skipping it.

**Done.** Caps live in `SolxConfig` as `console_max_entries` (default 5000)
and `console_ttl_days` (default 7), read live on every write/sweep rather than
snapshotted once — a config edit takes effect without a restart. The TTL sweep
runs once at `LocalActionManager::open()` (best-effort; a sweep failure logs a
warning rather than blocking startup) — there is no background scheduler yet,
so a long-lived process only sweeps at boot.

### Chunk write volume

Restating from the companion doc because it shapes this schema: a chat stream
at ~50 tokens/sec writing one row per token is ~50 inserts/sec per stream, and
no UI can render that granularity anyway. **Coalesce `chunk`-level writes** on
an interval (~100–250 ms) or a character threshold. Progress-style streams
(`pull_model`, ~1/sec) need no such handling. Design this in now — it is a
read-contract change if retrofitted.

## 7. `solx exec` live rendering

Decision: render live by default, allow pure JSON.

**Render console lines to stderr, not stdout.** This resolves the tension
completely rather than trading it off:

- `stdout` stays exactly one JSON blob — so `solx exec ... | jq` keeps working
  unchanged, *even with rendering on*.
- Live output goes to stderr, where a human sees it and a pipe ignores it.
- `--no-console` suppresses it entirely for anyone who wants silence.

Because stdout is never polluted, defaulting rendering **on** is safe — which
is what was asked for, without breaking the existing output contract.

**Done, as implemented:** `exec` first does one O(1) `console/list` lookup
(filtered to the exact `action_ref`) to read `next_seq` — the console's
current tip — and starts tailing from there, not from 0. Skipping straight to
the tip (rather than replaying full history) is what makes rendering usable
for an action that's been run many times before; without it, every invocation
of a chatty action would dump its entire past console to the terminal. The
tail runs as a spawned task racing `tokio::select!` against a `Notify` the
main task fires when `exec` returns; on stop it does one final non-blocking
`console/read` from the last-seen cursor so nothing printed right before
return is lost to the tail loop's own poll cadence. This is also the piece
that makes the console useful to a single-terminal user rather than only to a
concurrent observer.

## 8. Where console printing gets added, per package

45 actions across 8 packages. What each gets, and what it costs:

| Package | Actions | Type | Coverage |
|---|---|---|---|
| solx-google | 15 | webhook | needs core change #8 |
| solx-ollama | 13 | wasm | **free** via #6; already calls `host.log()` at `request.rs:84` |
| solx-media | 5 | command | needs migration to the shared crate (§8a) — see caveat below |
| solx-livejournal | 3 | wasm | needs `log()` calls added — **currently calls the logger nowhere** |
| solx-firefox | 2+2 | command + script | command needs the shared crate; script needs #9 |
| solx-mcp-actions | 2 | command | needs migration to the shared crate |
| solx-omniparse | 1 | command | needs migration to the shared crate |
| solx-quickjs | 1 | command | needs the shared crate added (nothing to migrate — it emits nothing today) |

**Correction from an earlier draft of this section, worth being explicit
about:** this table originally said Command packages would get console
output "free" once item 7 landed, on the assumption item 7 would be passive
stderr capture — core reading whatever a package already prints, no
package-side change required. §8a's loopback is a *push* mechanism instead:
a package has to actively call it. **Nothing is free for Command packages
anymore** — every one of them needs the small change of swapping its
existing `eprintln!`/`log_line()` calls for the shared crate. The `SOL_LOG_DIR`
file-logging fix (§9/Phase 0, done) genuinely *is* free — that one's passive,
core-side only — but console visibility specifically requires touching each
package.

Both flagged risks below were resolved by the work in this document, not just
identified — see the migration table further down for what actually happened
to each:

- **solx-google** needed core work, not a package-side crate — item 8
  (`run_webhook` logging) covers all 15 actions with zero package changes.
  Done.
- **solx-livejournal's 3 wasm actions never called the logger** — the WIT
  import was available but unused. Fixed directly in the JS guest.

`solx-ollama`'s own remaining item — coalesced `chunk` writes for streamed
`generate`/`chat`, and progress for `pull_model` — is tracked separately in
its own `docs/streaming-design.md`, since it depends on that package's
Phase 2 streaming design, not on anything in this document.

## 8a. Package logging today, and standardizing it

Audited every package in `solx-packages` directly against source (not
inferred from `install.solx`). Two findings drive everything below:

1. **Only `solx-ollama` reaches the console today, and only by accident** —
   it's WASM-typed and already called `host.log()` before the console existed
   for unrelated reasons. As of items 7–9 landing, Webhook and Command
   actions get console access at the core level automatically; every other
   package listed below still needs a package-side change to actually use
   the mechanism (Script actions get it from the `.solx` interpreter itself,
   also automatic — but none of the audited packages here use Script for
   more than a couple of thin actions).
2. **The packages that already emit output each reinvented the same
   eprintln!+file pattern independently, and inconsistently even within
   themselves.**

### Per-package audit

| Package | Action type(s) | Current mechanism | File logging | Reachable by an operator today |
|---|---|---|---|---|
| solx-omniparse | command ×2 | `eprintln!` + `log_line()` (`main.rs:43-92`), ~15 call sites across the whole pipeline | `$SOL_LOG_DIR/sol-omniparse.log` — note: still the pre-rename `sol-` filename | **No** — `run_command` never sets `SOL_LOG_DIR`; stderr discarded on success regardless |
| solx-media | command ×7 | Same pattern, explicitly copied from omniparse (`log_mirror.rs:1`: *"Copy of solx-omniparse's pattern"*) | `$SOL_LOG_DIR/solx-media.log` — but only for call sites routed through `log_mirror::log_line`; `audio.rs`/`video.rs`/`materialize.rs` mostly call raw `eprintln!`, which skips the file even when the var *is* set | No, same reason. Its whisper model download loop (`whisper_models.rs:240-247`) writes multi-hundred-MB files with **zero log calls at all** |
| solx-mcp-actions | command ×2 | `eprintln!`-only `log_line()` (`main.rs:88-94`), timestamp-prefixed | None | No |
| solx-quickjs | command ×1 | Nothing during the build; errors are `anyhow`'s Debug dump | None | N/A — nothing meaningful emitted |
| solx-firefox | command ×2, script ×2 | Nothing | None | N/A |
| solx-google | webhook ×15, script ×1 | Nothing — `run_webhook` has no logging calls | None | **Yes, now** — item 8 landed; every webhook call is logged with zero package-side work needed |
| solx-livejournal | wasm ×3 | Nothing — the WIT `logger` import is available but never called | None | N/A |
| solx-ollama | wasm ×13 | One call site (`request.rs:84-87`) via the WIT `logger` import | None | **Yes**, today, incidentally |

No package anywhere emits a percentage, a phase name, or a structured status
line — everything that exists is free text, with the occasional
`key=value` fragment (`mode=`, `dry_run=`, `ocr_status=`).

**This table is a snapshot from when the audit was written.** Two things it
describes as broken are now fixed core-side: `SOL_LOG_DIR` is set by
`run_command` (§9/Phase 0), and `run_webhook` logs on its own (item 8, just
above) — solx-google needs no package-side change at all. The "Reachable by
an operator today" column has been updated to reflect that; the rest of the
table (what each package's own code currently does) is left as originally
audited.

### How a package reaches the console: the loopback, as built

Two options were weighed here. The first — a package calling
`console/print` over HTTP straight to `solx-server` — doesn't actually work:
`print` resolves its target only from `ctx.caller` (§4), and an HTTP request
has none, so it hits the same "no action caller" refusal `get_secret` already
gives the CLI/MCP. Fixing that would mean a second, explicit-`action_ref`
entry point gated on the server's bearer token instead of a `Caller` — and
even then, `solx-server` isn't running by default (the CLI's default mode is
fully local, no server at all), so that path would leave every Command
package except `solx-media` (the one that already requires a server) with no
console access in the common case.

The second — `run_command` reading the child's stderr and capturing it — is
what item 7 originally planned, and §5 explains why it was shelved: it means
concurrently reading stderr while feeding stdin, which is exactly the shape
that deadlocks on payloads over ~64 KB unless done very carefully.

**What got built instead: a scoped loopback, not the real server.** Same
shape as `crate::loopback::oauth` (bind `127.0.0.1`, mint a random token,
tear it down when done), but started once and kept alive for the process's
lifetime rather than per sign-in — see `solx-actions/src/loopback/console.rs`.
`run_command` registers a one-shot token for `(console, action_ref,
invocation_id)` before spawning, hands the child `SOLX_CONSOLE_URL` /
`SOLX_CONSOLE_TOKEN` as env vars, and the `Registration` deregisters the
token automatically when it drops — which happens on every exit path in
`run_command` (success, failure, timeout, even a spawn failure), not just the
success path, because it's ordinary Rust scope-drop rather than something
that has to be remembered at each return site.

This resolves both problems at once: no pipe-reading, no deadlock risk
(`run_command`'s only change is two `.env(...)` calls before `spawn()`), and
no dependency on a running `solx-server` — it works in the default local-only
deployment because the listener is started by whatever process is already
running the action, CLI included.

**One real gap worth naming, not a limitation of the design:** a
`loopback::console`-registered token is scoped to its own invocation's
console only (by construction — the registry entry *is* the scoping, there's
no explicit-`action_ref` field a caller could override). A package wanting to
write into *another* action's console (cross-action attribution) isn't
possible over this channel. Nothing here forecloses adding that later; it
just isn't needed by anything today.

**Verified end to end**, not just unit-tested: a real spawned PowerShell
child read `SOLX_CONSOLE_URL`/`SOLX_CONSOLE_TOKEN`, POSTed
`{"level":"warn","message":"...","data":{"n":42}}` with a Bearer token, and
the entry showed up — correctly attributed (`source: "command"`, the right
`invocation_id`) — both in `solx exec`'s live stderr rendering and via
`console/read` afterward. 8 unit tests cover token issuance, auth rejection
(missing/unknown token), cross-registration isolation, and deregistration on
drop; all pass reliably under the full workspace suite (confirmed the failure
mode a naive `tokio::spawn` would have hit under `#[tokio::test]`'s per-test
runtime — the server now runs on its own dedicated OS thread with its own
runtime specifically so "process lifetime" is actually true rather than
incidental to whichever runtime happened to start it first).

### The shared crate gets simpler because of this

No stderr-format design is needed — no NDJSON, no line-parsing, no
raw-line fallback for un-migrated packages. A package just needs an HTTP
POST helper reading two env vars:

```rust
pub fn info(message: &str);
pub fn warn(message: &str);
pub fn error(message: &str);
pub fn with_data(level: Level, message: &str, data: Value);
```

Each does a best-effort `POST $SOLX_CONSOLE_URL` with
`Authorization: Bearer $SOLX_CONSOLE_TOKEN` and the `{level, message, data}`
body the loopback already expects — no new format to define, since the wire
shape is just `console/print`'s own params. If the env vars are absent (the
binary run by hand, outside solx-core, exactly `solx-omniparse`'s existing
fallback for `SOL_LOG_DIR`), the crate no-ops on the console side and falls
back to stderr/file only.

The crate mirrors to `$SOL_LOG_DIR/<package>.log` too (§9/Phase 0's env var,
**done** — `run_command` now sets it on every spawned child, pointed at
`logs_dir()/<slug of the action ref>/`, so `ConfigService::logs_dir()` is no
longer dead code). One implementation, one env var for file logging, one for
console access, instead of the current three independent (and in media's
case, internally inconsistent) versions of the same idea.

### Per-package migration — done

| Package | What happened | Status |
|---|---|---|
| solx-package-log | New crate (`solx-packages/solx-package-log`): stderr + `$SOL_LOG_DIR/<package>.log` + best-effort POST to `$SOLX_CONSOLE_URL`/`$SOLX_CONSOLE_TOKEN`, one call site (`info`/`warn`/`error`/`with_data`). Async core, plus a `blocking` module (own reusable current-thread runtime) for callers with no ambient tokio runtime. 6 tests, including a real capture-server round trip and the sync/no-runtime case. | **Done.** |
| solx-omniparse | `log_line()`/`LOG_FILE`/`package_log_path()` deleted; ~25 call sites swapped 1:1, with real level differentiation added (`warn`/`error` for the fail-open and hard-failure paths that were previously indistinguishable from routine progress). | **Done.** |
| solx-media | Same swap across `main.rs`/`audio.rs`/`video.rs`/`materialize.rs`, *including* the raw `eprintln!` call sites that used to bypass `log_mirror` entirely — those are now on equal footing with everything else. `MediaConfig.log_dir` removed (dead — the crate reads `SOL_LOG_DIR` itself). Whisper model download loop, previously silent, now logs start/throttled progress every ~5 MiB/pct-of-total/final size, plus the hash-mismatch and already-verified fast-path cases. | **Done.** |
| solx-mcp-actions | `log_line()` swap, 8 call sites, `fatal:` promoted to `error`. | **Done.** |
| solx-quickjs | Had zero logging. Added `init()` + start (entry/sources) + success (output size) around the one `componentize()` call. | **Done.** |
| solx-firefox | Had zero logging *and* no tokio runtime — the one package using `solx_package_log::blocking`. Added logging at every state transition: already-running (own vs. external), spawn, started/timeout, stopped, fatal. | **Done.** |
| solx-livejournal | JS guest, not a Rust Command package — `import { log } from "sol:actions/logger@0.1.0"}` added directly (the WIT import was already available, just never called). Logs page/entry progress in `harvest`/`harvest_page`, and start/save in `extract_entry` — the resumable, hour-long batch action is the one that most needed this. | **Done.** |
| solx-google | No package-side work — item 8 (`run_webhook` logging) covers all 15 actions core-side. | N/A, already covered. |

Verified: `cargo build`/`cargo test` clean across all six Rust packages (solx-package-log 6/6, solx-media 18/18, solx-mcp-actions 3/3, solx-quickjs 1/1; omniparse/firefox have no unit tests but build clean), `node --check` on the livejournal guest's JS, and one live end-to-end run of a real package through `solx exec` — installed `solx-quickjs` into a scratch appdata, ran `build-javascript-action` against `sample-document-summary.js`, and confirmed all three delivery paths independently: the `[INFO]` lines rendered live to stderr during the run, `console/read` returned both entries with `source: "command"` over the loopback, and `$SOL_LOG_DIR/solx-quickjs.log` held the same two lines on disk. Not just the crate's own tests.

## 9. Phasing

**Phase 0 — free wins, independent of consoles** (hours) — **done**
- `tracing` subscriber in `solx-cli`, writing to stderr (stdout stays the
  single JSON result). Verified: invisible without `RUST_LOG`, visible with
  it, doesn't interfere with `--no-console`.
- set `SOL_LOG_DIR` in `run_command` (restores old-sol parity, and per §8a is
  what makes every package's *existing* file-logging code start working
  with zero package-side changes) — kept the old name rather than
  introducing `SOLX_LOG_DIR`, resolving that open item; `logs_dir()` now has
  a real call site. Verified live: a spawned Command action's own
  `%SOL_LOG_DIR%` resolved to `<appdata>/logs/<slug>`.
- `tracing-appender` rolling files under `logs_dir()` — **not done, and not
  needed for this phase.** That would route solx-core's *own* `tracing::*`
  calls to a file; the stderr subscriber above already covers the "primary
  dev surface has no logging" gap this item existed for, and package file
  logging is handled by `SOL_LOG_DIR` above instead. Revisit only if
  solx-core's own logs need to persist past a terminal scrolling away.

**Phase 1 — console core** (items 1–6, 10) — **done, verified end to end**
Storage, seed split, 5 internal actions, schemas, `invocation_id`,
`logger.log` redirect, CLI rendering. Deliberately excluded #7.
Verified live: installed `solx-ollama` into a scratch appdata against a real
Ollama server; `solx exec /packages/solx-ollama/ollama-version` rendered its
`host.log()` line to stderr while running, the entry was readable via
`console/read` from a fresh process after exit (persistence), `--no-console`
suppressed rendering without suppressing the write, and a second run did not
replay the first run's history.

**Phase 2 — give every action type a path to the console** (items 7, 8, 9) —
**done, verified end to end**
- **Item 7.** Not stderr capture — rebuilt as the `loopback::console` listener
  (§8a). `run_command`'s only change was two `.env(...)` calls, so the
  deadlock risk that made this HIGH-risk in the original plan doesn't apply
  to what got built. 8 unit tests plus a live end-to-end run (a real spawned
  PowerShell child posting to the loopback, attributed and rendered
  correctly).
- **Item 8.** `run_webhook` logs method+URL at start, status/duration or the
  error at end — never body, headers, or resolved auth (`console/read` is
  unrestricted by design, so a nested reader must never see a secret this
  way). 2 tests: success against a real local HTTP server, failure against a
  closed port.
- **Item 9.** Each script stage logs a summary (`exec <ref>`, params
  excluded) before it runs, plus a warning on failure. Reuses the caller's
  existing `action_ref`/`invocation_id` — no new identity to mint, unlike 7/8.
  2 tests: multi-stage happy path, a failing stage.

Unlocks the remaining 16 of the 45 actions once the packages are migrated:
solx-google's 15 webhook actions (needs no package-side work at all — it's a
Webhook-typed package, so item 8 alone covers it), plus script stages in
solx-firefox and solx-google.

**Phase 3 — package work** — **done.** The shared `solx-package-log` crate,
migrated omniparse/media/mcp-actions, added logging to quickjs/firefox
(neither had any), and livejournal's JS guest (`logger.log` calls, no core
change needed — the WIT import was already there). See §8a's migration table
for what happened to each. Not done: solx-ollama's coalesced streaming — that
one depends on its own `docs/streaming-design.md`, tracked separately.

## 10. Future improvements

Explicitly out of scope, recorded so the phase-1 design does not preclude them:

- **Caller-supplied console names.** Pass an existing console into an action so
  several actions share one stream. This is where the
  [`names`](https://docs.rs/names) crate fits — v0.14.0, `Generator`
  implementing `Iterator`, `Name::Plain` ("rusty-nail") or `Name::Numbered`
  ("pushy-pencil-5602"), and custom adjective/noun dictionaries. Likely a
  small `/builtin/names/*` or a `names` package so any action can mint one.
  (Note it depends on `rand ^0.8`; check for a duplicate-`rand` pull-in.)
  Phase-1 impact: `console_entries.action_ref` should be understood as
  "console name that happens to default to the action ref", so widening it
  later is not a migration.
- **Cross-console run grouping** via inherited `run_id` (§2). Column reserved.
- **WebSocket push to the web UI.** `solx-server` already uses axum, which has
  WS support. The cursor model is what makes this straightforward: a socket is
  just a `tail` that pushes instead of being polled. Nothing in phase 1 should
  assume polling is the only reader.
- **Notifications** — a console entry that outlives its invocation and is
  addressed to a user. The `level` + `data` columns leave room.
- **Action events** — indirect invocation with a preset payload. Prior art in
  old sol (`EventHooksConfig`, `sol/sol-surface/src/rpc.rs:221`), not
  replicated in solx-core. Security note: an event hook is an indirection
  around `guard_executable_action`, so hook registration needs the same guard
  or events become a way to trigger Command actions a guest could not invoke
  directly.
- **Detached execution / future IDs** — the original framing. The console
  covers observability; it does not let an action outlive its caller (axum
  drops the handler future on client disconnect). Revisit only if that case
  actually appears.

## 11. Open items

Resolved during implementation:

- ~~Crate placement~~ — **`solx-actions/src/console/` (a module, not a
  separate crate).** Matches the phase-1 leaning; extractable later if
  notifications push consoles beyond actions.
- ~~Which DB file~~ — **the actions DB**, per explicit decision. `ConsoleStore`
  only ever touches its own `Db` handle, so retargeting it to a separate file
  later is a one-line change at construction; the real cost of that move
  would be copying existing rows across (or accepting the retention policy
  discarding old history on the switch), not a code change.
- ~~Does `console/print` need a caller at all?~~ — **Yes, always, for the
  action-facing `/builtin/console/print`**; there is no explicit-`action_ref`
  variant of that action. Every host-side writer that has no `Caller`
  (`wasm/host.rs`'s redirect, and now `loopback::console`'s `print_handler`)
  calls `ConsoleStore::print` directly on an already-held store handle,
  bypassing the `action-exec`/HTTP-action round trip entirely — exactly the
  pattern this bullet predicted for item 7 before it was built. Items 8 and 9
  will do the same: `run_webhook`/the script interpreter each already know
  their own `action_ref` and can call `ConsoleStore::print` directly.
- ~~`SOL_LOG_DIR` vs `SOLX_LOG_DIR`~~ — **Kept `SOL_LOG_DIR`**, implemented in
  `run_command`. Preserves `solx-omniparse`/`solx-media`'s existing reads
  with zero package changes, which was the deciding factor.
- ~~The stderr JSON-line format~~ — **moot.** That format existed to give a
  future stderr-capturing item 7 something structured to parse; item 7 was
  rebuilt as the `loopback::console` HTTP listener instead (§8a), which
  receives real JSON directly — there's no line-oriented text to parse or
  specify a format for.

Still open:

- **Level filtering on read** — worth having, or is `source` +
  client-side filtering enough?
- **The shared package-logging crate's name and location** — `solx-package-log`
  was a placeholder; needs a real name and a decision on whether it lives in
  `solx-packages` (alongside the packages that depend on it) or `solx-core`
  (alongside `loopback::console`, whose wire contract it's coupled to).
