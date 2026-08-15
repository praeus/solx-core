# Widget Actions

A **widget action** is a new `ActionType` that opens another UI inside a dialog within the host frontend (today: solx-web). Unlike the other five action types — which are all *executed* server-side (shell command, HTTP call, built-in handler, WASM guest, `.solx` script) — a widget is *rendered* client-side. The backend's job is just to point the frontend at the right JS bundle and hand it a tag name.

This document is the implementation plan. Read end-to-end before touching code.

---

## 1. Goals and non-goals

**Goals**

- A new `ActionType::Widget` variant that round-trips through the existing action store (save / get / list / delete), exactly like the other five.
- A widget is a **custom element** (Web Components). The bundle is plain ESM/JS that calls `customElements.define(...)` for a tag name the host knows about in advance. Vanilla TS by default; React via `@r2wc/react-to-web-component` optional.
- The widget is **isolated** from the host: it never sees credentials, it only sees a host-injected `systemRequest` proxy and a `hostContext` property. Style isolation is the widget author's choice (Shadow DOM recommended).
- Widget authoring integrates with the **existing `solx-packages` convention** (`install.solx` + `save file files/actions/shared/<name> --file bin/<name>` + `save action ...`).
- Backend → frontend coupling is **loose**: `solx-actions` does not depend on `solx-widgets`. The seam is a `WidgetHost` trait in `solx-surface`.

**Non-goals (this round)**

- Iframe isolation. Web Components + Shadow DOM is the default; iframe is a future opt-in.
- Server-side execution of widget code. Widgets are pure UI.
- A new server-side protocol (websockets, SSE, etc.). All widget I/O goes through the existing `/api/*` REST surface (already unauthenticated at the frontend→backend hop; see §6).
- Streaming responses from `exec`. The descriptor is a small JSON blob; the widget itself drives long-running work via `systemRequest` over the existing API.
- Auto-rebuild of widget bundles on upload. The `install.solx` is explicit (matches `solx-livejournal`/`solx-ollama` for WASM).

---

## 2. Architecture at a glance

```
                        ┌─────────────────────────────────────────────┐
                        │  solx-surface (the only shared contract)    │
                        │  • ActionType::Widget                       │
                        │  • WidgetDescriptor                         │
                        │  • trait WidgetHost                         │
                        └────────┬──────────────────┬─────────────────┘
                                 │                  │
              depends on trait   │                  │  depends on trait
                                 ▼                  ▼
       ┌─────────────────────────────┐    ┌──────────────────────────────┐
       │  solx-actions               │    │  solx-widgets (NEW)          │
       │  • exec_as dispatch arm     │    │  • LocalWidgetHost           │
       │    ActionType::Widget       │    │  • axum routes for           │
       │  • holds Arc<dyn WidgetHost>│    │    GET /widgets/{manifest,…} │
       └──────────────┬──────────────┘    └──────────────┬───────────────┘
                      │ wires up at startup              │
                      └──────────────┬───────────────────┘
                                     ▼
                          ┌────────────────────┐
                          │  solx-manager      │
                          │  App::wire_local   │
                          └─────────┬──────────┘
                                    ▼
                          ┌────────────────────┐
                          │  solx-server       │
                          │  mounts widget     │
                          │  axum routes       │
                          └─────────┬──────────┘
                                    ▼
                          ┌────────────────────┐
                          │  solx-web (Bun +   │
                          │  React 19)         │
                          │  • /api/widgets proxy │
                          │  • WidgetDialogHost   │
                          │  • useWidgetLoader    │
                          │  • useSystemProxy     │
                          └────────────────────┘
```

The key insight is the **trait seam** in `solx-surface`. `solx-actions` accepts `Arc<dyn WidgetHost>`. `solx-widgets` provides `LocalWidgetHost`. Neither crate imports the other.

---

## 3. The shared contract — `solx-surface`

### 3.1 `ActionType::Widget`

