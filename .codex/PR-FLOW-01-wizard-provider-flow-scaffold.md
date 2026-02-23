# PR-FLOW-01 — Expose flow scaffolding wizard provider (spec + apply -> plan)

**Repo:** `greentic-flow`  
**Theme:** Delegating QA-driven wizards with deterministic replay, multi-frontend UI, and i18n.

## Outcomes
- Adds/extends wizard capability in this repo so **`greentic-dev wizard`** can delegate to it.
- Maintains **deterministic** behavior: **`apply()` produces a plan, execution is separate**.
- Reuses existing QA/schema primitives; avoid duplicating type systems.

## Non-goals
- No breaking CLI UX unless explicitly documented.
- No new “parallel QA types” if existing ones already exist (reuse).
## Why
`greentic-dev wizard` needs to delegate into flow scaffolding:
- create a new flow file (e.g., `.ygtc`/`.jgtc`)
- optionally scaffold a default node graph
- ensure doctor/lint passes

## Audit
- Identify flow file formats and existing CLI scaffolds.
- `rg -n "scaffold|new|add-step|template|doctor|wizard|qa" .`
- Locate internal graph structures and serializers.

Write `docs/wizard/audit.md`.

## Implementation
- Add `src/wizard/` provider:
  - `spec(mode=scaffold|new, ctx) -> QaSpec`
  - `apply(..., answers) -> WizardPlan` containing:
    - ensure dirs
    - write flow file from template
    - optional `doctor` as a step (prefer in-process check if available)

- Stable question ids:
  - `flow.name`, `flow.path`, `flow.entrypoint`, `flow.kind`, `flow.nodes.*`

- CLI adapter in this repo:
  - `greentic-flow wizard new ...` delegates to the provider (`mode=new`) and executes returned plan steps.

## Decisions (Locked)
- Dependency policy:
  - Keep repo `Cargo.toml` on versioned deps (no committed sibling path overrides).
  - For local multi-repo development, use uncommitted `.cargo/config.toml` with:
    - `[patch.crates-io]`
    - `greentic-types = { path = "../greentic-types" }`
  - If needed to resolve transitive split-brain, add additional local patches (for example `greentic-interfaces`) in the same dev-only config.
- Mode surface:
  - Expose both `mode=scaffold` and `mode=new` immediately.
  - Internal implementation may alias both to the same template for MVP.
- Starter graph MVP:
  - Provide a schema-valid minimal graph with clear start and end.
  - Preferred simple options:
    - `start -> end`
    - `start -> log/echo -> end`
- Validation behavior:
  - Validation is optional and user-controlled (`validate: bool`, default `false`).
  - Add `--validate`/`--doctor` style flag to include a final validation plan step.
  - Prefer in-process `ValidateFlow`; fallback to `RunCommand` when needed.
- Provider registration for `greentic-dev`:
  - Use explicit compile-time registry wiring for MVP (no runtime autodiscovery/plugin loading).
  - Keep CLI/wizard command layer as adapter; provider remains plan-only deterministic logic.
- Plan-step shape:
  - Standardize around reusable step forms across repos:
    - `EnsureDir`
    - `WriteFile`
    - `ValidateFlow`
    - `RunCommand` (fallback)

## Tests
- snapshot: answers -> plan JSON
- execute plan in temp dir and run existing validation on created flow

## Docs
- `docs/wizard/README.md`
## Codex prompt (copy/paste)

You are implementing **PR-FLOW-01**.  
**Pre-authorized:** create/update files, add tests, add docs, run formatting, add CI checks if needed.  
**Avoid destructive actions:** do not delete large subsystems; prefer additive refactors; keep backward compatibility unless the PR explicitly says otherwise.

Steps:
1) Perform the **Audit** tasks first and summarize findings in PR notes.
2) Implement the change list with minimal diffs aligned to the current repo patterns.
3) Add tests (unit + one integration/smoke test) and update docs.
4) Ensure `cargo fmt` + `cargo test` pass.

Repo-specific guidance:
- Reuse existing serializers and CLI scaffolding logic.
- If doctor is an external command, wrap it as a plan step.
- Keep question set minimal for MVP (name/path/kind + one default graph option).
