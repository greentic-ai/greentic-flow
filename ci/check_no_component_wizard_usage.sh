#!/usr/bin/env bash
set -euo pipefail

if rg -n "component[-_]?wizard" src tests; then
  echo "ERROR: component-wizard must not be used in runtime/tests"
  exit 1
fi

echo "OK: no component-wizard usage in src/tests."
