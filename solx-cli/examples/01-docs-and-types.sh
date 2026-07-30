#!/usr/bin/env bash
# Exercises: post/get/list/search/delete for `type` and `doc`.
set -uo pipefail
source "$(dirname "$0")/lib.sh"

echo "== 01: types and documents =="

# -- types --------------------------------------------------------------
out=$(solx post type /demo/types/Note --json '{"description":"A demo note type","schema":{"type":"object"}}')
assert_eq "$(jget "$out" name)" "Note" "post type: created 'Note'"
assert_eq "$(jget "$out" path)" "/demo/types" "post type: path is /demo/types"

out=$(solx get type /demo/types/Note)
assert_eq "$(jget "$out" name)" "Note" "get type: round-trips"

out=$(solx list type --path /demo/types)
assert_eq "$(jget "$out" total)" "1" "list type: exactly one under /demo/types"

# -- documents ------------------------------------------------------------
out=$(solx post doc /demo/notes/hello --type /demo/types/Note --json '{"title":"Hello","contents":{"body":"first note"}}')
doc_id=$(jget "$out" id)
assert_not_empty "$doc_id" "post doc: got an id"
assert_eq "$(jget "$out" title)" "Hello" "post doc: title set"

out=$(solx get doc /demo/notes/hello)
assert_eq "$(jget "$out" contents.body)" "first note" "get doc: contents round-trip"

# update (post again onto the same reference) merges, doesn't replace title
out=$(solx post doc /demo/notes/hello --json '{"contents":{"body":"edited note"}}')
assert_eq "$(jget "$out" title)" "Hello" "post doc (update): title preserved when omitted"
assert_eq "$(jget "$out" contents.body)" "edited note" "post doc (update): contents overwritten"

out=$(solx list doc --path /demo/notes)
assert_eq "$(jget "$out" total)" "1" "list doc: exactly one under /demo/notes"

out=$(solx search "edited" --path /demo/notes)
assert_eq "$(jget "$out" total)" "1" "search: finds the edited note by full-text"

solx delete doc /demo/notes/hello >/dev/null
if solx get doc /demo/notes/hello >/dev/null 2>&1; then
  echo "  [FAIL] delete doc: still gettable after delete"
  FAIL_COUNT=$((FAIL_COUNT + 1))
else
  echo "  [PASS] delete doc: no longer gettable"
  PASS_COUNT=$((PASS_COUNT + 1))
fi

report_and_exit
