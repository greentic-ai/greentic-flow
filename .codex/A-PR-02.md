# A-PR-02 — QuestionsSpec → JSON Schema + Example engine

## Summary
Add a transformation module in greentic-flow converting QuestionsSpec into:
1) JSON Schema (Draft-07+)
2) Deterministic example answers JSON

## Schema Rules
- Root object, additionalProperties=false
- id → property name
- type → schema type / enum
- required → required[]
- when → if/then

## Example Generation
- Apply defaults
- Enums: first value if no default
- Required fields filled deterministically

## Acceptance Criteria
- Example always validates
- Deterministic output
- Unit tests included
