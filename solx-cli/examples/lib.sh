#!/usr/bin/env bash
# Shared setup + assertion helpers for the solx CLI example/test scripts.
# Source this from each numbered script: `source "$(dirname "$0")/lib.sh"`.

# Git Bash (MSYS) rewrites argv entries that look like POSIX absolute paths
# (e.g. "/builtin", "/scratch/x") into Windows paths before a native .exe
# ever sees them. solx's own `/path/name` references trigger this, so it
# must be disabled for every invocation of the solx binary.
export MSYS_NO_PATHCONV=1

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Native Windows executables (python.exe, solx.exe) don't reliably accept
# MSYS-style "/d/..." paths as argv even with MSYS_NO_PATHCONV=1 (that only
# suppresses auto-rewriting; it doesn't guarantee raw POSIX paths resolve
# correctly once the exe gets them). `cygpath -w` gives a real "D:\..."
# path; on non-Windows there's no cygpath and the POSIX path is already
# correct as-is.
to_native_path() {
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$1"
  else
    echo "$1"
  fi
}

# `curl -o /dev/null` unreliably fails ("client returned ERROR on write")
# in this environment when curl runs after a backgrounded job in the same
# script — the HTTP request itself succeeds but curl still exits non-zero.
# Route discarded response bodies through a real scratch file instead.
CURL_SCRATCH="$(mktemp)"
curl_ok() {
  curl -s -o "$CURL_SCRATCH" "$@"
}

JGET="$(to_native_path "$SCRIPT_DIR/jget.py")"

PY="${PY:-python3}"

# Isolate each run from the developer's real solx data (~/.praeus/solx or
# %APPDATA%/praeus/solx) unless the caller already set one up (run-all.sh
# shares a single dir across all sub-scripts).
if [ -z "${SOLX_APPDATA_DIR:-}" ]; then
  export SOLX_APPDATA_DIR="$(mktemp -d)"
  echo "[lib] isolated SOLX_APPDATA_DIR=$SOLX_APPDATA_DIR"
fi

# Resolve (building if necessary) the solx binary once per run.
if [ -z "${SOLX_BIN:-}" ]; then
  candidate="$REPO_ROOT/target/debug/solx.exe"
  [ -x "$candidate" ] || candidate="$REPO_ROOT/target/debug/solx"
  if [ ! -x "$candidate" ]; then
    echo "[lib] building solx-cli (first run only)..."
    (cd "$REPO_ROOT" && cargo build --quiet -p solx-cli) || { echo "[lib] build failed"; exit 1; }
  fi
  export SOLX_BIN="$candidate"
fi

solx() {
  "$SOLX_BIN" "$@"
}

# ── Assertions ───────────────────────────────────────────────────────────────

PASS_COUNT=0
FAIL_COUNT=0

# jget <json> <dotted.path> — prints the value, or "" if missing.
jget() {
  echo "$1" | "$PY" "$JGET" "$2" 2>/dev/null || true
}

assert_eq() {
  local actual="$1" expected="$2" msg="$3"
  if [ "$actual" = "$expected" ]; then
    echo "  [PASS] $msg"
    PASS_COUNT=$((PASS_COUNT + 1))
  else
    echo "  [FAIL] $msg -- expected '$expected', got '$actual'"
    FAIL_COUNT=$((FAIL_COUNT + 1))
  fi
}

assert_true() {
  local cond="$1" msg="$2"
  if [ "$cond" = "true" ]; then
    echo "  [PASS] $msg"
    PASS_COUNT=$((PASS_COUNT + 1))
  else
    echo "  [FAIL] $msg -- expected true, got '$cond'"
    FAIL_COUNT=$((FAIL_COUNT + 1))
  fi
}

assert_contains() {
  local haystack="$1" needle="$2" msg="$3"
  if [[ "$haystack" == *"$needle"* ]]; then
    echo "  [PASS] $msg"
    PASS_COUNT=$((PASS_COUNT + 1))
  else
    echo "  [FAIL] $msg -- '$needle' not found in: $haystack"
    FAIL_COUNT=$((FAIL_COUNT + 1))
  fi
}

assert_not_empty() {
  local actual="$1" msg="$2"
  if [ -n "$actual" ] && [ "$actual" != "null" ]; then
    echo "  [PASS] $msg"
    PASS_COUNT=$((PASS_COUNT + 1))
  else
    echo "  [FAIL] $msg -- value was empty/null"
    FAIL_COUNT=$((FAIL_COUNT + 1))
  fi
}

# Call at the end of each script. Exits non-zero if any assertion failed.
report_and_exit() {
  echo ""
  echo "  $PASS_COUNT passed, $FAIL_COUNT failed"
  [ "$FAIL_COUNT" -eq 0 ]
}
