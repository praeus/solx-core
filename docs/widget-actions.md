# Widget Actions

A **widget** is a WASM action that also has a UI. The WASM component is the
action's brain (it runs server-side under wasmtime, exactly like any other
`custom-action` guest); the UI is a JS/ESM bundle rendered in the host
frontend (today: solx-web). The two are connected by a **loopback** — a
`127.0.0.1`-only listener that serves the bundle, hosts a websocket, and holds
the widget's **fields** (a JSON object whose values typically correspond to
the fields in the widget's UI).

This is a **revision** of the earlier plan in this file's history. That plan
made a widget a *new `ActionType`* and a new `solx-widgets` crate. That
direction is dropped: a widget is not a new action type, it is a WASM action
that happens to open a UI. The `solx-widgets` crate is dropped too — the
backend half fits cleanly into the existing `solx-actions` crate (a new
loopback module + a new WIT interface + new internal actions), and the
actual UI half is entirely out of scope here (it lands in `solx-web`,
decoupled).

Read end-to-end before touching code.

---

## 1. Goals and non-goals

**Goals**

- A new `widget` **WIT interface** in the existing `custom-action` world,
  giving a WASM guest `open` / `close` / `show` / `hide` / `get` / `set` /
  `exec` over a widget.
- A new **loopback module** (`solx-actions/src/loopback/widget.rs`), modeled
  on the existing `console` loopback, that:
  - serves the widget's JS bundle,
  - hosts a **websocket** the widget's frontend connects to,
  - holds the widget's **fields** (the single source of truth),
  - relays messages between the WASM host and the widget.
- `open` creates the loopback registration and returns a **widget
  descriptor** the caller surfaces to the frontend so it can load the bundle
  and connect the websocket.
- A set of **internal actions** (`/builtin/widget/*`) mirroring each widget
  operation, so a `.solx` script or any other action can drive a widget via
  `action-exec` without being a WASM guest.
- Widget state is **ephemeral** — no database entries. Keyed off a fresh
  `widget_id` minted per `open` (so one invocation can open more than one
  widget); `invocation_id` is retained only for attribution.
- A widget's **lifetime is manual**: `open` returns the descriptor and the
  opening invocation then returns, while the widget keeps running until an
  explicit `close`.

**Non-goals (this round)**

- The widget's own adapter for making solx API calls. The widget is a **black
  box**: "a thing that runs in a browser, listens to websockets, and makes
  API calls." Its outbound solx operations are out of scope; we only build
  the descriptor + loopback + message relay + field state.
- The `solx-web` frontend half (loading the bundle, mounting the custom
  element, connecting the websocket). Decoupled; a follow-up.
- Iframe isolation (decided when the frontend is implemented), package
  signing (all packages are trusted today), and removing the inert `trusted`
  flag (a separate cleanup, not widget work).
- Auto-rebuild of widget bundles on upload. The `install.solx` convention is
  explicit, as before.

---

## 2. Architecture at a glance

```
                        ┌──────────────────────────────────────────────┐
                        │  solx-wasm/wit/custom-action.wit             │
                        │  • interface widget (open/close/show/hide/    │
                        │    get/set/exec)                              │
                        │  • world custom-action { import widget; ... } │
                        └──────────────┬───────────────────────────────┘
                                       │ bindgen! (host) / wit_bindgen (guest)
        ┌──────────────────────────────┴───────────────────────────────┐
        │                                                                │
        ▼                                                                ▼
┌───────────────────────────────┐                        ┌──────────────────────────────┐
│  solx-actions/src/wasm/host.rs│                        │  solx-actions/src/internal/   │
│  impl widget::Host for HostState│                       │  widget.rs (widget_open/close/ │
│  → calls loopback::widget      │                        │  show/hide/get/set/exec)       │
└───────────────┬───────────────┘                        └───────────────┬──────────────┘
                │                                                        │
                │  both converge on the same loopback registry           │
                └───────────────────────────┬────────────────────────────┘
                                            ▼
                        ┌──────────────────────────────────────────────┐
                        │  solx-actions/src/loopback/widget.rs         │
                        │  • process-lifetime 127.0.0.1 listener        │
                        │  • registry: widget_id → WidgetState          │
                        │    (fields, visible, bundle, tag_name, token, │
                        │     ws sender, action_ref, invocation_id)     │
                        │  • GET /bundle  → JS bytes                    │
                        │  • GET /ws      → websocket upgrade           │
                        └───────────────┬──────────────────────────────┘
                                        │  ws://127.0.0.1:{port}/ws?token=…
                                        ▼
                        ┌──────────────────────────────────────────────┐
                        │  solx-web (out of scope this round)           │
                        │  • loads bundle, mounts custom element        │
                        │  • connects websocket, relays get/set/show/   │
                        │    hide/exec messages                         │
                        └──────────────────────────────────────────────┘
```

