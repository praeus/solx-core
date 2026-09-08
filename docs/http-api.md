# solx-server HTTP API

`solx-server` hosts the local solx managers over HTTP so several processes can
share one appdata directory. It is also the supported way to build a custom
client: everything `solx-cli`, `solx-mcp` and `solx-web` can do is reachable
through the routes below.

The surface is REST. An entity's reference lives in the URL, list and search
options ride in the query string, and the HTTP method carries the verb.

- **Base URL** — `http://127.0.0.1:8766` by default (the server binds
  `127.0.0.1` only).
- **Auth** — every route except `/health` requires
  `Authorization: Bearer <token>`. The token is generated on first run and
  stored in the config; `solx server start` prints it.
- **CORS** — permissive, so a browser page can call the server directly. The
  bearer token is the only real gate.

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
| `GET` | `/health` | Unauthenticated. Returns `ok`. |
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
