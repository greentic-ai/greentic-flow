# greentic-flow CLI guide

This CLI edits YGTc v2 flows in-place and keeps a resolve sidecar (`<flow>.ygtc.resolve.json`) up to date. Nodes use the v2 authoring shape:

```yaml
nodes:
  my-node:
    handle_message:
      input: { msg: "hi" }
    routing: out        # or "reply" or an array of routes
```

Routing shorthand (`routing: out|reply`) is accepted on read and emitted only when routing is exactly that terminal edge. Flows never embed component ids; sidecar entries track component sources.

## Commands

### new
Create a minimal v2 flow file.

```
greentic-flow new --flow flows/main.ygtc --id main --type messaging \
  [--schema-version 2] [--name "Title"] [--description "text"] [--force]
```

Writes an empty `nodes: {}` skeleton. Refuses to overwrite unless `--force`.

### update
Non-destructive metadata edits (name/description/tags/id/type/schema_version).

```
greentic-flow update --flow flows/main.ygtc --name "New Title" --tags foo,bar
```

Preserves nodes/entrypoints. Changing `--type` is allowed only on empty flows (no nodes, no entrypoints, no start). Fails if the file is missing.

### add-step
Developer guide: insert a component-backed node and keep the sidecar in sync. Always writes v2 YAML; sidecar tracks where to fetch/locate the component (local wasm or remote ref).

Start simple (local wasm, manual payload):
```
greentic-flow add-step --flow flows/main.ygtc \
  --node-id hello-world \
  --operation handle_message --payload '{"input":"hi"}' \
  --local-wasm components/hello-world/target/wasm32-wasip2/release/hello_world.wasm
```
- Uses your local build artifact; sidecar stores a relative path. Add `--pin` to hash the wasm for reproducibility.
- Routing defaults to “thread to anchor’s current targets” (no placeholder exposed). Add `--after` to pick the anchor; otherwise it prepends before the entrypoint target.

Public component (remote OCI):
```
greentic-flow add-step --flow flows/main.ygtc \
  --node-id templates \
  --operation handle_message --payload '{"input":"hi"}' \
  --component oci://ghcr.io/greentic-ai/components/templates:0.1.2 --pin
```
- Sidecar records the remote reference; `--pin` resolves the tag to a digest so future builds are stable.
- Use this when you don’t have the wasm locally or want reproducible pulls in CI.

Using dev_flows (config mode) for schema-valid payloads:
```
greentic-flow add-step --flow flows/main.ygtc --mode config \
  --node-id hello-world \
  --manifest components/hello-world/component.manifest.json \
  --after start
```
- Runs the component’s `dev_flows.default` config to emit a StepSpec with defaults and placeholder routing.
- You can supply `--answers`/`--answers-file` to answer config questions non-interactively.
- Still requires a source: add `--local-wasm ...` for local builds or `--component ... [--pin]` for remotes.

Anchoring and placement:
- `--after <node>` inserts immediately after that node.
- If omitted, the new node is prepended before the entrypoint target (or first node) and the entrypoint is retargeted to the new node.
- Node IDs come from `--node-id`; collisions get `__2`, `__3`, etc. Placeholder hints are rejected.

Required inputs:
- `--node-id` sets the new node id.
- `--local-wasm` (local) or `--component` (remote) provides the sidecar binding.

Routing flags (no JSON needed):
- Default (no flag): thread to the anchor’s existing routing.
- `--routing-out`: make the new node terminal (`routing: out`).
- `--routing-reply`: reply to origin (`routing: reply`).
- `--routing-next <node>`: route to a specific node.
- `--routing-multi-to a,b`: fan out to multiple nodes.
- `--routing-json <file>`: escape hatch for complex arrays (expert only).
- Config-mode still enforces placeholder semantics internally; you never type the placeholder.

Sidecar expectations:
- `--component` accepts `oci://`, `repo://`, or `store://` references. `oci://` must point to a public registry.
- Local wasm paths are stored as `file://<relative/path>` from the flow directory in the sidecar.
- `--pin` hashes local wasm or resolves remote tags to digests; stored in `*.ygtc.resolve.json`.

Safety/inspection:
- `--dry-run` prints the updated flow without writing; `--validate-only` plans/validates without changing files.

### update-step
Re-materialize an existing node using its sidecar binding. Prefills with current payload; merges answers; preserves routing unless overridden.

```
greentic-flow update-step --flow flows/main.ygtc --step hello \
  --answers '{"input":"hi again"}' --routing-reply
```

Requires a sidecar entry for the node; errors if missing (suggests `bind-component` or re-run add-step). `--non-interactive` merges provided answers/prefill and fails if required fields are still missing. `--operation` can rename the op key. Use `--routing-out`, `--routing-reply`, `--routing-next`, `--routing-multi-to`, or `--routing-json` to override routing.

### delete-step
Remove a node and optionally splice predecessors to its routing.

```
greentic-flow delete-step --flow flows/main.ygtc --step mid \
  [--strategy splice|remove-only] \
  [--if-multiple-predecessors error|splice-all] \
  [--assume-yes] [--write]
```

Default `splice` rewires predecessors that point at the deleted node to the deleted node’s routes (terminal routes drop the edge). Removes the sidecar entry. Errors on multiple predecessors unless `splice-all`.

### bind-component
Attach or repair a sidecar mapping without changing the flow content.

```
greentic-flow bind-component --flow flows/main.ygtc --step hello \
  --local-wasm components/hello-world/target/wasm32-wasip2/release/hello_world.wasm \
  [--pin] [--write]
```

Use when a node exists but its sidecar entry is missing/incorrect.

### doctor
Validate flows against the embedded schema and optional adapter registry.

```
greentic-flow doctor flows/          # recursive over .ygtc files
greentic-flow doctor --json --stdin < flows/main.ygtc
```

Defaults to the embedded `schemas/ygtc.flow.schema.json`. `--json` emits a machine-readable report for one flow; `--registry` enables adapter_resolvable linting.
Also updates the flow’s `*.ygtc.resolve.json` to drop stale node bindings and keep the flow name in sync.

## Output reference
- add-step/update-step/delete-step/bind-component print a summary line; flows are written unless `--dry-run`/`--validate-only`.
- Sidecar (`*.ygtc.resolve.json`): schema_version=1; `nodes.{id}.source` contains `kind` (`local` or `remote`), `path` or `reference`, and optional `digest` when `--pin` is used.
- doctor `--json` output matches `LintJsonOutput` (ok flag, diagnostics, bundle metadata).

## Validation and warnings
- Flows must be YGTc v2 (one op key per node, routing shorthand allowed). Legacy `component.exec` is accepted on read but emitted as v2.
- add-step rejects tool/placeholder outputs, missing NEXT_NODE_PLACEHOLDER (config mode), and missing operations.
- All write paths validate against the schema and routing rules; failures abort without writing.

## CI usage
- Run `ci/local_check.sh` (or `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`) in CI.
- Use `greentic-flow doctor` in pipelines to enforce schema validity on committed flows.
