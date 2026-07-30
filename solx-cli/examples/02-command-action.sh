#!/usr/bin/env bash
# Exercises: ActionType::Command — fn_name is the literal shell command,
# params are passed as JSON on stdin only.
set -uo pipefail
source "$(dirname "$0")/lib.sh"

echo "== 02: command action =="

out=$(solx post action /demo/actions/echo-number --json '{"action_type":"command","fn_name":"echo 42"}')
assert_eq "$(jget "$out" action_type)" "command" "post action: action_type is command"
assert_eq "$(jget "$out" fn_name)" "echo 42" "post action: fn_name stored verbatim"

out=$(solx exec /demo/actions/echo-number)
assert_true "$(jget "$out" success)" "exec: command action succeeds"
assert_eq "$(jget "$out" result)" "42" "exec: stdout '42' parsed as a JSON number"

# Command actions see their invocation params as JSON on stdin
# (exec.rs::run_command) — round-trip a value through it. `more` copies
# stdin to stdout verbatim on both cmd.exe and POSIX shells.
solx post action /demo/actions/echo-params --json '{"action_type":"command","fn_name":"more"}' >/dev/null
out=$(solx exec /demo/actions/echo-params --json '{"who":"world"}')
assert_true "$(jget "$out" success)" "exec: params-echoing command succeeds"
assert_eq "$(jget "$out" result.who)" "world" "exec: stdin carried the params JSON through"

# A command that fails (nonzero exit) must surface as success=false via an error.
solx post action /demo/actions/always-fails --json '{"action_type":"command","fn_name":"exit 1"}' >/dev/null
if solx exec /demo/actions/always-fails >/dev/null 2>&1; then
  echo "  [FAIL] exec: failing command should return a non-zero CLI exit / error"
  FAIL_COUNT=$((FAIL_COUNT + 1))
else
  echo "  [PASS] exec: failing command surfaces as an error"
  PASS_COUNT=$((PASS_COUNT + 1))
fi

report_and_exit