The key insight is unchanged from the old plan but relocated: the seam is no
longer a `WidgetHost` trait in `solx-surface`. It is the **loopback registry**
inside `solx-actions` itself. Both the WASM host and the internal actions are
already in `solx-actions`, so there is no cross-crate trait to define — the
loopback is just another module, like `console` and `oauth`.

---

## 3. The WIT interface — `solx-wasm/wit/custom-action.wit`

Add one interface to the existing world. No new world, no new crate. The
`custom-action` world already imports `action-exec`, `artifact-read`, and
`logger`; `widget` joins them.

```wit
/// Host interface for driving a client-side widget from a WASM guest.
///
/// A widget is a WASM action that also has a UI. The guest opens a widget
/// (standing up a loopback the frontend connects to), then drives it —
/// show/hide, read/write its fields, dispatch events — through this
/// interface. The widget's own frontend code talks back over the same
/// loopback (a websocket) and, separately, makes its own solx API calls
/// through a host-injected adapter (out of scope; the widget is a black box).
interface widget {
  /// Open a widget. `spec` is a JSON object:
  ///   { "bin_name": "<bundle artifact>", "tag_name": "<custom-element tag>",
  ///     "fields": { ... } }
  /// Returns a JSON-encoded widget descriptor the caller surfaces to the
  /// frontend so it can load the bundle and connect the websocket.
  open: func(spec: string) -> result<string, string>;

  /// Close a widget and tear down its loopback registration + websocket.
  close: func(widget-id: string) -> result<_, string>;

  show: func(widget-id: string) -> result<_, string>;
  hide: func(widget-id: string) -> result<_, string>;

  /// Read one field (JSON-encoded) or, if `field` is empty, the whole
  /// fields object.
  get: func(widget-id: string, field: string) -> result<string, string>;

  /// Set one field to a JSON-encoded value.
  set: func(widget-id: string, field: string, value: string) -> result<_, string>;

  /// Dispatch an event to the widget's frontend code.
  exec: func(widget-id: string, event: string, payload: string) -> result<_, string>;
}
```

And the world gains one line:

```wit
world custom-action {
  import action-exec;
  import artifact-read;
  import logger;
  import widget;   // NEW
  export runner;
}
```

**Backward compatibility.** A guest built against the *old* world (three
imports, no `widget`) still instantiates against a host that provides four
imports — the component model resolves only the imports the guest declares.
The package-local copies of `custom-action.wit` (`solx-ollama`,
`solx-google`) can stay as-is; they simply don't get widget access until
their WIT copy is refreshed. The host's `bindgen!` in
`solx-actions/src/wasm/host.rs` picks up the new interface automatically
because it binds `world: "custom-action"` by path.

The `solx-wasm` helper crate (`solx-wasm/src/lib.rs`) re-exports the guest
bindings; add `pub use sol::actions::widget::*;` (or the individual
functions) so widget authors can call them.

---

## 4. The widget loopback — `solx-actions/src/loopback/widget.rs`

Modeled directly on `loopback/console.rs` (process-lifetime listener, one-shot
token per registration, deregister-on-drop) and `internal/http_stream.rs`
(the in-memory registry with a `OnceLock`). The one structural addition over
`console` is the **websocket**: the loopback must hold a live connection to
the widget so host-side operations can push messages to it.

### 4.1 Lifecycle

- **One listener, process-lifetime**, bound to `127.0.0.1:0` (random port),
  on its own dedicated OS thread with a `new_current_thread` runtime — the
  exact `ensure_started` shape from `console.rs`, including the
  `spawn_blocking`-wrapped channel handshake so a bind failure surfaces to the
  caller rather than only inside the thread.
- **Per-widget registrations** have a **manual lifetime**. `open` mints a
  `widget_id` (UUID v4) and a one-shot `token`, inserts a `WidgetState` into
  the registry, and returns a descriptor. The opening invocation then
  returns; the widget keeps running until an explicit `close` removes the
  state and closes the websocket. (Unlike `console`, there is no
  deregister-on-drop: the registration is not tied to the opening call's
  scope, because that call has already returned while the widget lives on.)
  The listener itself stays up, exactly as `console` argues: widgets can
  open at any point throughout the process, and per-widget bind/unbind would
  be needless churn plus port-reuse races.

