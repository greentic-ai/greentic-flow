# A-PR-01 — Add `greentic-flow answers` CLI command

## Summary
Introduce a new CLI command `greentic-flow answers` that allows users to discover and materialize
the JSON answer format required by a component operation (e.g. adaptive-card `card`),
without running interactive prompts.

The command generates:
- `<name>.schema.json` — JSON Schema describing valid answers
- `<name>.example.json` — deterministic example answers generated from defaults

This command is read-only and does not modify flows.

## Motivation
Currently, components (e.g. adaptive-card) ask questions interactively during `add-step` /
`update-step`. Non-interactive usage (CI, scripts, Codex) has no way to know expected keys,
required fields, or conditionals.

`greentic-flow answers` exposes this information in a machine-readable, testable form.

## CLI Design
```bash
greentic-flow answers \
  --component <oci|path|file> \
  --operation <operation> \
  --name <prefix> \
  [--out-dir <dir>]
```

## Implementation
- Resolve component (OCI or local)
- Invoke existing questions / emit-config phase in non-interactive mode
- Collect QuestionsSpec
- Transform to schema + example (PR-02)
- Write output files

## Acceptance Criteria
- Command runs without user input
- Works with component-adaptive-card
- Generated example validates against generated schema
- No flow files are modified
