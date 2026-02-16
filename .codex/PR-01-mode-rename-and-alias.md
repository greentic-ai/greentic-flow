# PR-01: Flow wizard mode rename upgrade -> update (with alias + regression tests)

## Goals
- Update greentic-flow 0.6 wizard to use mode name `update` (rename from upgrade).
- Keep strict schema validation and canonical CBOR behavior intact.
- Add a compatibility alias for `upgrade` in CLI for a migration window (warn).

## Implementation Steps
1) Rename mode:
   - Update `component-wizard-v0_6.wit` enum: upgrade -> update
   - Update Rust enums/mappings:
     - `WizardMode::Upgrade => "upgrade"` -> Update
     - default wizard mode (currently Upgrade) -> Update
   - Update CLI help/docs to show `update`.

2) Compatibility:
   - Accept `--mode upgrade` as alias for `update` in CLI parsing (emit deprecation warning).
   - If wizard state stores mode as string, ensure old states are read safely.

3) Keep validation:
   - Ensure `validate_config_schema(&describe, &config_cbor)` remains mandatory on update path.
   - Add regression tests:
     - update-mode path still validates
     - alias upgrade triggers update behavior

4) Diagnostics:
   - Add doc note: capability enforcement is runtime/operator-owned.
   - Optional: display “component requested capabilities: …” if available in describe payload.

5) Run:
   - `cargo fmt`
   - `cargo clippy -D warnings`
   - `cargo test`

## Acceptance Criteria
- CLI shows `default|setup|update|remove`; `upgrade` accepted as deprecated alias.
- No regression in schema validation and canonical CBOR outputs.
- Tests pass.


