# Codex Rules: Canonical Component v0.6

These rules prevent reintroduction of parallel setup contracts in
`greentic-flow`.

1. Use canonical component runtime only:
   - `greentic:component@0.6.0`
   - `describe()` for setup discovery
   - `invoke("setup.apply_answers")` for setup application
2. Use WIT-derived types from `greentic_interfaces::canonical::*`.
3. Do not add local bindgen/runtime integrations for `component-wizard` worlds.
4. Keep legacy WIT references in `wit/legacy/**` only, with explicit LEGACY banner.
5. Keep guard checks enabled so `component[-_]?wizard` does not appear in runtime/tests.
