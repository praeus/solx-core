#!/usr/bin/env bash
# Exercises: ActionType::Command — fn_name is a key resolved against the
# command_actions allowlist in solx-config.json (deny-by-default; see
# docs/next-steps.md §1), params are passed as JSON on stdin only.
set -uo pipefail
source "$(dirname "$0")/lib.sh"

echo "== 02: command action =="

allow_command "echo-number" "echo 42"
out=$(solx save action /demo/actions/echo-number --json '{"action_type":"command","fn_name":"echo-number"}')
assert_eq "$(jget "$out" action_type)" "command" "save action: action_type is command"
assert_eq "$(jget "$out" fn_name)" "echo-number" "save action: fn_name stored verbatim (a command_actions key, not a literal command)"

out=$(solx exec /demo/actions/echo-number)
assert_true "$(jget "$out" success)" "exec: command action succeeds"
assert_eq "$(jget "$out" result)" "42" "exec: stdout '42' parsed as a JSON number"

# An unregistered key is denied before anything runs.
out=$(solx save action /demo/actions/unregistered --json '{"action_type":"command","fn_name":"not-a-registered-key"}')
if solx exec /demo/actions/unregistered >/dev/null 2>&1; then
  echo "  [FAIL] exec: an unregistered command key should be denied"
  FAIL_COUNT=$((FAIL_COUNT + 1))
else
  echo "  [PASS] exec: unregistered command key is denied"
  PASS_COUNT=$((PASS_COUNT + 1))
fi

# Command actions see their invocation params as JSON on stdin
# (exec.rs::run_command) — round-trip a value through it. `more` copies
# stdin to stdout verbatim on both cmd.exe and POSIX shells.
allow_command "echo-params" "more"
solx save action /demo/actions/echo-params --json '{"action_type":"command","fn_name":"echo-params"}' >/dev/null
out=$(solx exec /demo/actions/echo-params --json '{"who":"world"}')
assert_true "$(jget "$out" success)" "exec: params-echoing command succeeds"
assert_eq "$(jget "$out" result.who)" "world" "exec: stdin carried the params JSON through"

# A command that fails (nonzero exit) must surface as success=false via an error.
allow_command "always-fails" "exit 1"
solx save action /demo/actions/always-fails --json '{"action_type":"command","fn_name":"always-fails"}' >/dev/null
if solx exec /demo/actions/always-fails >/dev/null 2>&1; then
  echo "  [FAIL] exec: failing command should return a non-zero CLI exit / error"
  FAIL_COUNT=$((FAIL_COUNT + 1))
else
  echo "  [PASS] exec: failing command surfaces as an error"
  PASS_COUNT=$((PASS_COUNT + 1))
fi

report_and_exit
