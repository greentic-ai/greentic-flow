#!/usr/bin/env bash
set -euo pipefail

PATTERN='^package\s+greentic:component@'

MATCHES="$(rg -n --hidden --glob '!.git/*' --glob '*.wit' \
  --glob '!**/target/**' \
  "$PATTERN" . || true)"

if [[ -n "$MATCHES" ]]; then
  echo "ERROR: greentic-flow must not define canonical greentic:component WIT."
  echo
  echo "$MATCHES"
  exit 1
fi

echo "OK: No canonical greentic:component WIT found."
