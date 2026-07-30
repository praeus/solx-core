#!/usr/bin/env bash
# Exercises the seeded /builtin actions (ActionType::Internal — every
# built-in is native dispatch now, no WASM component involved): entity CRUD,
# legacy document field ops, general-purpose file store, and scoped secrets.
set -uo pipefail
source "$(dirname "$0")/lib.sh"

echo "== 04: builtin actions =="

out=$(solx list action --path /builtin)
assert_eq "$(jget "$out" total)" "30" "builtins: 30 actions catalogued"

# -- entity CRUD (Document) --------------------------------------------------
out=$(solx exec /builtin/entity_post_document \
  --json '{"path":"/scratch","name":"note","type_ref":"/types/docs/Document","title":"From builtin","contents":{"n":1}}')
assert_true "$(jget "$out" success)" "entity_post_document: succeeds"
assert_eq "$(jget "$out" result.title)" "From builtin" "entity_post_document: title round-trips"
assert_eq "$(jget "$out" result.path)" "/scratch" "entity_post_document: path is honored (not silently dropped to root)"

out=$(solx exec /builtin/entity_get_document --json '{"path":"/scratch","name":"note"}')
assert_eq "$(jget "$out" result.contents.n)" "1" "entity_get_document: contents round-trip"

out=$(solx exec /builtin/entity_post_document --json '{"path":"/scratch","name":"note","type_ref":"/types/docs/Document","contents":{"n":2}}')
assert_eq "$(jget "$out" result.contents.n)" "2" "entity_post_document: update (upsert) replaces contents"

out=$(solx exec /builtin/entity_list_documents --json '{"path_prefix":"/scratch"}')
assert_eq "$(jget "$out" result.items.0.name)" "note" "entity_list_documents: filters by path prefix"

out=$(solx exec /builtin/set_field --json '{"path":"/scratch","name":"note","field":"status","value":"reviewed"}')
assert_true "$(jget "$out" success)" "set_field: writes into contents"
out=$(solx exec /builtin/get_field --json '{"path":"/scratch","name":"note","field":"status"}')
assert_eq "$(jget "$out" result)" "reviewed" "get_field: reads the field back"

out=$(solx exec /builtin/entity_delete_document --json '{"path":"/scratch","name":"note"}')
assert_true "$(jget "$out" success)" "entity_delete_document: succeeds"

# -- general-purpose file store (unrestricted rel_path access) --------------
out=$(solx exec /builtin/file_put --json '{"rel_path":"scratch/demo.txt","content":"hello from a builtin action"}')
assert_true "$(jget "$out" success)" "file_put: succeeds"

out=$(solx exec /builtin/file_get --json '{"rel_path":"scratch/demo.txt"}')
assert_eq "$(jget "$out" result.content)" "hello from a builtin action" "file_get: reads back the same bytes"

out=$(solx exec /builtin/file_copy --json '{"source":"scratch/demo.txt","dest":"scratch/demo-copy.txt"}')
assert_true "$(jget "$out" success)" "file_copy: succeeds"
out=$(solx exec /builtin/file_get --json '{"rel_path":"scratch/demo-copy.txt"}')
assert_eq "$(jget "$out" result.content)" "hello from a builtin action" "file_copy: copy has identical contents"

out=$(solx exec /builtin/file_delete --json '{"rel_path":"scratch/demo.txt"}')
assert_true "$(jget "$out" success)" "file_delete: succeeds"

# -- secrets, scoped to the calling action's own action_config.secrets ------
# NOTE: get_secret/set_secret persist to the real OS keyring (Windows
# Credential Manager, service "sol-secrets") rather than the sandboxed
# SOLX_APPDATA_DIR, and there's no delete_secret action to clean up after
# itself. Uses a distinctly-named test key so it's obviously a solx-examples
# artifact if you spot it later in Credential Manager. Secrets are scoped to
# whichever action row is *currently executing* — here that's the shared
# /builtin/get_secret and /builtin/set_secret rows themselves, so both need
# the key configured on their own action_config (post is upsert, so this
# just adds action_config on top of the seeded row).
secret_name="SOLX_EXAMPLE_DEMO_KEY"
key_b64=$(solx random 32)
solx post action /builtin/get_secret --json "{\"action_config\":{\"secrets\":{\"$secret_name\":\"$key_b64\"}}}" >/dev/null
solx post action /builtin/set_secret --json "{\"action_config\":{\"secrets\":{\"$secret_name\":\"$key_b64\"}}}" >/dev/null

out=$(solx exec /builtin/set_secret --json "{\"name\":\"$secret_name\",\"value\":\"correct horse battery staple\"}")
assert_true "$(jget "$out" success)" "set_secret: encrypts and stores under the action's own key"

out=$(solx exec /builtin/get_secret --json "{\"name\":\"$secret_name\"}")
assert_eq "$(jget "$out" result.value)" "correct horse battery staple" "get_secret: decrypts back to the original value"

# -- get_env/set_env round trip ---------------------------------------------
# The environment store lives in an in-process OnceLock, not on disk, so
# set_env and get_env must run inside the *same* process to observe each
# other — `solx script` dispatches both stages through one shared App.
out=$(solx script -e "exec /builtin/set_env --json '{\"key\":\"SOLX_EXAMPLE_ENV\",\"value\":\"round-tripped\"}'; exec /builtin/get_env --json '{\"key\":\"SOLX_EXAMPLE_ENV\"}'")
assert_eq "$(jget "$out" result.value)" "round-tripped" "get_env/set_env: round-trip within one process via solx script"

report_and_exit
