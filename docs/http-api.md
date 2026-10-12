# solx-server HTTP API

`solx-server` hosts the local solx managers over HTTP so several processes can
share one appdata directory. It is also the supported way to build a custom
client: everything `solx-cli`, `solx-mcp` and `solx-web` can do is reachable
through the routes below.

The surface is REST. An entity's reference lives in the URL, list and search
options ride in the query string, and the HTTP method carries the verb.

- **Base URL** — `http://127.0.0.1:8766` by default. The bind address and
  port come from `--host`/`--port`, else the config's `server_host`/
  `server_port`.
- **Auth** — every route except `/health` requires
  `Authorization: Bearer <token>`. The token is generated on first run and
  stored in the config; `solx server start` prints it.
- **CORS** — on a loopback bind, permissive, so a browser page can call the
  server directly. On any other bind, only the origins listed in the config's
  `server_cors_origins` are allowed (none by default). Either way, the bearer
  token is the only real gate.

### Serving other machines

Set `server_host` to `0.0.0.0` (or a specific interface IP), or start the
server with `--host`. On another machine, point a client at it with
`server_url` (or `SOLX_SERVER_URL`) and copy over the server's
`server_token`. The server prints a warning at startup when it isn't bound
to loopback, because:

- The single token grants full access (read/write on every entity, action
  execution and `/mcp`). The only way to revoke it is to rotate it, and
  rotating it breaks every client, local ones included.
- Traffic is plain HTTP, so the token is sent unencrypted. Run it over a
  private network (Tailscale/WireGuard) or behind a TLS reverse proxy (Caddy,
  nginx). Don't expose it directly to the internet.

## Entity references in the URL

Every entity is identified by a `path` (directory-like, e.g. `/research/ai`)
and a `name` (a single segment). The URL is simply the two joined:

| Reference | URL |
| --- | --- |
| path `/research/ai`, name `note` | `/docs/research/ai/note` |
| path `/` (root), name `note` | `/docs/note` |

`/`, `\` and `:` are forbidden inside a path segment or name. Everything else
is allowed — including spaces, `%`, `#` and `?` — so **percent-encode each
segment**. A name of `100% #1` becomes `100%25%20%231`.

## Routes

### Documents

| Method | Route | Body | Returns |
| --- | --- | --- | --- |
| `GET` | `/docs` | — | `Page<Document>` |
| `GET` | `/docs/{ref}` | — | `Document` |
| `PUT` | `/docs/{ref}` | `DocumentInput` | `Document` |
| `DELETE` | `/docs/{ref}` | — | `204` |
| `GET` | `/docs-paths` | — | `Page<PathFacet>` |
| `GET` | `/search` | — | `Page<Document>` |

`PUT` is create-or-replace, so it is used for both. It always answers `200`
with the saved entity — the underlying manager does not distinguish a create
from a replace, so there is no `201`.

Query parameters for `/docs` (all optional): `pathPrefix`, `limit`, `offset`,
`filterField`, `filterValue`, `sortBy`, `sortOrder` (`asc`/`desc`),
`dateAfter`, `dateBefore` (RFC 3339). The same set applies to `/types` and
`/actions`. `filterField`'s and `sortBy`'s *values* name the underlying
column (e.g. `sortBy=created_at`) and stay snake_case — only the parameter
keys themselves are camelCase.

Query parameters for `/search`: `q`, `pathPrefix`, `typeRef`, `linkedTo`,
`limit`, `offset`.

Search is a top-level route rather than `/docs/search` on purpose. A static
segment beside a catch-all wins the match for every method it is registered
under, so `/docs/search` would make a document named `search` at the root
permanently unreachable.

### Types

| Method | Route | Body | Returns |
| --- | --- | --- | --- |
| `GET` | `/types` | — | `Page<TypeEntity>` |
| `GET` | `/types/{ref}` | — | `TypeEntity` |
| `PUT` | `/types/{ref}` | `TypeInput` | `TypeEntity` |
| `DELETE` | `/types/{ref}` | — | `204` |
| `GET` | `/types-paths` | — | `Page<PathFacet>` |
| `POST` | `/validate` | `{ "value": …, "typeRef": "…" }` | `204`, or `422` |

There is no `resolve` route. Resolving a type reference is just splitting it
into `(path, name)` and fetching it — which `GET /types/{ref}` already is.

### Actions

| Method | Route | Body | Returns |
| --- | --- | --- | --- |
| `GET` | `/actions` | — | `Page<Action>` |
| `GET` | `/actions/{ref}` | — | `Action` |
| `PUT` | `/actions/{ref}` | `ActionInput` | `Action` |
| `DELETE` | `/actions/{ref}` | — | `204` |
| `POST` | `/actions/{ref}` | params (JSON) | `ActionExecResult` |
| `GET` | `/actions-paths` | — | `Page<PathFacet>` |
| `GET` | `/actions-search?q=…&exclude_hidden=…` | — | `Page<Action>` |

`POST` on an action's own URL executes it — RFC 9110's "resource-specific
processing of the request payload". The body is optional: a parameterless
action can be invoked with an empty `POST` and no `Content-Type`.

Secrets in an action's `actionConfig` are masked as `"***"` in every
response. Saving a masked config back does not destroy the stored secret.

