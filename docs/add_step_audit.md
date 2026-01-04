add-step audit (greentic-flow)
==============================

Executive summary
- Mode selection: fail – no CLI/subcommand or flag surface for add-step, and library API takes a fully-populated `AddStepSpec` with no default/config switch (`src/bin/greentic-flow.rs:19-23`, `src/add_step.rs:11-86`).
- Schema compliance: fail – YAML splice path accepts arbitrary node maps (including `tool`) and never normalizes against the flow schema (`src/splice.rs:62-79`, `schemas/ygtc.flow.schema.json:33-118`).
- Determinism: partial – no randomness and use of `IndexMap`/`BTreeMap` yields stable ordering (`src/add_step.rs:89-104`, `src/flow_ir.rs:119-198`), but IDs are caller-provided with no stable generation strategy.
- Placement/rewiring: partial – anchor rewired to a single unconditional `to` route, dropping prior status/out/reply semantics (`src/add_step.rs:89-99`), and no support for default placement when `--after` is omitted.
- Validation: partial – schema validation happens only when the flow is (re)loaded, not during add-step; `validate_flow` is opt-in and not invoked by any CLI (`src/add_step.rs:107-189`, `src/loader.rs:17-213`).

Call graph (current code)
- CLI: no `add-step` command; the only CLI entrypoint is `greentic-flow new` (`src/bin/greentic-flow.rs:19-106`).
- Library happy-path (tests): load flow → `parse_flow_to_ir`/`FlowIr::from_doc` (`src/flow_ir.rs:69-117`) → `plan_add_step` (`src/add_step.rs:36-87`) → `apply_plan` (`src/add_step.rs:89-104`) → optional `validate_flow` (`src/add_step.rs:107-189`) → render via `FlowIr::to_doc` (`src/flow_ir.rs:119-198`) → downstream compile/serialize.
- YAML splice helper: `splice_node_after` rewrites routing and inserts the raw node map (`src/splice.rs:16-79`); no validation follows.

Flow schema expectations
- Embedded schema `$id` `ygtc.flow.schema.json` (Draft 2020-12) is always used when loading flows (`src/loader.rs:13-111`).
- Nodes must match `patternProperties` with exactly one component key plus optional `routing`, `output`, `telemetry`, `pack_alias`, `operation`; `tool` is not allowed (`schemas/ygtc.flow.schema.json:33-118`).
- Compiled flows are emitted with `schema_version: "flow-v1"` regardless of input (`src/lib.rs:125`), so pack flows with `schema_version: 1` map to this schema only.

DEFAULT mode (not implemented)
- No code paths load a component dev_flow/template; `plan_add_step` expects the caller to supply `payload`, `pack_alias`, `operation`, and optional routing up front (`src/add_step.rs:11-86`).
- There is no prompt or defaulting logic; the only templating is `NEXT_NODE_PLACEHOLDER` replacement inside provided routing (`src/add_step.rs:191-208`).

CONFIG mode behavior
- The config-flow harness executes a flow under the embedded schema, seeding state from provided answers and following the first non-out route until a `template` node emits `{node_id, node}` (`src/config_flow.rs:28-94`).
- Answers come from the passed `Map<String, Value>`; missing answers without defaults fail (`src/config_flow.rs:110-138`).
- Template rendering substitutes `{{state.key}}` in strings and returns arbitrary JSON (`src/config_flow.rs:142-186`).
- Post-processing only normalizes a `tool` wrapper into schema fields (`src/config_flow.rs:208-276`); it does not derive IDs or reroute.
- No add-step path invokes `run_config_flow`; wiring the config output into a flow is left to the caller (e.g., via `AddStepSpec` or `splice_node_after`), so mode separation is absent.

Placement and edge rewiring
- `plan_add_step` records the anchor’s prior routing and precomputes the new node’s routing with `NEXT_NODE_PLACEHOLDER` substitution or fallback inheritance (`src/add_step.rs:67-86`, `src/add_step.rs:191-208`).
- `apply_plan` overwrites the anchor routing with a single `{to: new_id}` route (no status/out/reply), then appends the new node (`src/add_step.rs:89-104`). Branch metadata on the anchor is discarded; any multi-branch logic migrates to the new node only.
- YAML helper `splice_node_after` mirrors this logic at the YAML level, replacing the anchor routing and inserting the new node mapping (`src/splice.rs:55-79`, `src/splice.rs:117-143`).
- No handling for “no --after” exists; the caller must supply an anchor or planning fails (`src/add_step.rs:49-55`).

