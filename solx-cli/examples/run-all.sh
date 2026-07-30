#!/usr/bin/env bash
# Runs every numbered example/test script against one shared, isolated
# solx appdata directory, building the CLI once up front. Exit code is
# non-zero if any script fails.
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

export SOLX_APPDATA_DIR="$(mktemp -d)"
echo "solx CLI example/test suite"
echo "SOLX_APPDATA_DIR=$SOLX_APPDATA_DIR"
echo ""

overall_status=0
for script in "$SCRIPT_DIR"/0*.sh; do
  echo "---------------------------------------------------------------"
  bash "$script"
  status=$?
  if [ "$status" -ne 0 ]; then
    overall_status=1
    echo "  >> $(basename "$script") FAILED (exit $status)"
  fi
  echo ""
done

echo "---------------------------------------------------------------"
if [ "$overall_status" -eq 0 ]; then
  echo "ALL SCRIPTS PASSED"
else
  echo "SOME SCRIPTS FAILED"
fi
exit $overall_status
