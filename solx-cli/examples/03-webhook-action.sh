#!/usr/bin/env bash
# Exercises: ActionType::Webhook — fn_name is the literal URL, bearer auth
# resolved via webhook_auth::resolve_auth, custom headers merged in.
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

out=$(solx post action /demo/actions/ping-webhook \
  --json "{\"action_type\":\"webhook\",\"fn_name\":\"http://127.0.0.1:$PORT/echo\",\"action_config\":{\"auth\":{\"type\":\"bearer\",\"token\":\"secret-token\"},\"headers\":{\"X-Demo\":\"1\"}}}")
assert_eq "$(jget "$out" action_type)" "webhook" "post action: action_type is webhook"

out=$(solx exec /demo/actions/ping-webhook --json '{"greeting":"hi"}')
assert_eq "$(jget "$out" result.received.greeting)" "hi" "exec: mock server received the posted JSON body"
assert_eq "$(jget "$out" result.auth)" "Bearer secret-token" "exec: bearer auth header resolved and forwarded"

# No allowlist gate: any URL works purely by being posted as an action's
# fn_name (Preliminary phase decision) — a second, differently-pathed action
# hitting the same URL needs no separate registration.
out=$(solx post action /demo/other/another-ping \
  --json "{\"action_type\":\"webhook\",\"fn_name\":\"http://127.0.0.1:$PORT/echo\"}")
out=$(solx exec /demo/other/another-ping --json '{"n":1}')
assert_eq "$(jget "$out" result.received.n)" "1" "exec: no config-level webhook allowlist required"
assert_eq "$(jget "$out" result.auth)" "null" "exec: no auth configured -> no Authorization header sent"

report_and_exit
