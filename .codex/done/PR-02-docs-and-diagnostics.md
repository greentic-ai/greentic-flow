# PR-02: Docs + diagnostics for capability gating boundaries

## Goals
- Strengthen docs and UX around ownership boundaries and capability gating.

## Implementation Steps
1) Update README/cli docs:
   - Flow is orchestration only; runtime/operator enforces capability gating.
2) If describe contains capabilities, display them in wizard summary step.
3) Add a clear error message when component calls a denied host ref (surface the denial).

## Acceptance Criteria
- Docs clarify ownership boundaries.
- Wizard shows helpful errors for denied capabilities (where possible).