Determinism audit
- IDs: generated externally; the library never derives stable IDs (caller passes `AddStepSpec.new_id`, `src/add_step.rs:11-20`). Config flows can emit placeholders like `COMPONENT_STEP` unchanged (`tests/config_flow.rs:71-117`).
- Ordering: `IndexMap` preserves insertion for IR; serialization via `BTreeMap` in `FlowIr::to_doc` is stable (`src/flow_ir.rs:119-198`).
- Placeholder replacement and routing rewrites are pure functions with deterministic vector order (`src/add_step.rs:191-208`, `src/splice.rs:117-143`).
- Randomness: none detected in add-step path (no uuid/rand usage; repo scan empty of runtime calls).

Validation behavior
- Loading uses JSON Schema validation before producing `FlowDoc` (`src/loader.rs:83-213`), but add-step operates on already-loaded IR/YAML and does not re-run schema validation.
- `validate_flow` checks entrypoints, routing targets, and required config keys against the component catalog, but is not automatically invoked by `apply_plan` or the YAML splice helper (`src/add_step.rs:107-189`).
- No CLI enforces post-write validation; tests call `validate_flow` manually (`tests/add_step_golden.rs:33-47`).

Known repros (schema_version:1 pack, insert after start)
- If a config flow emits `{ node: { tool: { component, ... }, routing: [{to: NEXT_NODE_PLACEHOLDER}] } }`, splicing that YAML output directly via `splice_node_after` preserves the `tool` key, yielding a schema-invalid node (`src/splice.rs:62-79`) even though the schema forbids `tool` (`schemas/ygtc.flow.schema.json:33-118`).
- The same config template can emit a hard-coded `node_id: "COMPONENT_STEP"` (fixture behavior in `tests/config_flow.rs:71-117`); add-step never derives or normalizes it, so the inserted node id stays non-deterministic/opaque (`src/add_step.rs:11-20`).
- Anchor routing is overwritten with a single `to` edge, stripping any prior `out`/status/reply semantics and potentially creating back-edges if the inherited routing pointed upstream (`src/add_step.rs:89-104`).

Gap list (mismatches vs target contract)
| Gap | Severity | Location | Expected | Current | Minimal fix direction |
| --- | --- | --- | --- | --- | --- |
| G1: Node shape uses `tool` (schema-invalid) | P0 | `src/splice.rs:62-79`; schema disallows `tool` `schemas/ygtc.flow.schema.json:33-118` | Add-step/config path should normalize config-flow output into component + pack_alias/operation fields before writing | Raw YAML insertion keeps `tool`, producing schema-invalid flows | Normalize node maps (reuse `normalize_node_shape`) before splice/render |
| G2: Node id derivation (`COMPONENT_STEP`) | P1 | `tests/config_flow.rs:71-117`; `src/add_step.rs:11-20` | Default/config modes should derive stable ids from component + context | IDs are caller/template provided and pass through unchanged | Add deterministic id generator keyed on component + anchor |
| G3: Edge rewiring drops semantics | P0 | `src/add_step.rs:89-104`; `src/splice.rs:55-79` | Insertion should preserve anchor route metadata (status/out/reply) while threading through new node | Anchor routing is replaced with unconditional `{to: new_id}`, losing branch/end semantics and enabling cycles | Rewire by wrapping prior routes without stripping flags/status |
| G4: Default vs config mode separation | P1 | `src/bin/greentic-flow.rs:19-23`; `src/add_step.rs:11-86`; `src/config_flow.rs:28-94` | CLI/entry should let users choose default (template) vs config (questions) modes | No add-step CLI and no mode-aware planner; config harness not wired in | Introduce add-step command with mode flag and drive appropriate source (dev_flow vs config flow) |
| G5: No deterministic routing/entry validation in pipeline | P1 | `src/add_step.rs:107-189`; `src/loader.rs:83-213` | add-step should validate schema + routing after apply and fail fast | Validation is optional and not invoked by any entrypoint | Run schema + `validate_flow` after apply before writing output |
| G6: No default anchor handling | P2 | `src/add_step.rs:49-55` | When `--after` omitted, insert at first/entry node per contract | Planning errors if anchor absent; caller must supply `after` | Default anchor to entrypoint or first node when flag missing |