`solx-surface/src/entities.rs` (alongside the existing `Wasm`/`Webhook`/`Command`/`Internal`/`Script` variants):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionType {
    Wasm,
    Webhook,
    Command,
    Internal,
    Script,
    /// A client-side UI widget (`bin_name` = the JS/ESM bundle artifact,
    /// `fn_name` = the custom-element tag name). Not executed server-side:
    /// `exec` returns a [`WidgetDescriptor`] the host frontend uses to load
    /// and mount the widget in a dialog.
    Widget,
}
```

The serde `rename_all = "snake_case"` already on the enum maps the new variant to the wire string `"widget"` automatically. No code changes needed in any consumer that deserializes action_type from JSON.

### 3.2 `WidgetDescriptor`

Same file. The thing `exec` returns for a widget action:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WidgetDescriptor {
    /// The custom-element tag name the bundle registers (e.g. `my-widget`).
    pub tag_name: String,
    /// URL the host should fetch the widget's JS/ESM bundle from.
    pub entry_url: String,
    /// Initial context handed to the widget on mount (as a `hostContext`
    /// property). May be `null`.
    #[serde(default)]
    pub initial_data: serde_json::Value,
    /// Capabilities the widget declares it needs (informational — the host
    /// decides what to grant). Defaults to empty.
    #[serde(default)]
    pub capabilities: Vec<String>,
}
```

`entry_url` is computed by the host from a public base URL (e.g. `http://127.0.0.1:8766`) plus a path the host understands. The frontend doesn't need to know about the file store.

### 3.3 `WidgetHost` trait

`solx-surface/src/managers.rs` (alongside `TypeManager`/`DocManager`/`FileStore`/`ActionManager`):

```rust
/// Widget host: resolves a [`ActionType::Widget`] action into a
/// [`WidgetDescriptor`] and serves its JS/ESM bundle bytes.
///
/// Loose-coupling seam: `solx-actions` depends only on this trait (defined
/// here, in `solx-surface`); the concrete impl lives in `solx-widgets`,
/// which `solx-actions` never references directly. `solx-server`
/// constructs the concrete host and hands it to the action manager,
/// exactly like the other four managers.
///
/// The host receives the full [`Action`] (already loaded by the caller) so
/// it can read `bin_name` (the bundle artifact) and `fn_name` (the tag
/// name) without depending on `ActionManager` itself — which would
/// otherwise form an `actions → widgets → actions` cycle.
#[async_trait]
pub trait WidgetHost: Send + Sync {
    async fn describe(&self, action: &Action, params: serde_json::Value)
        -> Result<WidgetDescriptor>;
    async fn bundle(&self, action: &Action) -> Result<Vec<u8>>;
}
```

Two methods, deliberately small:

- `describe` — what the `exec` path calls. Returns a descriptor pointing at the bundle URL.
- `bundle` — what the serving route calls. Returns the raw JS bytes.

### 3.4 `Action.bin_name` doc tweak

The existing comment on `bin_name` (`Wasm: …; Script: …; Unused by Command/Webhook/Internal.`) needs the widget case added. One-line edit.

---

## 4. The widget crate — `solx-widgets` (new)

```
solx-widgets/
├── Cargo.toml
└── src/
    ├── lib.rs        # re-exports
    ├── host.rs       # LocalWidgetHost impl of WidgetHost
    └── serve.rs      # axum router (manifest + bundle)
```

Dependencies: `solx-surface`, `solx-files`, `axum`, `serde`, `serde_json`, `url`, `async-trait`. **No** `solx-actions`, `solx-manager`, or any other impl crate.

### 4.1 `LocalWidgetHost`

`src/host.rs`. Holds `Arc<dyn FileStore>` and a base URL string (e.g. `http://127.0.0.1:8766`). Resolves the bundle the same way `solx-actions::load_wasm_bytes` does (shared first, then per-action):