An action whose `resultTypeRef` is `/builtin/types/WidgetDescriptor` renders
a UI. Its result is `{ "tag_name": …, "bin_name": …, "fields": … }` (this
result payload is the action's own opaque return value, not a generic entity
response, so it keeps the widget schema's own snake_case field names): create
the custom element `tag_name`, load its bundle from `GET /files/{bin_name}`,
and hand it `fields`. There is no widget-specific route or protocol — the
widget calls back in through the routes on this page like any other client.
See [widget-actions.md](widget-actions.md).

### Files

| Method | Route | Body | Returns |
| --- | --- | --- | --- |
| `GET` | `/files?prefix=…` | — | `{ "paths": [ … ] }` |
| `GET` | `/files/{relPath}` | — | raw bytes |
| `PUT` | `/files/{relPath}` | raw bytes | `{ "relPath": "…" }` |
| `DELETE` | `/files/{relPath}` | — | `204` |

File content is transferred as raw bytes, not base64. On `GET`, the
`Content-Type` is guessed from the extension (`application/octet-stream` when
unknown) — the file store does not persist a content type of its own; that
lives on a `FileRef` in document metadata. The request `Content-Type` on `PUT`
is ignored. Bodies are capped at 64 MiB.

A `relPath` may contain `/` and is used as-is (no path/name split). Traversal
is rejected by the file store.

### Other

| Method | Route | Notes |
| --- | --- | --- |
| `GET` | `/health` | Unauthenticated. Returns `{status, version, commit, commit_date}`. |
| `POST`/`GET`/`DELETE` | `/mcp` | MCP Streamable HTTP transport, same auth gate. |

## Errors

Every error response body is the tagged error itself:

```json
{ "kind": "not_found", "message": "document /research/ai/note" }
```

| `kind` | Status |
| --- | --- |
| `not_found` | 404 |
| `invalid` | 400 |
| `validation` | 422 |
| `conflict` | 409 |
| `io`, `db`, `exec`, `config`, `other` | 500 |

A malformed reference (a forbidden character, a `.`/`..` segment, a missing
name) is `400 invalid`, not a 404.

## Examples

```bash
TOKEN=...            # printed by `solx server start`
BASE=http://127.0.0.1:8766
AUTH="Authorization: Bearer $TOKEN"

# Save a document (create or replace).
curl -X PUT "$BASE/docs/research/ai/note" -H "$AUTH" \
     -H 'Content-Type: application/json' \
     -d '{"typeRef":"/types/docs/Document","title":"A note","contents":{"k":"v"}}'

# Read it back.
curl -H "$AUTH" "$BASE/docs/research/ai/note"

# List and search.
curl -H "$AUTH" "$BASE/docs?pathPrefix=/research&limit=20&sortBy=created_at"
curl -H "$AUTH" "$BASE/search?q=ada&typeRef=/types/docs/Document"

# Execute an action.
curl -X POST "$BASE/actions/tools/echo" -H "$AUTH" \
     -H 'Content-Type: application/json' -d '{"hello":"world"}'

# Upload and download a file, unencoded.
curl -X PUT "$BASE/files/media/pic.png" -H "$AUTH" --data-binary @pic.png
curl -H "$AUTH" "$BASE/files/media/pic.png" -o roundtrip.png

# Delete (204, empty body).
curl -X DELETE -H "$AUTH" "$BASE/docs/research/ai/note"
```

## Existing clients

You do not have to hand-roll one:

- **Rust** — `solx-client` (`RemoteTypeManager`, `RemoteDocManager`,
  `RemoteActionManager`, `RemoteFileStore`), implementing the
  `solx-surface` manager traits.
- **TypeScript / browser** — `@solx/http`'s `connectHttp(baseUrl, token)`,
  zero-dependency and browser-safe.

Both are thin wrappers over exactly the routes above, and are the best
reference for how a request is meant to be shaped.

## IPC (Tauri)

`solx-tauri-plugin` exposes the same managers to a Tauri webview over IPC,
for the solx desktop app (solx-explorer). There is no URL or token: the
host process holds the `App`, which is either local (the server build, also
served over HTTP by an embedded solx-server unless `server_embedded` is
`false`) or a `solx-client` proxy (the client build). From JS, use
`@solx/tauri`'s `connectTauri(invoke)`; commands are `plugin:solx|<name>`
and need the `solx:default` capability permission.

Each command calls the same manager method as the route beside it, and
fails with the same tagged `{kind, message}` error body. Arguments are
named (camelCase from JS); `path`/`name` are normalized exactly like a URL
reference.

| Command | Arguments | Route |
| --- | --- | --- |
| `docs_list` / `docs_paths` | `opts?: ListOptions` | `GET /docs`, `/docs-paths` |
| `docs_get` / `docs_delete` | `path, name` | `GET`/`DELETE /docs/{ref}` |
| `docs_save` | `path, name, input` | `PUT /docs/{ref}` |
| `docs_search` | `query?: SearchQuery` | `GET /search` |
| `types_list` / `types_paths` | `opts?` | `GET /types`, `/types-paths` |
| `types_get` / `types_delete` / `types_save` | `path, name[, input]` | `/types/{ref}` |
| `types_resolve` | `typeRef` | — (`GET /types/{ref}`) |
| `types_validate` | `value, typeRef` | `POST /validate` |
| `actions_list` / `actions_paths` | `opts?` | `GET /actions`, `/actions-paths` |
| `actions_get` / `actions_delete` / `actions_save` | `path, name[, input]` | `/actions/{ref}` |
| `actions_search` | `query?: ActionSearchQuery` | `GET /actions-search` |
| `actions_exec` | `path, name, params?` | `POST /actions/{ref}` |
| `files_list` | `prefix?` | `GET /files` (returns the bare list) |
| `files_get` | `relPath` | `GET /files/{relPath}` (raw `ArrayBuffer`) |
| `files_put` | raw bytes body; `x-solx-path` header (percent-encoded) | `PUT /files/{relPath}` (returns the path) |
| `files_delete` | `relPath` | `DELETE /files/{relPath}` |
| `info` | — | stands in for `/health`: `{mode, client, version, serverUrl?, embeddedServer?}` |
| `connect` / `disconnect` | `serverUrl, token` / — | client build only |
