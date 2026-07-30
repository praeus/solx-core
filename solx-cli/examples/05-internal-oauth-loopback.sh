#!/usr/bin/env bash
# Exercises: ActionType::Internal — the OAuth 2.0 loopback lifecycle
# (oauth_start -> simulated browser redirect -> oauth_await -> oauth_stop).
#
# The loopback registry (REGISTRY/INBOX in internal.rs) is process-local
# in-memory state with no persistence, so oauth_start/oauth_await/oauth_stop
# only see each other's state within a single `solx` process. Each
# `solx exec ...` CLI invocation is its own fresh process, so the three
# steps must run inside one `solx script` invocation (which shares one App
# across all its stages) rather than as three separate CLI calls. The
# "browser" hit is simulated via the seeded /builtin/fetch_html internal
# action (a real HTTP GET) so it runs in-process too, alongside the
# loopback calls.
set -uo pipefail
source "$(dirname "$0")/lib.sh"

echo "== 05: internal action (oauth loopback) =="

solx post action /demo/oauth/start --json '{"action_type":"internal","fn_name":"oauth_start"}' >/dev/null
solx post action /demo/oauth/await --json '{"action_type":"internal","fn_name":"oauth_await"}' >/dev/null
solx post action /demo/oauth/stop  --json '{"action_type":"internal","fn_name":"oauth_stop"}'  >/dev/null

SCRIPT_FILE="$(mktemp)"
cat > "$SCRIPT_FILE" <<'SOLX'
$start = exec /demo/oauth/start --json '{"port":18766}';
exec /builtin/fetch_html --json '{"url":"$start.result.redirect_uri?code=demo-auth-code&state=$start.result.state_value"}';
$await = exec /demo/oauth/await --json '{"state_value":"$start.result.state_value","timeout_secs":5}';
$stop = exec /demo/oauth/stop --json '{"state_value":"$start.result.state_value"}';
json '{"await":$await,"stop":$stop}'
SOLX

out=$(solx script -f "$(to_native_path "$SCRIPT_FILE")")
rm -f "$SCRIPT_FILE"

assert_true "$(jget "$out" await.success)" "oauth_await: succeeds"
assert_eq "$(jget "$out" await.result.code)" "demo-auth-code" "oauth_await: captured the redirected code"
assert_true "$(jget "$out" await.result.succeeded)" "oauth_await: succeeded=true"
assert_true "$(jget "$out" stop.result.stopped)" "oauth_stop: tears down the listener that was started+awaited in-process"

# Stopping (or awaiting) an unregistered state_value doesn't crash.
out=$(solx exec /demo/oauth/stop --json '{"state_value":"never-registered"}')
assert_eq "$(jget "$out" result.stopped)" "false" "oauth_stop: unknown state_value reports stopped=false, not an error"

if solx exec /demo/oauth/await --json '{"state_value":"never-registered","timeout_secs":1}' >/dev/null 2>&1; then
  echo "  [FAIL] oauth_await: unknown state_value should error"
  FAIL_COUNT=$((FAIL_COUNT + 1))
else
  echo "  [PASS] oauth_await: unknown state_value surfaces as an error"
  PASS_COUNT=$((PASS_COUNT + 1))
fi

report_and_exit