```rust
pub struct LocalWidgetHost {
    files: Arc<dyn FileStore>,
    base_url: String,  // e.g. "http://127.0.0.1:8766"
}

impl LocalWidgetHost {
    pub fn new(files: Arc<dyn FileStore>, base_url: impl Into<String>) -> Self;
    /// Path the serving route exposes for a given action — what the
    /// descriptor's `entry_url` will be.
    pub fn entry_path(action: &Action) -> String; // "/widgets/<path>/<name>.js"
    /// Resolve `bin_name` to bundle bytes (shared then owned lookup).
    async fn load_bundle_bytes(&self, action: &Action) -> Result<Vec<u8>>;
}
```

The `describe` impl:

1. Requires `bin_name` and `fn_name` on the action (else `Exec` error).
2. Reads `action.action_config.initial_data` if present, else uses the caller-supplied `params` (so a widget can be invoked with a payload that becomes its `hostContext`).
3. Reads `action.capabilities` into the descriptor (informational — the host gates on its own policy, but the bundle can read it from the descriptor too).
4. Builds `entry_url = format!("{base}{path}", path = Self::entry_path(action))`.

The `bundle` impl just calls `load_bundle_bytes` and returns the bytes. Same shared/owned resolution rule as WASM/script artifacts.

### 4.2 Serving routes — `solx-widgets/src/serve.rs`

Two axum handlers, mounted under whatever prefix the server uses (see §6.2 — `solx-server` mounts them at `/widgets/...`):

```
GET /widgets/manifest/{path}/{name}     -> JSON WidgetDescriptor
GET /widgets/bundle/{path}/{name}       -> raw JS bytes (Content-Type: application/javascript)
```

Both handlers take `State<AppState>` (or equivalent) holding an `Arc<dyn WidgetHost>`. The manifest handler calls `WidgetHost::describe(action, json!(null))` — the manifest is built without exec params, since the descriptor only needs static metadata + the URL. The bundle handler calls `WidgetHost::bundle(action)` and streams the bytes.

The bundle handler's `Content-Type` is `application/javascript; charset=utf-8` so the frontend can inject the response via `<script type="module">` without surprises.

### 4.3 Re-exports

`src/lib.rs`:

```rust
pub mod host;
pub mod serve;

pub use host::LocalWidgetHost;
```

---

## 5. Wiring `solx-actions` — minimal, dispatch-only

`solx-actions` already holds `Arc<dyn TypeManager>` / `Arc<dyn DocManager>` / `Arc<dyn FileStore>`. We add a fourth: `Arc<dyn WidgetHost>`. **No new dependencies** — `WidgetHost` is in `solx-surface`, which `solx-actions` already depends on.

### 5.1 `LocalActionManager::open` gains a widget host param

```rust
pub async fn open(
    db_path: &Path,
    config: Arc<ConfigService>,
    types: Arc<dyn TypeManager>,
    docs: Arc<dyn DocManager>,
    files: Arc<dyn FileStore>,
    widgets: Arc<dyn WidgetHost>,    // NEW
) -> Result<Self>
```

The struct gains a field:

```rust
pub struct LocalActionManager {
    // …existing fields…
    widgets: Arc<dyn WidgetHost>,
}
```

### 5.2 String maps

`solx-actions/src/lib.rs` `action_type_to_str` / `action_type_from_str` get a new arm each (`"widget"` ↔ `Some(ActionType::Widget)`). The `#[serde(rename_all = "snake_case")]` on the enum already handles JSON wire encoding.

### 5.3 `exec_as` dispatch arm

In `exec_as`, the `match action.action_type` block adds a single arm:

