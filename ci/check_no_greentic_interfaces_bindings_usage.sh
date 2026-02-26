#!/usr/bin/env bash
set -euo pipefail

if rg -n "greentic_interfaces::bindings::|\bbindings::greentic::" src tests docs README.md; then
  echo "ERROR: use greentic_interfaces::canonical instead of bindings::* in downstream code/docs"
  exit 1
fi

echo "OK: no greentic_interfaces::bindings usage in src/tests/docs/README."