- **Connection-based reaping.** Because a widget's lifetime is manual and no
  longer tied to a `Registration` drop, a widget that is opened but never
  `close`d would otherwise leak its `WidgetState` (and its websocket) for the
  life of the process. The websocket connection is the natural lifetime
  owner — the widget's UI lives in the frontend, and the frontend holds the
  connection — so reap on **disconnect** rather than on an idle timer: when
  the websocket closes (tab closed, navigation, crash), the widget is
  dropped. Two edge cases need a grace period rather than a timer:
  - **Connect window.** `open` returns the descriptor *before* the frontend
    has connected. A widget that is never connected within a short
    config-driven TTL (e.g. `widget_connect_ttl_secs`) is reaped.
  - **Transient reconnect.** A brief disconnect (e.g. a page reload) should
    not immediately destroy the widget; allow a short grace period before
    reaping so the frontend can reconnect with the same token.

- **Transport keepalive, not app-level ping/pong.** The WebSocket protocol's
  own ping/pong control frames (RFC 6455 §5.5.2) are the keepalive: have the
  server send periodic pings (a small interval task — the websocket library
  answers incoming pings but does not originate them by default), so a
  frontend that dies without a clean close (process killed, network drop) is
  detected and the connection closed, which then triggers the disconnect
  reap above. No application-level `ping`/`pong` message is needed — the
  connection status is the signal, and the transport frames are consumed by
  the websocket library, which is exactly what we want: they keep the
  *connection* honest without polluting the JSON envelope. The one thing
  transport keepalive cannot do is distinguish "connected but idle" from
  "connected and active" — a frozen background tab's network stack may keep
  answering pings while its JS is suspended. If we ever need to reap
  idle-but-connected widgets, that is the point at which an app-level
  keepalive earns its keep; for now, connection status is the right
  granularity.

### 4.2 `WidgetState`

```rust
struct WidgetState {
    fields: Value,                       // the single source of truth
    visible: bool,
    bundle: Vec<u8>,                     // resolved at open time
    tag_name: String,
    token: String,                       // one-shot, gates /bundle and /ws
    ws_tx: Option<mpsc::UnboundedSender<Message>>, // the connected widget, if any
    action_ref: String,                  // attribution only
    invocation_id: String,              // attribution only
    opened_at: Instant,                 // for the connect-window TTL (§4.1)
}
```

`fields` is the shared state both sides read/write. `ws_tx` is `None` until
the widget connects; host-side `set`/`show`/`hide`/`exec` are **fire-and-
forget** when no widget is listening (the state still updates, so a later
`get` or a later connect sees the truth).

### 4.3 Public API (called by the WASM host and the internal actions)

```rust
pub async fn open(
    files: Arc<dyn FileStore>,
    bin_name: &str,
    tag_name: &str,
    fields: Value,
    action_ref: &str,
    invocation_id: &str,
) -> Result<WidgetDescriptor>;

pub async fn close(widget_id: &str) -> Result<()>;
pub async fn show(widget_id: &str) -> Result<()>;
pub async fn hide(widget_id: &str) -> Result<()>;
pub async fn get(widget_id: &str, field: &str) -> Result<Value>;
pub async fn set(widget_id: &str, field: &str, value: Value) -> Result<()>;
pub async fn exec(widget_id: &str, event: &str, payload: Value) -> Result<()>;
```

`open` reads the bundle bytes from `files` once (the bundle is a small JS
file) and stores them in the state, so the `/bundle` route is a pure
in-memory read. `get`/`set`/`show`/`hide`/`exec` mutate the state and, where
the widget should react, push a message onto `ws_tx`.

### 4.4 Routes

```
GET /bundle?token=<token>   -> bundle bytes (Content-Type: application/javascript; charset=utf-8)
GET /ws?token=<token>       -> websocket upgrade
```

The token travels in the **query string** for both, because a browser cannot
set an `Authorization` header on a `WebSocket` handshake. Unknown/missing
tokens get `401` (bundle) or a rejected upgrade (ws). The token is
single-purpose: it resolves to exactly one `WidgetState`, and it is removed
from the registry on `close`, so it cannot be replayed after teardown.

### 4.5 Message protocol (websocket)