```rust
Some(ActionType::Widget) => {
    let bin_name = action.bin_name.as_deref().ok_or_else(|| {
        SolxError::Exec("widget action has no bin_name (bundle artifact)".into())
    })?;
    let tag_name = action.fn_name.as_deref().ok_or_else(|| {
        SolxError::Exec("widget action has no fn_name (custom-element tag)".into())
    })?;
    let _ = tag_name; // fn_name is checked here for clarity; the host
                      // re-derives tag_name from the action's fn_name.
    let descriptor = self.widgets.describe(&action, params).await?;
    return Ok(ActionExecResult {
        action: action_ref,
        result: serde_json::to_value(&descriptor)?,
        success: true,
        message: None,
    });
}
```

The descriptor itself contains the `tag_name` (set by `LocalWidgetHost::describe` from `action.fn_name`), so the host frontend reads it from the descriptor, not the action.

Note this arm **returns directly** with `return Ok(…)` — the `Action` was already loaded via `get_unmasked`, the descriptor is fully resolved, and there's no `result` wrapping step to run (matching how the `Wasm` arm already short-circuits).

### 5.4 `guard_executable_action` in `internal/mod.rs`

The current guard blocks `Command` and `Webhook` from being created/edited through the `entity_save_action` / `entity_delete_action` built-ins (forcing CLI use for executable actions). `Widget` is *not* executable, so it falls into the `_ => continue` branch and is allowed by default — correct, matching how `wasm`/`script` are treated today.

### 5.5 Tests

`solx-actions/src/lib.rs` already has a comprehensive test module. Add:

- `exec_widget_returns_descriptor` — saves a widget action with a stub artifact, exec's it, asserts the result is the descriptor JSON with the right `tag_name` and `entry_url`.
- `exec_widget_missing_bin_name_errors` / `exec_widget_missing_fn_name_errors` — same shape as the existing WASM counterparts.

Both tests use a tiny `StubWidgetHost` (struct holding a hardcoded descriptor and bundle bytes) so `solx-actions`' tests don't need to depend on `solx-widgets`.

---

## 6. Wiring `solx-manager` + `solx-server`

### 6.1 `App::wire_local`

