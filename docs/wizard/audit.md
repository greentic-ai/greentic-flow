# Flow Scaffold Wizard Audit

Date: 2026-02-23

## Scope scanned
- Search pattern used: `scaffold|new|add-step|template|doctor|wizard|qa`
- Primary files reviewed:
  - `src/bin/greentic-flow.rs`
  - `src/wizard_ops.rs`
  - `src/wizard_state.rs`
  - `src/model.rs`
  - `src/flow_ir.rs`
  - `docs/cli.md`

## Findings
- Existing flow scaffolding entrypoint is `greentic-flow new` (`src/bin/greentic-flow.rs:812`), which writes an empty flow skeleton (`nodes: {}`) with metadata and no default graph.
- Existing wizard logic (`src/wizard_ops.rs`) is component setup/apply focused, not flow-file scaffold provider focused.
- Existing wizard state persistence (`src/wizard_state.rs`) stores per-step mode metadata for component-step editing workflows.
- Existing validation path is available via `doctor` + builtin lint (`src/bin/greentic-flow.rs:724`, `src/lint/mod.rs`) and can be reused as a post-apply plan step.
- Existing flow structures (`src/model.rs`, `src/flow_ir.rs`) are sufficient to generate deterministic scaffold YAML without introducing a parallel flow type system.
- `docs/cli.md` documents `new`, `add-step`, and `doctor`; there is no dedicated flow-scaffold wizard provider contract documented yet.

## Gaps for PR-FLOW-01
- No dedicated provider module for `spec(mode=scaffold|new)` and deterministic `apply -> plan`.
- No stable namespaced question set for scaffolding (`flow.*`).
- No plan-step model in this repo for scaffold operations (`EnsureDir`, `WriteFile`, `ValidateFlow`, fallback command step).
- No tests snapshotting `answers -> plan JSON` for flow scaffolding.
- No `docs/wizard/README.md` for provider contract/usage.

## Reuse decisions
- Reuse `FlowDoc` serialization for deterministic scaffold file content.
- Reuse loader + compile + builtin lint as in-process validation for `ValidateFlow` plan steps.
- Keep command execution out of provider core; expose `RunCommand` only as fallback step shape.
