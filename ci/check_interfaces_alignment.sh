#!/usr/bin/env bash
set -euo pipefail

expected_version="${1:-0.4.93}"
cargo_toml="Cargo.toml"
cargo_lock="Cargo.lock"

if [[ ! -f "${cargo_toml}" ]]; then
  echo "missing ${cargo_toml}" >&2
  exit 1
fi

if [[ ! -f "${cargo_lock}" ]]; then
  echo "missing ${cargo_lock}" >&2
  exit 1
fi

if ! grep -Eq "^greentic-interfaces\\s*=\\s*\"=${expected_version}\"" "${cargo_toml}"; then
  echo "greentic-interfaces must be pinned to =${expected_version} in ${cargo_toml}" >&2
  exit 1
fi

extract_lock_version() {
  local crate_name="$1"
  awk -v target="${crate_name}" '
    $0 == "[[package]]" { in_pkg=1; name=""; version=""; next }
    in_pkg && /^name = "/ {
      name=$0
      sub(/^name = "/, "", name)
      sub(/"$/, "", name)
      next
    }
    in_pkg && /^version = "/ {
      version=$0
      sub(/^version = "/, "", version)
      sub(/"$/, "", version)
      if (name == target) {
        print version
        exit
      }
    }
  ' "${cargo_lock}"
}

interfaces_version="$(extract_lock_version "greentic-interfaces")"
guest_version="$(extract_lock_version "greentic-interfaces-guest")"

if [[ "${interfaces_version}" != "${expected_version}" ]]; then
  echo "Cargo.lock has greentic-interfaces ${interfaces_version}, expected ${expected_version}" >&2
  exit 1
fi

if [[ "${guest_version}" != "${expected_version}" ]]; then
  echo "Cargo.lock has greentic-interfaces-guest ${guest_version}, expected ${expected_version}" >&2
  exit 1
fi

echo "greentic-interfaces alignment OK (${expected_version})"
