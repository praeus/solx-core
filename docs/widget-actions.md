# Widget Actions

A **widget** is an action that renders a UI. The action is an ordinary
action — WASM, script, internal, whatever — and it becomes a widget purely by
what it *returns*: a descriptor naming a custom-element tag and a JS/ESM
bundle in the file store.

```jsonc
{ "tag_name": "my-chart", "bin_name": "widgets/chart.js", "fields": { "title": "…" } }
```

There is **no widget runtime on the backend**. No listener, no registry, no
websocket, no server-side widget state. The frontend fetches the bundle over
the existing files route and the widget integrates by invoking actions over
the ordinary HTTP API. Both halves already exist; see `docs/http-api.md`.

---

## 1. The contract

An action declares itself a widget by setting

```
result_type_ref: "/builtin/types/WidgetDescriptor"
```

That reference is the discriminator a frontend keys off — real, queryable
metadata on the `Action` row rather than guessing from the shape of a result.
The schema is seeded in `solx-types/src/seed.rs` under `BUILTIN_TYPES_PATH`:

| Field | Required | Meaning |
| --- | --- | --- |
| `tag_name` | yes | The custom-element tag the bundle registers. |
| `bin_name` | yes | File-store path of the widget's JS/ESM bundle. |
| `fields` | no | Initial data handed to the element on mount. Any JSON. |

`result_type_ref` is stored but not validated at exec time
(`solx-actions/src/lib.rs`), so this is declarative — which is what is wanted.
A frontend that does not care can ignore it and treat the result as data.

The TypeScript mirror is `WidgetDescriptor` in
`solx-js/packages/surface/src/entities.ts`. Note it is **snake_case**, unlike
the entity types beside it: the descriptor rides inside
`ActionExecResult.result`, which `@solx/http` passes through verbatim with no
case conversion.

---

## 2. Flow

```
       action exec                                    widget invokes actions
  ┌──────────────────────┐                          ┌────────────────────────┐
  │ POST /actions/{ref}  │                          │  POST /actions/{ref}   │
  │   -> { tag_name,     │                          │  POST /actions/builtin │
  │        bin_name,     │                          │       /action/start    │
  │        fields }      │                          │       /action/poll     │
  └──────────┬───────────┘                          └───────────▲────────────┘
             │                                                  │
             ▼                                                  │
  ┌──────────────────────┐    GET /files/{bin_name}   ┌──────────┴────────────┐
  │  frontend (solx-web) ├───────────────────────────►│  the mounted element  │
  │  mounts <tag_name>   │    bundle bytes            │  <my-chart>           │
  └──────────────────────┘                            └───────────────────────┘
```

Everything on both arrows is an existing route. CORS is permissive and the
bearer token is the gate, so a browser page can drive all of it directly.

---

## 3. Mounting one (the solx-web half — still to be built)

1. After an `exec`, check `action.resultTypeRef ===
   '/builtin/types/WidgetDescriptor'`; if so, treat `result` as a descriptor.
2. Fetch the bundle. `@solx/http`'s `files.get(relPath): Promise<Uint8Array>`
   already does this. It is a fetch rather than a `<script src>` because
   `GET /files/{path}` requires the bearer header, which a script tag cannot
   set:
   ```ts
   const bytes = await solx.files.get(d.bin_name);
   const url = URL.createObjectURL(new Blob([bytes], { type: 'text/javascript' }));
   try { await import(/* @vite-ignore */ url); } finally { URL.revokeObjectURL(url); }
   ```
   Dedupe by `bin_name` so a bundle is imported once per page.
3. `document.createElement(d.tag_name)`, assign `el.fields = d.fields`, and
   inject a scoped client so the element can call back in — a `connectHttp`
   façade narrowed to what the widget should reach.
4. The widget invokes actions for everything else. For long-running work,
   `/builtin/action/start` then `/builtin/action/poll` with `wait_secs`.

Bundles are expected to be self-contained single-file ESM: a blob URL has no
useful base, so relative imports inside the bundle will not resolve.

---

## 4. Why there is no widget loopback

An earlier round built one — a process-lifetime `127.0.0.1` listener serving
the bundle and holding a websocket, a `fields` object as shared mutable state
between host and frontend, seven ops (`open`/`close`/`show`/`hide`/`get`/
`set`/`exec`) exposed twice over a WIT interface and `/builtin/widget/*`, plus
a connect-TTL / reconnect-grace reaper and two config keys. It was removed
before anything consumed it. The reasons, so this is not re-litigated:

- **The integrating half was never the loopback's.** Widget → backend calls
  were an explicit non-goal of that design, yet they are the thing that
  actually connects frontend to backend — and they need no new backend code
  at all. Building the push channel first inverted the priority.
- **It was the only websocket, and the only server-push channel, in the
  system.** Every other async path here is cursor + long-poll over an
  ordinary action call: `console_tail`, `action_poll`, `http_stream_poll`.
  `solx-server` has no SSE and no WS.
- **Its transport was strictly weaker than the HTTP API beside it.**
  `ws://127.0.0.1:{random_port}` pointed at a loopback inside whichever
  process held `LocalActionManager`; nothing proxied it, so the URLs died on
  restart and were unreachable off-box.
- **`fields`-as-shared-truth is a distributed-state problem this system does
  not otherwise have.** Flat top-level keys only, no patches, no message ids
  or request correlation, no fan-out (one viewer maximum), no hello frame, no
  resync after reconnect. Every one of those was a pending follow-up.
- **The manual lifetime was pure overhead.** Because a widget outlived its
  invocation it needed a reap sweep, a disconnect clock, two config knobs and
  a `config()` accessor on `LocalActionManager` with exactly one caller. It
  still was not right: cancelling an invocation did not close its widgets.

**What was given up: server-initiated push.** Long-polling covers it today
(`/builtin/action/poll` with `wait_secs`, `/builtin/console/tail`), and both
are reachable from a browser. If genuine push is ever needed, the right shape
is one generic SSE route on `solx-server` serving consoles, invocations *and*
widgets alike — not a per-widget socket on an ephemeral loopback port.

---

## 5. Open questions

1. **Package signing.** All packages are trusted today, so there is no
   integrity check on a bundle served out of the file store.
2. **Isolation.** Shadow DOM versus an iframe + `postMessage` bridge is a
   frontend decision, to be made when solx-web mounts its first widget. The
   bundle runs with whatever client façade it is handed, so the narrowness of
   that façade is the real boundary.
3. **Auto-rebuild of widget bundles on upload.** Not done; the `install.solx`
   convention stays explicit.
