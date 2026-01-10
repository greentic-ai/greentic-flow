## greentic-flow overview

- **Purpose:** Defines the YGTC flow schema, loaders, IR, validation, and add-step orchestration for inserting component nodes safely (deterministic routing, cycle checks, schema validation).
- **Key crates/modules:**
  - `loader.rs` — YAML+schema loading into `FlowDoc`.
  - `flow_ir.rs` — typed IR + conversions to/from `FlowDoc`.
-  - `add_step` — planning, rewiring, validation, and helpers (`add_step_from_config_flow`, `anchor_candidates`).
-  - `config_flow.rs` — minimal config-flow runner with type normalization and node normalization.
-  - `component_catalog.rs` — manifest-backed catalog with legacy operations normalization.
-  - `splice.rs` — legacy YAML splice helper (prefer `add_step`).
- **CLI (`src/bin/greentic-flow.rs`):** `new` (v2 skeleton), `update` (non-destructive metadata edits), `add-step`, `update-step`, `delete-step`, and `doctor` (flow linting/validation, replacing `ygtc-lint`); defaults to strict routing/validation via the `add_step` module. Add-step uses structured routing flags (`--routing-out|reply|next|multi-to|json`, default threads to anchor) and manages sidecar resolve files (`*.ygtc.resolve.json`) to track component sources.
- **Sidecar model:** `add-step`/`bind-component` write sidecar mappings (`--local-wasm` or `--component`, optional `--pin`); `update-step` requires the mapping and will fail if the referenced component artifact is missing locally (local wasm) or uncached (remote). `delete-step` removes mappings.
- **Schemas:** `schemas/ygtc.flow.schema.json` (flow), `docs/schemas` for reference.
- **Docs:** `docs/add_step_design.md` (design/behavior), `docs/add_step_audit.md` (older audit), `docs/deployment-flows.md` (deployment-specific notes).
- **Tests:** `tests/add_step_*` for add-step behavior, `tests/config_flow.rs` for config-flow runner, `tests/manifest_normalization.rs` for manifest upgrades.
- **Releases:** GitHub Releases on every `master` push bundle `greentic-flow` binaries for Linux x86_64, macOS arm64/x86_64, and Windows x86_64 (binstall-compatible `.tgz` plus SHA256 checksums).
