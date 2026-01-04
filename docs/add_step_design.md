### Overview

`add-step` now works over a typed Flow IR to plan edits, apply them, and validate
the resulting flow. The IR mirrors the runtime shape (nodes, routing, entrypoints)
without relying on YAML ordering. Validation runs on the IR and enforces required
invariants before rendering back to YGTC.

### Flow IR
- `FlowIr { id, kind, entrypoints, nodes }` with entrypoints explicitly mapped.
- `NodeIr { id, kind, routing }` where `kind` can be `Component`, `Questions`,
  `Template`, or `Other`.
- `Route { to, out, status, reply }` mirrors the routing schema.
- Converters: `FlowIr::from_doc` parses a `FlowDoc`; `FlowIr::to_doc` renders a
  `FlowDoc` for serialization; `parse_flow_to_ir` loads YAML directly into IR.

### Component Catalog
- `ComponentCatalog` trait exposes `resolve(id) -> ComponentMetadata`.
- `ManifestCatalog` loads component.manifest.json files; `MemoryCatalog` lets
  tests seed components. `required_fields` drives config validation.

### add-step flow
1. **Plan** – `plan_add_step(flow_ir, spec, catalog)` checks anchor existence,
   new-node uniqueness, and component availability. Returns a plan or
   diagnostics (`ADD_STEP_*` codes).
2. **Apply** – `apply_plan(flow_ir, plan)` rewires the anchor to the new node and
   substitutes `NEXT_NODE_PLACEHOLDER` in the new node’s routing with the
   anchor’s prior routes (or inherits them if none provided).
3. **Validate** – `validate_flow(flow_ir, catalog)` emits diagnostics with codes:
   `ENTRYPOINT_MISSING`, `ROUTE_TARGET_MISSING`, `COMPONENT_NOT_FOUND`,
   `COMPONENT_PAYLOAD_REQUIRED`, `COMPONENT_CONFIG_REQUIRED`,
   `QUESTIONS_FIELDS_REQUIRED`, `TEMPLATE_EMPTY`. Consumers should fail on any
   error diagnostics.

### Tests
- Golden test under `tests/golden/add_step/` round-trips through plan/apply/
  validate and compares to expected YAML.
- Integration test exercises add-step with real component manifests when
  `ADD_STEP_REAL_MANIFEST` and `ADD_STEP_REAL_COMPONENT` env vars are set, and
  prints a skip reason otherwise.
