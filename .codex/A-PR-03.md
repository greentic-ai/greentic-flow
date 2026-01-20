# A-PR-03 — Add `greentic-flow doctor-answers` CLI command

## Summary
Add a CLI command to validate answers JSON against a schema.

## CLI
```bash
greentic-flow doctor-answers \
  --schema <schema.json> \
  --answers <answers.json> \
  [--json]
```

## Behavior
- Exit 0 on success
- Exit 1 on validation failure
- Clear diagnostics

## Acceptance Criteria
- Valid answers pass
- Invalid answers fail
