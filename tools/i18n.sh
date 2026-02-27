#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODE="${1:-all}"
I18N_REPO="${I18N_REPO:-$ROOT_DIR/../greentic-i18n}"
AUTH_MODE="${AUTH_MODE:-auto}"
LOCALE="${LOCALE:-en}"

if [[ ! -d "$I18N_REPO" ]]; then
  echo "missing i18n repo: $I18N_REPO" >&2
  echo "set I18N_REPO=/path/to/greentic-i18n if needed" >&2
  exit 2
fi

DEFAULT_EN_PATHS=(
  "$ROOT_DIR/i18n/en.json"
  "$ROOT_DIR/i18n/wizard/en.json"
)

resolve_en_paths() {
  if [[ -n "${EN_PATH:-}" ]]; then
    printf '%s\n' "$EN_PATH"
    return 0
  fi
  printf '%s\n' "${DEFAULT_EN_PATHS[@]}"
}

base_langs_csv() {
  local base_dir="$ROOT_DIR/i18n"
  ls -1 "$base_dir"/*.json 2>/dev/null \
    | xargs -n1 basename \
    | sed 's/\.json$//' \
    | grep -v '^en$' \
    | sort -u \
    | paste -sd, -
}

langs_for_en_path() {
  local en_path="$1"
  if [[ -n "${LANGS:-}" ]]; then
    printf '%s\n' "$LANGS"
    return 0
  fi

  if [[ "$en_path" == "$ROOT_DIR/i18n/wizard/en.json" ]]; then
    local langs
    langs="$(base_langs_csv)"
    if [[ -n "$langs" ]]; then
      printf '%s\n' "$langs"
      return 0
    fi
  fi

  printf '%s\n' "all"
}

run_for_path() {
  local mode="$1"
  local en_path="$2"
  local langs="$3"
  local cmd=(cargo run -p greentic-i18n-translator -- --locale "$LOCALE" "$mode" --langs "$langs" --en "$en_path")
  if [[ "$mode" == "translate" ]]; then
    cmd+=(--auth-mode "$AUTH_MODE")
  fi
  (cd "$I18N_REPO" && "${cmd[@]}")
}

while IFS= read -r path; do
  if [[ ! -f "$path" ]]; then
    echo "missing English source map: $path" >&2
    exit 2
  fi
  abs_path="$(cd "$(dirname "$path")" && pwd)/$(basename "$path")"
  langs="$(langs_for_en_path "$abs_path")"
  case "$MODE" in
    translate)
      echo "==> translate: $abs_path"
      run_for_path "translate" "$abs_path" "$langs"
      ;;
    validate)
      echo "==> validate: $abs_path"
      run_for_path "validate" "$abs_path" "$langs"
      ;;
    status)
      echo "==> status: $abs_path"
      run_for_path "status" "$abs_path" "$langs"
      ;;
    all)
      echo "==> translate: $abs_path"
      run_for_path "translate" "$abs_path" "$langs"
      echo "==> validate: $abs_path"
      run_for_path "validate" "$abs_path" "$langs"
      echo "==> status: $abs_path"
      run_for_path "status" "$abs_path" "$langs"
      ;;
    *)
      echo "Unknown mode: $MODE" >&2
      exit 2
      ;;
  esac
done < <(resolve_en_paths)
