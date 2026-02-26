#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODE="${1:-all}"
EN_PATH_DEFAULT="$ROOT_DIR/i18n/en.json"
EN_PATH="${EN_PATH:-$EN_PATH_DEFAULT}"
I18N_REPO="${I18N_REPO:-$ROOT_DIR/../greentic-i18n}"
I18N_TOOL="$I18N_REPO/tools/i18n.sh"

if [[ ! -f "$EN_PATH" ]]; then
  echo "missing English source map: $EN_PATH" >&2
  exit 2
fi

if [[ ! -x "$I18N_TOOL" ]]; then
  echo "missing i18n driver: $I18N_TOOL" >&2
  echo "set I18N_REPO=/path/to/greentic-i18n if needed" >&2
  exit 2
fi

EN_PATH="$(cd "$(dirname "$EN_PATH")" && pwd)/$(basename "$EN_PATH")"
EN_PATH="$EN_PATH" "$I18N_TOOL" "$MODE"