A JSON envelope, one message per line. Two directions:

**Host → widget** (pushed when the host drives the widget):

```json
{ "op": "set",  "field": "title", "value": "…" }
{ "op": "show" }
{ "op": "hide" }
{ "op": "exec", "event": "clicked", "payload": { … } }
{ "op": "close" }
```

**Widget → host** (the widget reads/writes its own fields, shows/hides itself):

```json
{ "op": "get",  "field": "title" }
{ "op": "set",  "field": "title", "value": "…" }
{ "op": "show" }
{ "op": "hide" }
```

The loopback answers a widget-side `get` with a `{ "op": "get", "field": …,
"value": … }` reply on the same socket. A widget-side `set` updates `fields`
(so a later host `get` sees it) but does **not** echo back to the widget —
the widget already knows what it wrote. Host-side `set`/`show`/`hide`/`exec`
update state and push to the widget so its UI stays in sync. Keepalive is
handled at the transport layer (WebSocket ping/pong), not in this envelope —
see §4.1.

The websocket read loop is the only place the widget's messages are handled;
everything else in the module is the host-facing API above.

### 4.6 Security

Bound to `127.0.0.1` only. One token per widget, deregistered on `close`.
The widget never sees credentials: the token is a loopback capability, not a
solx bearer token, and the widget's own solx API access (out of scope) is a
separate host-injected adapter. Same posture as `console`/`oauth`.

---

## 5. The WASM host impl — `solx-actions/src/wasm/host.rs`

Add one `impl` block, alongside the existing `logger`/`action_exec`/
`artifact_read` impls:

```rust
impl sol::actions::widget::Host for HostState {
    async fn open(&mut self, spec: String) -> Result<String, String> {
        // parse { bin_name, tag_name, fields } from spec
        // read bundle via self.files.get(bin_name)
        // loopback::widget::open(...) -> WidgetDescriptor
        // serde_json::to_string(&descriptor)
    }
    async fn close(&mut self, widget_id: String) -> Result<(), String> { … }
    async fn show(&mut self, widget_id: String) -> Result<(), String> { … }
    async fn hide(&mut self, widget_id: String) -> Result<(), String> { … }
    async fn get(&mut self, widget_id: String, field: String) -> Result<String, String> { … }
    async fn set(&mut self, widget_id: String, field: String, value: String) -> Result<(), String> { … }
    async fn exec(&mut self, widget_id: String, event: String, payload: String) -> Result<(), String> { … }
}
```

`HostState` already holds `files: Arc<dyn FileStore>` and `caller: Caller`
(with `action_ref()` and `invocation_id()`), so `open` has everything it
needs to attribute the widget to the calling action and its invocation. The
`widget_id` minted by `open` is returned inside the descriptor; the guest
surfaces it (as its action result) so the frontend can display the widget,
and keeps it to address later `close`/`show`/`hide`/`get`/`set`/`exec` calls.

No new dependencies: `widget` is in the same world `bindgen!` already binds.

---

## 6. Internal actions — `solx-actions/src/internal/widget.rs`

Mirror each widget operation as a built-in internal action, so a `.solx`
script or any other action can drive a widget via `action-exec` without being
a WASM guest. This is the "action dispatch flows into internal actions that
are events handled by the widget frontend code" half of the design.

New submodule `internal/widget.rs`, dispatched from `run_internal`:

```rust
"widget_open"  => widget::open(params, &ctx.files).await,
"widget_close" => widget::close(params).await,
"widget_show"  => widget::show(params).await,
"widget_hide"  => widget::hide(params).await,
"widget_get"   => widget::get(params).await,
"widget_set"   => widget::set(params).await,
"widget_exec"  => widget::exec(params).await,
```

Each handler is a thin wrapper over `loopback::widget`:

- `widget_open` — `{ bin_name, tag_name, fields? }` → reads the bundle via
  `ctx.files`, calls `loopback::widget::open`, returns the descriptor JSON.
  Attribution: `ctx.caller` (action_ref + invocation_id) when present, else
  a fresh invocation id (the CLI/MCP/HTTP path has no caller). The widget
  then lives on independently of this call — `open` returns the descriptor
  and the invocation returns.
- `widget_close`/`show`/`hide`/`get`/`set`/`exec` — `{ widget_id, … }` →
  call the matching loopback function.

Seed entries under a new `/builtin/widget` subpath (add `WIDGET_PATH` to
`seed.rs`, mirroring `OAUTH_PATH`/`CONSOLE_PATH`):

