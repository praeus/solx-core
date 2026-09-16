#!/usr/bin/env bash
# Exercises the seeded /builtin actions (ActionType::Internal — every
# built-in is native dispatch now, no WASM component involved): entity CRUD,
# legacy document field ops, general-purpose file store, and scoped secrets.
set -uo pipefail
source "$(dirname "$0")/lib.sh"

echo "== 04: builtin actions =="

out=$(solx list action --path /builtin)
assert_eq "$(jget "$out" total)" "47" "builtins: 47 actions catalogued"

# -- entity CRUD (Document) --------------------------------------------------
out=$(solx exec /builtin/document/entity-save-document \
  --json '{"path":"/scratch","name":"note","typeRef":"/types/docs/Document","title":"From builtin","contents":{"n":1}}')
assert_true "$(jget "$out" success)" "entity-save-document: succeeds"
assert_eq "$(jget "$out" result.title)" "From builtin" "entity-save-document: title round-trips"
assert_eq "$(jget "$out" result.path)" "/scratch" "entity-save-document: path is honored (not silently dropped to root)"

out=$(solx exec /builtin/document/entity-get-document --json '{"path":"/scratch","name":"note"}')
assert_eq "$(jget "$out" result.contents.n)" "1" "entity-get-document: contents round-trip"

out=$(solx exec /builtin/document/entity-save-document --json '{"path":"/scratch","name":"note","typeRef":"/types/docs/Document","contents":{"n":2}}')
assert_eq "$(jget "$out" result.contents.n)" "2" "entity-save-document: update (upsert) replaces contents"

out=$(solx exec /builtin/document/entity-list-documents --json '{"pathPrefix":"/scratch"}')
assert_eq "$(jget "$out" result.items.0.name)" "note" "entity-list-documents: filters by path prefix"

out=$(solx exec /builtin/document/set-field --json '{"path":"/scratch","name":"note","field":"status","value":"reviewed"}')
assert_true "$(jget "$out" success)" "set-field: writes into contents"
out=$(solx exec /builtin/document/get-field --json '{"path":"/scratch","name":"note","field":"status"}')
assert_eq "$(jget "$out" result)" "reviewed" "get-field: reads the field back"

out=$(solx exec /builtin/document/entity-delete-document --json '{"path":"/scratch","name":"note"}')
assert_true "$(jget "$out" success)" "entity-delete-document: succeeds"

# -- general-purpose file store (unrestricted rel_path access) --------------
out=$(solx exec /builtin/file/file-put --json '{"rel_path":"scratch/demo.txt","content":"hello from a builtin action"}')
assert_true "$(jget "$out" success)" "file-put: succeeds"

out=$(solx exec /builtin/file/file-get --json '{"rel_path":"scratch/demo.txt"}')
assert_eq "$(jget "$out" result.content)" "hello from a builtin action" "file-get: reads back the same bytes"

out=$(solx exec /builtin/file/file-copy --json '{"source":"scratch/demo.txt","dest":"scratch/demo-copy.txt"}')
assert_true "$(jget "$out" success)" "file-copy: succeeds"
out=$(solx exec /builtin/file/file-get --json '{"rel_path":"scratch/demo-copy.txt"}')
assert_eq "$(jget "$out" result.content)" "hello from a builtin action" "file-copy: copy has identical contents"

out=$(solx exec /builtin/file/file-delete --json '{"rel_path":"scratch/demo.txt"}')
assert_true "$(jget "$out" success)" "file-delete: succeeds"

# -- secrets, scoped to the calling action's own action_config.secrets ------
# NOTE: get-secret/set-secret persist to the real OS keyring (Windows
# Credential Manager, service "sol-secrets") rather than the sandboxed
# SOLX_APPDATA_DIR, and there's no delete_secret action to clean up after
# itself. Uses a distinctly-named test key so it's obviously a solx-examples
# artifact if you spot it later in Credential Manager. Secrets are scoped to
# whichever action row is *currently executing* — here that's the shared
# /builtin/secrets/get-secret and /builtin/secrets/set-secret rows themselves, so both need
# the key configured on their own action_config (save is upsert, so this
# just adds action_config on top of the seeded row).
secret_name="SOLX_EXAMPLE_DEMO_KEY"
key_b64=$(solx random 32)
solx save action /builtin/secrets/get-secret --json "{\"actionConfig\":{\"secrets\":{\"$secret_name\":\"$key_b64\"}}}" >/dev/null
solx save action /builtin/secrets/set-secret --json "{\"actionConfig\":{\"secrets\":{\"$secret_name\":\"$key_b64\"}}}" >/dev/null

out=$(solx exec /builtin/secrets/set-secret --json "{\"name\":\"$secret_name\",\"value\":\"correct horse battery staple\"}")
assert_true "$(jget "$out" success)" "set-secret: encrypts and stores under the action's own key"

out=$(solx exec /builtin/secrets/get-secret --json "{\"name\":\"$secret_name\"}")
assert_eq "$(jget "$out" result.value)" "correct horse battery staple" "get-secret: decrypts back to the original value"

# -- get-env/set-env round trip ---------------------------------------------
# The environment store lives in an in-process OnceLock, not on disk, so
# set-env and get-env must run inside the *same* process to observe each
# other — `solx script` dispatches both stages through one shared App.
out=$(solx script -e "exec /builtin/env/set-env --json '{\"key\":\"SOLX_EXAMPLE_ENV\",\"value\":\"round-tripped\"}'; exec /builtin/env/get-env --json '{\"key\":\"SOLX_EXAMPLE_ENV\"}'")
assert_eq "$(jget "$out" result.value)" "round-tripped" "get-env/set-env: round-trip within one process via solx script"

report_and_exit
