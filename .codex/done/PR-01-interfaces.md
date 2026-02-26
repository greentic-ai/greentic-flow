# PR-01-interfaces: Downstream repos must use `greentic_interfaces::canonical` (never `bindings::*`)

**Date:** 2026-02-19  
**Scope:** Any repo that depends on `greentic-interfaces`

## Why
`greentic-interfaces` generates WIT bindings under world/version-scoped modules
(for example `bindings::greentic_component_0_6_0_component::...`).
Those paths are not stable across versions/world sets, and there is no guaranteed
`bindings::greentic::...` root in external consumer builds.

To keep downstream code stable and version-ready, consumers must use the canonical facade:

- `greentic_interfaces::canonical::types`
- `greentic_interfaces::canonical::node`
- `greentic_interfaces::canonical::core`

`bindings::*` is internal implementation detail.

## Rule (must-follow)
Never import from `greentic_interfaces::bindings::*` (or `bindings::greentic::*`) in
application/library code, tests, or README/examples.

The only allowed location for `bindings::*` references is inside `greentic-interfaces`
itself (ABI facade internals).

## Required changes in downstream repos

### 1) Update imports

Replace:

```rust
use greentic_interfaces::bindings::greentic_component_0_6_0_component::greentic::interfaces_types::types as wit_types;
```

With:

```rust
use greentic_interfaces::canonical::types as wit_types;
```

### 2) Update type aliases and matches

Replace:

```rust
type WitProtocol = greentic_interfaces::bindings::greentic::interfaces_types::types::Protocol;
```

With:

```rust
type WitProtocol = greentic_interfaces::canonical::types::Protocol;
```

### 3) Update tests and README/examples too

Tests/docs are copy-paste sources and must follow the same rule.

## Search patterns

- `greentic_interfaces::bindings::`
- `bindings::greentic::`
- `interfaces_types::types::` (when prefixed by `bindings::`)
- `greentic_component_` (inside `bindings::` path)

## Acceptance criteria

- `cargo test` passes
- No `greentic_interfaces::bindings` references remain (outside `greentic-interfaces`)
- README/examples compile where applicable

## Recommended CI guardrail

```bash
rg -n "greentic_interfaces::bindings::|\bbindings::greentic::" src tests docs README.md \
  && echo "ERROR: use greentic_interfaces::canonical instead" && exit 1 \
  || true
```