```rust
a_at(WIDGET_PATH, "open",  "widget_open",  "Open a widget …", Some("WidgetOpenParams")),
a_at(WIDGET_PATH, "close", "widget_close", "Close a widget …", Some("WidgetRefParams")),
a_at(WIDGET_PATH, "show",  "widget_show",  "Show a widget …",  Some("WidgetRefParams")),
a_at(WIDGET_PATH, "hide",  "widget_hide",  "Hide a widget …",  Some("WidgetRefParams")),
a_at(WIDGET_PATH, "get",   "widget_get",   "Read a widget field …", Some("WidgetGetParams")),
a_at(WIDGET_PATH, "set",   "widget_set",   "Write a widget field …", Some("WidgetSetParams")),
a_at(WIDGET_PATH, "exec",  "widget_exec",  "Dispatch an event to a widget …", Some("WidgetExecParams")),
```

Add the corresponding param types to `solx-types/src/seed.rs` (or reuse
`EmptyParams`/a generic ref type where the shape is just `{ widget_id }`).

---

## 7. The descriptor — `solx-surface/src/entities.rs`

The old `WidgetDescriptor` (tag_name / entry_url / initial_data /
capabilities) is **replaced** with the shape the loopback actually returns:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WidgetDescriptor {
    /// Loopback-minted id; the handle every other widget op addresses.
    pub widget_id: String,
    /// Custom-element tag name the bundle registers.
    pub tag_name: String,
    /// URL the frontend fetches the JS bundle from (the loopback's /bundle).
    pub entry_url: String,
    /// WebSocket URL the frontend connects to (the loopback's /ws, token baked in).
    pub ws_url: String,
    /// One-shot loopback token (also embedded in entry_url/ws_url).
    pub token: String,
    /// Initial fields handed to the widget on mount.
    #[serde(default)]
    pub fields: Value,
}
```

It stays in `solx-surface` (already re-exported from `lib.rs`) so the
`solx-js` mirror and the frontend share one shape. It is no longer returned
by `exec` of a `Widget` action type — it is returned by `widget_open` /
`widget::open`.

---

## 8. What to remove (the old `ActionType::Widget` scaffolding)

The old plan was partially implemented. Revert it:

1. **`solx-surface/src/entities.rs`** — remove the `Widget` variant from
   `ActionType` (and its doc comment). Keep the (revised) `WidgetDescriptor`.
2. **`solx-surface/src/managers.rs`** — remove the `WidgetHost` trait.
3. **`solx-surface/src/lib.rs`** — drop `WidgetHost` from the `managers`
   re-export; keep `WidgetDescriptor`.
4. **`solx-actions/src/lib.rs`** — remove the `"widget"` arms from
   `action_type_to_str` / `action_type_from_str`, and remove the stopgap
   `Some(ActionType::Widget) => …` arm in `exec_as_with_inner`.
5. **`solx-types/src/seed.rs`** — remove `"widget"` from the
   `ActionCrudParams` `action_type` enum.
6. **`solx-js/packages/surface/src/entities.ts`** — the `ActionType` union
   already omits `widget` (it was never added there); no revert needed, but
   add the `WidgetDescriptor` mirror (see §9).
7. **`solx-widgets`** — never created; nothing to delete. Drop it from the
   plan.

`guard_executable_action` in `internal/entity.rs` is unaffected: it only
blocks `Command`/`Webhook`, and `Widget` no longer exists.

---

## 9. `solx-js` mirror

`solx-js/packages/surface/src/entities.ts` gains the descriptor mirror (the
`ActionType` union is already correct — no `widget`):

```ts
export interface WidgetDescriptor {
  widgetId: string;
  tagName: string;
  entryUrl: string;
  wsUrl: string;
  token: string;
  fields?: JsonObject;
}
```

Then rebuild + vendor (per `solx-js-save-vs-post.md` in user memory):

```bash
cd /d/Projects/solx-js && bun run build
cd /d/Projects/solx-web && bun run vendor   # if a vendor step still exists
```

Note: `solx-web`'s frontend now talks to `solx-server` directly via
`@solx/http` (the Bun backend was removed — see `next-steps.md` item 3), so
the vendor/`bun install` dance from the old plan's §7 may no longer apply;
the only hard requirement is that the `@solx/surface` alias in
`web/vite.config.ts` sees the new type.

---

## 10. `solx-web` (decoupled, out of scope this round)

Documented for completeness; not implemented here. The frontend half:

1. A `useWidgetLoader(entryUrl)` hook — inject `<script type="module">` with
   dedupe (unchanged from the old plan's §8.2).
2. A `useWidgetSocket(wsUrl)` hook — open the `WebSocket`, parse the JSON
   envelope, expose `send(op, …)` and a message subscription.
3. A `WidgetDialogHost` component — load the bundle, `createElement(tagName)`,
   set `hostContext = fields`, wire the socket, bind a `widget-close` event.
4. `ActionEditor` — when a WASM action's result is a `WidgetDescriptor`,
   offer "Open Widget" instead of the raw result.

The widget's own outbound solx API access (its "adapter") is a black box for
now — the widget is just "a thing that runs in a browser, listens to
websockets, and makes API calls."

---

## 11. Step-by-step implementation order

1. **`solx-wasm/wit/custom-action.wit`** — add the `widget` interface and
   `import widget;` to the world. *(1 file.)*
2. **`solx-wasm/src/lib.rs`** — re-export the widget guest bindings.
3. **`solx-surface`** — remove `ActionType::Widget` + `WidgetHost`; revise
   `WidgetDescriptor`; fix re-exports. *(3 files.)*
4. **`solx-actions/src/loopback/widget.rs`** (new) — the loopback module;
   register it in `loopback/mod.rs`. Add the `ws` feature to `axum` in the
   workspace `Cargo.toml` (or pull `tokio-tungstenite`).
5. **`solx-actions/src/wasm/host.rs`** — `impl sol::actions::widget::Host`.
6. **`solx-actions/src/internal/widget.rs`** (new) — the seven handlers;
   add dispatch arms in `internal/mod.rs`; add `WIDGET_PATH` + seed entries
   in `seed.rs`.
7. **`solx-actions/src/lib.rs`** — remove the `Widget` arms (type maps +
   `exec_as`).
8. **`solx-types/src/seed.rs`** — remove `"widget"` from the enum; add the
   widget param types.
9. **`cargo build -p solx-actions -p solx-wasm -p solx-manager -p solx-server`**
   + **`cargo test -p solx-actions`**. Existing tests must still pass.
10. **`solx-js`** — add the `WidgetDescriptor` mirror; rebuild.
11. **`solx-web`** — the decoupled frontend half (separate change).

---

## 12. Verification checklist

- `cargo build -p solx-actions -p solx-wasm -p solx-manager -p solx-server -p solx-cli -p solx-mcp -p solx-client` — green.
- `cargo test -p solx-actions` — existing tests pass; new loopback tests pass
  (open returns a descriptor; get/set round-trip through the registry; a
  websocket connect + widget-side `set` updates `fields`; host-side `set`
  pushes to a connected widget; `close` deregisters the token).
- A guest built against the *old* world still instantiates (backward compat).
- `bun run build` in solx-js; the `@solx/surface` alias sees `WidgetDescriptor`.
- Manual: a WASM action calls `widget.open`, returns the descriptor; the
  descriptor's `entry_url`/`ws_url` are reachable on `127.0.0.1`; a
  `widget_set` internal action updates the field a later `widget_get` reads.

---

## 13. Open questions / future work

1. **Package signing.** A future consideration — today all packages are
   trusted, so there is no integrity check on the bundle served from the
   file store. Not in scope this round.
2. **`trusted` flag.** Should be removed outright — it is not used anywhere
   today (it is an inert legacy field on `Action`, kept only to avoid a DB
   migration). It is not a widget concern; flag it as a separate cleanup,
   not part of this work.
3. **Iframe isolation.** Deferred to the frontend implementation. Decide
   then whether Shadow DOM is sufficient or an iframe + `postMessage` bridge
   is needed.
4. **Streaming.** A widget can use the existing streaming actions
   (`/builtin/web/stream/*`) for long-running work — no new streaming
   mechanism is needed for widgets.
5. **Widget lifetime is manual.** The opening invocation returns the widget
   descriptor and then *returns* — the widget keeps running independently.
   Its lifetime is governed entirely by the explicit `open`/`close` methods,
   not by the opening invocation's lifetime. State is keyed off the
   `widget_id` (minted at `open`), with `invocation_id` retained only for
   attribution.
6. **One websocket per widget.** At most one connected frontend per widget.
   Multiple simultaneous viewers would need a fan-out (`Vec<Sender>` instead
   of `Option<Sender>`). Not needed now.
