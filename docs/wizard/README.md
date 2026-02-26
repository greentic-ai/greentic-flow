# Flow Scaffold Wizard Provider

`greentic-flow` exposes a deterministic scaffold provider under `src/wizard/` for delegated wizard orchestration.

## Contract
- `spec(mode=scaffold|new, ctx) -> QaSpec`
- `apply(mode, ctx, answers, options) -> WizardPlan`
- `execute_plan(plan)` is separate from `apply` to keep replay deterministic.

## CLI adapter in this repo
- `greentic-flow wizard new ...` is wired as the adapter entrypoint.
- Adapter flow: CLI args -> provider answers -> `apply(mode=new)` -> execute plan.
- File writing still uses existing CLI overwrite/backup behavior.

## Stable question IDs
- `flow.name`
- `flow.title` (optional)
- `flow.description` (optional)
- `flow.path`
- `flow.entrypoint`
- `flow.kind`
- `flow.nodes.scaffold`
- `flow.nodes.variant`

## Modes
- `scaffold`
- `new`

Both modes are currently aliases over the same scaffold template logic for MVP.

## Plan steps
- `EnsureDir`
- `WriteFile`
- `ValidateFlow` (in-process loader/compile/lint)
- `RunCommand` (fallback shape; execution intentionally not enabled in-process)

## Validation behavior
- Validation is opt-in via `ApplyOptions { validate: true }`.
- Default is `validate = false` for faster scaffolding.

## Starter graph variants
- `start-end`
- `start-log-end`

If `flow.nodes.scaffold = false`, the generated flow has no starter nodes.
