# Legacy Compatibility and Deprecation Signals

This page tracks legacy surfaces that are intentionally not part of canonical
v0.6 runtime behavior.

## Legacy surfaces

1. `wit/legacy/component-wizard-v0_6.wit`
   - Status: `LEGACY / COMPAT` WIT only.
   - Canonical replacement: `greentic:component@0.6.0` + `describe()` + `invoke("setup.apply_answers")`.
2. `wit/legacy/component-wizard-legacy.wit`
   - Status: `LEGACY / COMPAT` WIT only.
   - Canonical replacement: `greentic:component@0.6.0` + `describe()` + `invoke("setup.apply_answers")`.
3. Any runtime use of `component-wizard` worlds
   - Status: disallowed in `src/**` and `tests/**` via CI/local guard.
   - Canonical replacement: canonical node world setup flow.
4. Local bindgen for wizard contracts
   - Status: disallowed for flow runtime.
   - Canonical replacement: `greentic-interfaces` bindings.
5. Legacy setup apply contract (`apply-answers(...)` export)
   - Status: deprecated and not used in runtime path.
   - Canonical replacement: invoke op `setup.apply_answers`.
6. Legacy wizard setup discovery exports (`describe/qa-spec` on wizard world)
   - Status: deprecated and not used in runtime path.
   - Canonical replacement: `node.describe()` and `descriptor.setup`.
7. `--wizard-mode upgrade`
   - Status: deprecated alias.
   - Canonical replacement: `--wizard-mode update`.
8. Unconfirmed remove operations
   - Status: disallowed.
   - Canonical replacement: mandatory `Type REMOVE to confirm`.
9. Direct usage of non-canonical WIT type modules for setup flow
   - Status: deprecated in flow runtime.
   - Canonical replacement: `greentic_interfaces::canonical::*`.
10. Reintroduction of `component-wizard` strings in runtime/tests
    - Status: blocked by `ci/check_no_component_wizard_usage.sh`.
    - Canonical replacement: canonical setup terminology.

## Usage rule

- New implementation work must target canonical v0.6 only.
- Keep legacy references isolated to `wit/legacy/**` and docs.
