# PR-FLOW-01: Remove `component-wizard` ABI; keep default/setup/update/remove via canonical `greentic:component@0.6.0` (describe + invoke)

**Date:** 2026-02-19  
**Repo:** `greentic-flow`  
**Type:** Behavior alignment (remove parallel ABI) + docs + tests  
**Dependencies:** use local `greentic-interfaces` via `[patch.crates-io]` until crates.io is stable for canonical facade usage.

---

## Why
`greentic-flow` currently vendors/uses a parallel lifecycle ABI (`greentic:component-wizard@0.6.0`) that duplicates functionality already represented in the canonical component contract (`greentic:component@0.6.0`). This creates drift and incompatibilities.

We must keep the functionality semantics that were requested previously:

- **default** = ask only required fields without defaults
- **setup** = full personalized setup
- **update** = update an existing setup
- **remove** = remove an existing setup

…but we must not create or rely on non-standard ABIs. The canonical approach is:

- Discover lifecycle via `node.describe() -> component-descriptor` (with `setup-contract`)
- Execute lifecycle by `node.invoke(op, envelope)` with a CBOR payload that includes the desired mode

---

## Goals
1) Remove runtime usage of the `component-wizard` world (describe/qa-spec/apply-answers exports).
2) Preserve the **four mode semantics** (default/setup/update/remove) **without** a new ABI.
3) Use canonical `greentic:component@0.6.0`:
   - `describe()` for setup discovery (descriptor.setup)
   - `invoke()` for applying answers / producing config
4) Ensure greentic-flow consumes WIT types via `greentic-interfaces` canonical facade (no local bindgen for component contracts).
5) Update docs so contributors and Codex do not reintroduce a parallel solution.
6) Add tests + guards preventing `component-wizard` from creeping back.

---

## Non-goals
- Do not redesign greentic-qa in this PR.
- Do not migrate other repos (operator/runner/providers) here.
- Do not introduce new canonical WIT worlds.
- Do not change pack schemas unless required for tests.

---

## Canonical model (what greentic-flow must do)

### A) Setup discovery (standardized)
- Call `node.describe()` (canonical `greentic:component@0.6.0`)  
- Read `component-descriptor.setup`:
  - if missing => component does not support setup wizard flows
  - if present => contains:
    - `qa-spec: schema-source`
    - `answers-schema: schema-source`
    - `examples/outputs` (optional)

### B) Four modes (standardized semantics, not a new ABI)
`greentic-flow` keeps these CLI/wizard modes, but implements them as orchestrator/QA-runner policy + canonical invoke.

- **default**: run QA but only ask required unanswered fields; auto-apply defaults where defined
- **setup**: run full QA questions (personalized)
- **update**: run QA seeded from existing config; ask only what is needed/changed
- **remove**: require explicit confirmation in UX (`Type REMOVE to confirm`), then invoke removal mode

### C) Apply answers (standardized via canonical invoke op)
Instead of `apply-answers()` export, define a **standard op name** and payload contract:

- op name: **`setup.apply_answers`** (standard)
- payload: CBOR of a small record including `mode`

Recommended CBOR payload schema (conceptual):

```text
setup-apply-answers-payload = {
  mode: "default" | "setup" | "update" | "remove",
  current_config_cbor: bstr?,
  answers_cbor: bstr?,
  metadata_cbor: bstr?,
}
```

Rules:
- `current_config_cbor` required for update/remove (unless component tolerates empty)
- `answers_cbor` required for default/setup/update; may be empty for remove
- output is CBOR config (or an empty config per component rules)

greentic-flow MUST call:
- `node.invoke(op="setup.apply_answers", envelope.payload-cbor=<payload>)`

If the op is not present in `descriptor.ops`, error with a clear message.

---

## Work plan (concrete changes)

### 1) Remove internal component-wizard ABI usage (runtime path)
- Stop using local WIT bindgen for:
  - `component-wizard-v0_6.wit`
  - `component-wizard-legacy.wit`
- Remove all calls to:
  - `component-wizard.describe()`
  - `component-wizard.qa-spec(mode)`
  - `component-wizard.apply-answers(mode, current-config, answers)`

Primary files likely involved:
- `wizard_ops.rs`
- `qa_runner.rs`
- `greentic-flow.rs` (CLI)
- any module that directly calls those exported functions

### 2) Use canonical descriptor for lifecycle discovery
Add a helper module, e.g. `src/component_setup.rs`, which takes a canonical `component-descriptor` and exposes:

- `has_setup(descriptor) -> bool`
- `qa_spec_ref(descriptor) -> schema-source`
- `answers_schema_ref(descriptor) -> schema-source`
- `setup_outputs(descriptor) -> list<setup-output>` (optional)

**Required:** the descriptor and schema-source types must come from `greentic-interfaces` canonical facade:

