# sol-actions

WASM Component Model action bundle for Sol.

This crate compiles to `sol-actions.wasm` and provides dispatch handlers keyed by action `fn_name`.

## Scope

- Dispatch table in `src/lib.rs`
- WIT world and interfaces in `wit/sol-actions.wit`
- Entity CRUD passthrough actions via `entity-ops` host interface
- Utility actions for filesystem, model inference, and document helpers

## Build

```bash
# Backend component (wasmtime / Tauri host)
cargo build --target wasm32-wasip2 --release

# Native tests (pure-Rust helpers only — WIT host imports not available)
cargo test --target x86_64-pc-windows-msvc
```

The resulting artifact is consumed by the Tauri host and stored as an Artifact entity named `sol-actions.wasm`.

## Dispatch table

| `action-name`                              | Host interface  | Description                         |
|--------------------------------------------|-----------------|-------------------------------------|
| `new_document` / `create_document`         | `document-ops`  | Create a typed document             |
| `get_field` / `set_field`                  | `document-ops`  | Read / write a document field       |
| `document_attach_artifact`                 | `entity-ops`    | Attach an artifact to a document    |
| `entity_new_document` … `entity_list_documents` | `entity-ops` | Document CRUD                  |
| `entity_new_action` … `entity_list_actions`     | `entity-ops` | Action CRUD                    |
| `entity_new_artifact` … `entity_list_artifacts` | `entity-ops` | Artifact CRUD                  |
| `artifact_upload_file`                     | `entity-ops`    | Upload file as Artifact             |
| `entity_new_type` … `entity_list_types`         | `entity-ops` | Type CRUD                      |
| `entity_new_doc_ref` / `entity_new_action_ref`  | `entity-ops` | Reference entity create         |
| `entity_new_model` … `entity_list_models`       | `entity-ops` | Model CRUD                     |
| `entity_new_permission` … `entity_list_permissions` | `entity-ops` | Permission CRUD             |
| `entity_new_schedule` … `entity_list_schedules` | `entity-ops` | Schedule CRUD                  |
| `entity_new_knowledge_base` / `entity_new_catalog` … | `entity-ops` | KnowledgeBase CRUD        |
| `model_chat`                               | `model-ops`     | Multi-turn model chat               |
| `model_generate`                           | `model-ops`     | Single-turn text generation         |
| `model_list`                               | `model-ops`     | List registered models              |
| `file_read` / `file_write`                 | `system-ops`    | File read / write                   |
| `dir_list` / `file_copy` / `dir_copy`      | `system-ops`    | Directory and file operations       |
| `get_env`                                  | `system-ops`    | Read environment variable           |
| `fetch_html`                               | `system-ops`    | Fetch page HTML by URL              |
| `fetch_and_extract`                        | `system-ops`    | Fetch URL then run extraction       |

### Browser actions (native dispatch, not in this WASM bundle)

The following actions have `bin_name = None` and are dispatched natively by
`sol_manager::browser_handler` through the `BrowserActions` callback that the
sol-browser app injects at startup (see `sol-browser/src-tauri/src/browser_actions_impl.rs`).
WASM components can still invoke them by name via `entity_exec("Action", ...)`.

| `fn_name`        | Effect                                  |
|------------------|-----------------------------------------|
| `dom_snapshot`   | Capture outer HTML of an element by id |
| `dom_query`      | Return outerHTML for a CSS selector    |
| `navigate`       | Navigate the browser webview to a URL  |
| `go_back`        | Go back in browser history             |
| `go_forward`     | Go forward in browser history          |
| `get_title`      | Return the current page title          |
| `current_url`    | Return the current page URL            |

## Security Notes

- Actions executed from this artifact can call host interfaces exposed by sol-browser.
- Treat uploaded/registered WASM artifacts as trusted code.
- In production usage, restrict who can write Artifact and Action records.
