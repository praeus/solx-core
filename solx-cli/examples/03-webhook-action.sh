#!/usr/bin/env bash
# Exercises: ActionType::Webhook — fn_name is the literal URL, but it must
# match a prefix in the allowed_webhook_base_urls allowlist in
# solx-config.json (deny-by-default; see docs/next-steps.md §1). Bearer auth
# resolved via auth::resolve_auth, custom headers merged in.
set -uo pipefail
source "$(dirname "$0")/lib.sh"

echo "== 03: webhook action =="

PORT=18234
SERVER_PY="$(to_native_path "$SCRIPT_DIR/mock_webhook_server.py")"
"$PY" "$SERVER_PY" "$PORT" &
SERVER_PID=$!
cleanup() { kill "$SERVER_PID" >/dev/null 2>&1 || true; }
trap cleanup EXIT

# Wait for the mock server to come up (up to ~5s).
ready=false
for _ in $(seq 1 50); do
  if curl_ok "http://127.0.0.1:$PORT/health"; then
    ready=true
    break
  fi
  sleep 0.1
done
if [ "$ready" != true ]; then
  echo "  [FAIL] mock webhook server never came up on port $PORT"
  FAIL_COUNT=$((FAIL_COUNT + 1))
  report_and_exit
  exit $?
fi

# The URL isn't allowed yet — exec must be denied before any request reaches
# the mock server.
out=$(solx save action /demo/actions/ping-webhook \
  --json "{\"action_type\":\"webhook\",\"fn_name\":\"http://127.0.0.1:$PORT/echo\",\"action_config\":{\"auth\":{\"type\":\"bearer\",\"token\":\"secret-token\"},\"headers\":{\"X-Demo\":\"1\"}}}")
assert_eq "$(jget "$out" action_type)" "webhook" "save action: action_type is webhook"
if solx exec /demo/actions/ping-webhook --json '{"greeting":"hi"}' >/dev/null 2>&1; then
  echo "  [FAIL] exec: webhook to an unlisted URL should be denied"
  FAIL_COUNT=$((FAIL_COUNT + 1))
else
  echo "  [PASS] exec: webhook to an unlisted URL is denied"
  PASS_COUNT=$((PASS_COUNT + 1))
fi

# Once the prefix is allowlisted, the same action executes normally.
allow_webhook "http://127.0.0.1:$PORT"
out=$(solx exec /demo/actions/ping-webhook --json '{"greeting":"hi"}')
assert_eq "$(jget "$out" result.received.greeting)" "hi" "exec: mock server received the saved JSON body"
assert_eq "$(jget "$out" result.auth)" "Bearer secret-token" "exec: bearer auth header resolved and forwarded"

# A second, differently-pathed action hitting the same URL needs no separate
# registration — the allowlist is a global prefix list, not per-action.
out=$(solx save action /demo/other/another-ping \
  --json "{\"action_type\":\"webhook\",\"fn_name\":\"http://127.0.0.1:$PORT/echo\"}")
out=$(solx exec /demo/other/another-ping --json '{"n":1}')
assert_eq "$(jget "$out" result.received.n)" "1" "exec: same allowlisted prefix covers a second action"
assert_eq "$(jget "$out" result.auth)" "null" "exec: no auth configured -> no Authorization header sent"

report_and_exit