`use greentic_interfaces::canonical::node::{component_descriptor, schema_source, setup_contract, setup_output, ...};`

No `abi::v0_6_0` imports.

### 3) Replace apply-answers world call with canonical invoke pattern
Implement a small CBOR payload struct in greentic-flow (Rust struct + serde_cbor or your CBOR tooling).
In the wizard runner path, call:

- `invoke("setup.apply_answers", envelope(payload_cbor=<encoded payload>))`

Mode mapping:
- CLI `default` => payload.mode="default"
- CLI `setup` => payload.mode="setup"
- CLI `update` => payload.mode="update", include current_config_cbor
- CLI `remove` => payload.mode="remove", include current_config_cbor; only after explicit UX confirmation (`Type REMOVE to confirm`)

### 4) Mode semantics cleanup (no non-standard ABI)
- Keep CLI flags/UX for all 4 modes.
- Document that standardized discovery is `descriptor.setup`.
- Document that standardized execution is op `setup.apply_answers` via invoke.

No new WIT exports are added. No parallel worlds are introduced.

### 5) Remove/legacy-label local wizard WIT files
Required approach for this PR:
- Move wizard WIT files to a legacy directory, e.g. `wit/legacy/`
- Add a big LEGACY banner at the top:
  “LEGACY / COMPAT only. Do not use. Replaced by greentic:component@0.6.0 describe+invoke.”
- Ensure build/bindgen does not include these WIT files.
- Add a guard check so runtime code cannot use them.

### 6) Docs sweep (must do)
Update or create:
- `docs/vision/v0.6.md`
  - Canonical component contract: describe() + invoke()
  - Setup contract is discovered via descriptor.setup
  - Modes are orchestrator semantics, carried in invoke payload
- `docs/vision/legacy.md`
  - Migration entry: component-wizard removed/replaced
- `docs/vision/codex.md`
  - Rule: never vendor a parallel component ABI
  - Rule: use `greentic_interfaces::canonical::*`
- (optional) `docs/vision/deprecations.md` if not already present

### 7) Tests + guards
Required tests:
1) **Unit test**: descriptor -> setup extraction
   - Provide a sample descriptor with `setup` and confirm helper returns correct refs.
2) **Integration test**: invoke-based apply-answers call path
   - Use a fixture/mock component (or a minimal dummy) that advertises `setup.apply_answers`
   - Validate greentic-flow encodes payload and calls invoke with correct op name.
3) **Guard test/check**: prevent reintroduction of component-wizard bindgen usage
   - Add a test or CI step that fails if:
     - `component[-_]?wizard` appears in `src/**` or `tests/**`
     - only exemptions allowed: `wit/legacy/**`, `docs/**`, `target/**`

Guard command (required):
```bash
rg -n "component[-_]?wizard" src tests \
  && echo "ERROR: component-wizard must not be used in runtime/tests" && exit 1 \
  || true
```

Optional global guard:
```bash
rg -n "component[-_]?wizard" . \
  --glob '!wit/legacy/**' --glob '!docs/**' --glob '!**/target/**' \
  && echo "ERROR: component-wizard references outside docs/legacy are not allowed" && exit 1 \
  || true
```

---

## Acceptance criteria
- `greentic-flow` no longer uses the `greentic:component-wizard@0.6.0` ABI in runtime code.
- `default/setup/update/remove` modes remain supported in CLI/wizard UX.
- Lifecycle discovery uses canonical `node.describe()` and `descriptor.setup`.
- Lifecycle execution uses canonical `node.invoke(op="setup.apply_answers")` with CBOR payload including `mode`.
- All WIT types imported from `greentic-interfaces` canonical facade (no local component bindgen).
- Docs updated: canonical approach is front-and-center; legacy wizard documented as legacy only (or removed).
- Tests pass and guard prevents reintroduction.

---

## Codex instructions (paste into greentic-flow Codex)
Implement PR-FLOW-01 exactly:
- Remove component-wizard runtime ABI usage and local bindgen references.
- Add `component_setup` helper reading descriptor.setup from canonical interfaces.
- Keep four modes (default/setup/update/remove) as orchestrator semantics.
- Implement `setup.apply_answers` invoke pattern with CBOR payload containing `mode`.
- Import WIT types via `greentic_interfaces::canonical::*` only.
- Update docs/vision and add tests + guard check.

---

## Decisions locked (2026-02-19)
1) Imports: canonical only (`greentic_interfaces::canonical::*`), no `abi::v0_6_0`.
2) Remove mode: mandatory UX confirmation (`Type REMOVE to confirm`) before invoke.
3) Wizard WIT files: move to `wit/legacy/` with explicit LEGACY banner; no runtime/bindgen references.
4) Guard scope: fail on `component[-_]?wizard` in `src/**` and `tests/**`; exempt only docs/legacy/target.
