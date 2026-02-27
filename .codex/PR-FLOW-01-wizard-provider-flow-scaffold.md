# PR-FLOW-01: Fix duplicate `--non-interactive` panic

Status: accepted and unchanged in scope, keep this PR surgical.

## Goal
Remove the clap duplicate-argument panic on:

`greentic-flow wizard update-step --help`

## Changes
- Remove duplicated clap arg `--non-interactive` from either:
  - wrapper wizard args struct, or
  - flattened inner args struct.
- Keep behavior identical otherwise.
- Add a help-render smoke test for the wizard update-step command.

## Acceptance
- `cargo run --bin greentic-flow -- wizard update-step --help` exits cleanly.
- No panic from clap debug asserts.

## Notes
- Do not mix this PR with menu or i18n redesign work.
- No UX rewrite in this PR.