`solx-manager/src/lib.rs` constructs the concrete host and passes it to `LocalActionManager::open`. The App struct gains a `widgets: Arc<dyn WidgetHost>` field (or, since `App` already implements `Solx` and that's the only consumer-facing API, we can stash it on `App` privately and hand a clone to the action manager without exposing it via `Solx` — actions is the only thing that ever needs it).

```rust
let files: Arc<dyn FileStore> = Arc::new(LocalFileStore::from_config(&config));
let widgets: Arc<dyn WidgetHost> = Arc::new(
    LocalWidgetHost::new(files.clone(), /* base URL */)
);
```

The base URL: `solx-server` constructs `LocalWidgetHost` itself, so it knows the URL it bound on. For `App::build_local`/`wire_local` (the manager path), we can compute the base URL the same way `solx-server` does (read `server_port` from config, default `solx_config::DEFAULT_SERVER_PORT` — 8766). Descriptor URLs from `wire_local` and from `solx-server` always match because both use the same config.

### 6.2 `solx-server` routes

`solx-server/src/routes/widgets.rs` (new file, alongside `actions.rs`/`docs.rs`/`files.rs`/`types.rs`). Two routes:

```rust
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/widgets/manifest/*path", get(manifest))
        .route("/widgets/bundle/*path",   get(bundle))
}

async fn manifest(State(s): State<AppState>, Path(p): Path<String>) -> Result<Json<WidgetDescriptor>, ApiError>;
async fn bundle(State(s): State<AppState>, Path(p): Path<String>) -> Result<impl IntoResponse, ApiError>;
```

The path segment `{*path}` is `/path/name` for both routes; the handler splits it via the existing `splitRef()` helper in `routes/_helpers.rs` and looks up the action via `state.app.actions().get(&path, &name)` to read `bin_name`/`fn_name`. The bundle handler doesn't need the action in full — but going through `get` keeps it consistent with the manifest handler and lets us return 404 for missing actions without an extra branch.

`solx-server/src/state.rs` gets a `widgets: Arc<dyn WidgetHost>` field. `solx-server/src/main.rs` constructs `LocalWidgetHost::new(state.app.files(), format!("http://127.0.0.1:{port}"))` and hands it both to the action manager (via `App::wire_local`-equivalent — see §6.1) and to the new routes' state.

`solx-server/src/routes/mod.rs` merges `widgets::router()` into the protected router.

---

## 7. Wiring `solx-js`

`solx-js/packages/surface/src/entities.ts` (the hand-maintained TS mirror) gets one line:

```ts
export type ActionType = 'wasm' | 'webhook' | 'command' | 'internal' | 'script' | 'widget';
export const ActionType = {
  Wasm: 'wasm', Webhook: 'webhook', Command: 'command',
  Internal: 'internal', Script: 'script', Widget: 'widget',
} as const;
```

Also export `WidgetDescriptor` (matching the Rust DTO).

Then:

```bash
cd /d/Projects/solx-js && bun run build
cd /d/Projects/solx-web && bun run vendor
# force a fresh install in solx-web/server (file: links don't always overwrite):
cd /d/Projects/solx-web/server && rm -rf node_modules bun.lock && bun install
```

(`ActionType` is the only enum the SDK mirrors; the new field `WidgetDescriptor` is just another DTO that flows through automatically once the type union includes it.)

---

## 8. Wiring `solx-web`

### 8.1 Backend proxy — `server/src/routes/widgets.ts`

Mirror of `routes/actions.ts`. Two endpoints:

```
GET /api/widgets/manifest/*path    -> { tagName, entryUrl, initialData, capabilities }
GET /api/widgets/bundle/*path      -> raw JS bytes
```

`server/src/solx-context.ts` gains `widgets: WidgetHost` (or just a `fetchWidgetsManifest`/`fetchWidgetsBundle` pair — whichever fits the existing pattern). The proxy reads the bearer token from `SOLX_SERVER_TOKEN` and forwards to `solx-server`, exactly like the other entity routes.

### 8.2 Frontend hooks and components

**`web/src/hooks/useWidgetLoader.ts`** — dynamic `<script type="module">` injection with dedupe. Behavior:

1. Look up `script[src="${entryUrl}"]`; if present, reuse (check its `data-status` attribute).
2. Otherwise create a script tag, set `async = true`, set `type = "module"`, append to `<head>`.
3. Resolve the returned promise on the script's `load` / `error` event; cache the promise on the element so concurrent callers share it.
4. On unmount: nothing to do (the script stays in the DOM for the next open).

Exports:

```ts
type LoadStatus = 'loading' | 'ready' | 'error';
export function useWidgetLoader(entryUrl: string): LoadStatus;
```

**`web/src/hooks/useSystemProxy.ts`** — the auth proxy the host injects into the widget. **Critically, the frontend is currently tokenless.** The only bearer token in the system is `SOLX_SERVER_TOKEN` on the *backend* (Bun), used to authenticate against `solx-server`. The frontend → backend hop is unauthenticated, and the backend proxies with the token.

This means the proxy is a *plain* `fetch` wrapper around the existing `/api/*` endpoints — **not** a bearer-token-injection helper like the design-doc's first draft assumed. The frontend's `request<T>()` helper (`web/src/api/api.ts`) already does exactly this (sets `Content-Type`, throws on non-2xx, parses JSON).

```ts
export interface ProxyRequestOptions extends Omit<RequestInit, 'body'> {
  body?: Record<string, unknown> | string;
}
export type SystemRequestProxy = (
  endpoint: string,
  options?: ProxyRequestOptions,
) => Promise<unknown>;
export function useSystemProxy(): SystemRequestProxy;
```

Implementation: closes over a stable `fetch` that prepends `/api`, sets `Content-Type: application/json`, JSON-stringifies object bodies, throws `ApiError` on non-2xx, returns parsed JSON. No token. The backend does the auth.

**`web/src/components/WidgetDialogHost.tsx`** — the dialog component. Mirrors the existing hand-rolled dialog pattern (`.dialog-backdrop` + `.dialog`) used by `ActionEditor.tsx`, `TypeEditor.tsx`, etc. — no portal, no new abstraction.

Props:

```ts
interface WidgetDialogHostProps {
  descriptor: WidgetDescriptor;   // from /api/widgets/manifest
  label: string;                   // for the dialog header
  onClose: () => void;
}
```

Behavior:

1. Call `useWidgetLoader(descriptor.entryUrl)`. On `ready`:
2. Create the element: `document.createElement(descriptor.tagName)`.
3. Set `(el as any).systemRequest = systemRequest()` — the closure from `useSystemProxy`.
4. Set `(el as any).hostContext = descriptor.initialData`.
5. Append to a ref'd container `<div>`.
6. Bind a `widget-close` listener on the element (bubbles up; the host also catches it on the container for safety) that calls `onClose()`.
7. On unmount: remove the element, do *not* remove the script (it's cached).

**`web/src/components/ActionEditor.tsx`** — the `ACTION_TYPES` dropdown at lines 20–26 gains `widget`. While editing a widget action, an "Open Widget" button replaces the "Run Action" button in the Run panel — clicking it calls the new manifest endpoint, opens the dialog, and lets the user run the widget.

---

## 9. Authoring a widget package

A widget package follows the existing `install.solx` convention verbatim. Example `solx-packages/solx-hello-widget/install.solx`:

```text
# Stage the bundled ESM file in the shared action file store.
save file files/actions/shared/hello-widget.js --file bin/hello-widget.js;
# Register the action. bin_name is the bundle artifact, fn_name is the
# custom-element tag name the bundle registers.
save action /packages/solx-hello-widget/hello \
    --json '{"action_type":"widget","bin_name":"hello-widget.js","fn_name":"hello-widget","caption":"Hello Widget","description":"Greets the user."}';
```

`bin/hello-widget.js` (bundled by the package author's build step — esbuild, vite, whatever):

```js
class HelloWidget extends HTMLElement {
  set hostContext(data) { this._ctx = data; }
  set systemRequest(fn) { this._req = fn; }
  connectedCallback() {
    this.innerHTML = `<button>Say hi</button>`;
    this.querySelector('button').onclick = async () => {
      // The widget never sees the bearer token — systemRequest is a
      // host-managed proxy to the existing /api/* surface.
      const r = await this._req('/actions/list', { method: 'POST', body: { limit: 5 } });
      this.innerHTML = `<pre>${JSON.stringify(r, null, 2)}</pre>`;
      this.dispatchEvent(new CustomEvent('widget-close', { bubbles: true, composed: true }));
    };
  }
}
customElements.define('hello-widget', HelloWidget);
```

This is intentionally framework-agnostic. React authors would compile `WidgetApp.tsx` to a custom element via `@r2wc/react-to-web-component` before `esbuild` bundles it.

---

## 10. Step-by-step implementation order

1. **`solx-surface`**: add `ActionType::Widget`, `WidgetDescriptor`, `WidgetHost` trait, re-exports. Doc tweak on `Action.bin_name`. *(Touches 3 files.)*
2. **`solx-types`**: add `"widget"` to the `ActionCrudParams` schema enum. *(1 line.)*
3. **`solx-widgets`** (new crate): `Cargo.toml`, `src/lib.rs`, `src/host.rs`, `src/serve.rs`. Register in `solx-core/Cargo.toml` workspace members.
4. **`solx-actions`**: add `widgets: Arc<dyn WidgetHost>` field + `open` param; `action_type_to_str`/`from_str` arms; `exec_as` arm. *(No new deps.)*
5. **`solx-manager`**: construct `LocalWidgetHost` in `wire_local`; pass to `LocalActionManager::open`. Add `widgets` field to `App`. *(1 dep: `solx-widgets`.)*
6. **`solx-server`**: `state.widgets`; new `routes/widgets.rs`; mount in `routes/mod.rs`; construct host in `main.rs`. *(1 dep: `solx-widgets`.)*
7. **`solx-actions` tests**: add `StubWidgetHost`, `exec_widget_returns_descriptor`, `exec_widget_missing_*_errors`. *(No new deps.)*
8. **`cargo build -p solx-widgets -p solx-actions -p solx-server -p solx-manager`** + **`cargo test -p solx-actions`**. Existing tests must still pass.
9. **`solx-js`**: extend the `ActionType` TS mirror; export `WidgetDescriptor`. `bun run build`; `bun run vendor` in solx-web; fresh `bun install` in `solx-web/server`.
10. **`solx-web/server/src/routes/widgets.ts`**: proxy routes.
11. **`solx-web/web`**: `hooks/useWidgetLoader.ts`, `hooks/useSystemProxy.ts`, `components/WidgetDialogHost.tsx`, update `components/ActionEditor.tsx` (`ACTION_TYPES` + "Open Widget" button).
12. **`solx-packages/solx-hello-widget`** (new example package): `install.solx` + `bin/hello-widget.js`. Documents the convention.
13. **Manual smoke test**: build everything, run solx-server, run solx-web, install `solx-hello-widget` via the existing `install.solx` flow, click "Open Widget" in ActionEditor, verify the dialog loads the bundle and `systemRequest('/actions/list', …)` returns data.

---

## 11. Verification checklist

- `cargo build -p solx-widgets -p solx-actions -p solx-server -p solx-manager -p solx-cli -p solx-mcp -p solx-client` — all green.
- `cargo test -p solx-actions` — all existing tests pass; new widget tests pass.
- `cargo test -p solx-widgets` (if any) — passes.
- `bun run build` in solx-js; `bun run vendor` in solx-web; `bun install` in solx-web/server.
- `bun run build` in solx-web/web (frontend tsc + vite).
- Manual: create a widget action via `solx-cli` (`save action ... action_type: "widget"`), confirm it appears in solx-web's ActionList with the `widget` badge, click "Open Widget", confirm the bundle loads from `solx-server` and the dialog renders.
- Manual: a widget calling `systemRequest('/actions/list', { method: 'POST', body: { limit: 5 } })` gets the same data as the existing ActionList panel.
- Manual: the existing Command/Webhook guard in `entity_save_action` still rejects new Command/Webhook actions; it does *not* reject Widget (matching WASM/Script).

---

## 12. Open questions / future work

1. **Bundle signing/integrity.** Today the bundle is served raw from the file store. A hostile actor with file-store write access could swap a bundle for a malicious one and a user opening the dialog would be pwned. Mitigation: hash pin in `action_config`, or sign bundles at upload time. Out of scope for this round — flag for follow-up.
2. **`trusted` flag.** `Action.trusted` is legacy/inert for WASM today. For widgets, a trust flag could gate whether a widget is allowed to receive `systemRequest` at all (untrusted widgets get a no-op proxy). Decide later — for now, the host's dialog is the only mounting point, and dialogs are user-initiated.
3. **Iframe isolation.** If a widget's bundle is genuinely untrusted, Shadow DOM may not be enough (DOM access is still shared). An iframe with `postMessage` bridge is the next tier. Plan documented; not implementing.
4. **Streaming exec.** Widgets are *rendered* client-side; the underlying actions they call via `systemRequest` are still regular `exec` calls. Long-running CLI actions today return only their final value. If we want live progress in the widget (the original design doc's terminal-streaming idea), the right place is `ActionExecResult` — out of scope here.
5. **Auto-vendor solx-js after the mirror edit.** Per `solx-js-save-vs-post.md` in user memory, the vendor step + a fresh `bun install` is required; otherwise the solx-web backend will still use the old `ActionType` union and reject `"widget"`.
