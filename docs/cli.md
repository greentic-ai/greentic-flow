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

## Global flags

```
greentic-flow --version
greentic-flow --help
```

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
  --component oci://ghcr.io/greentic-ai/components/hello-world:latest --pin \
  --after start
```
- Runs the component’s `dev_flows.default` config to emit a StepSpec with defaults and placeholder routing.
- If the selected dev_flow defines questions, add-step prompts interactively unless you pass `--answers`/`--answers-file`.
- `--answers`/`--answers-file` accept JSON objects keyed by question id; non-interactive mode fails if required answers are missing.
- Still requires a source: add `--local-wasm ...` for local builds or `--component ... [--pin]` for remotes.
- If you don’t pass `--config-flow` or `--manifest`, config mode reads `component.manifest.json` next to the local wasm or inside the cached remote component.

Question definitions (component manifest):
- `questions.fields` supports `type` (`string`, `bool`, `int`, `choice`), `default`, `required`, and `options` for choices.
- Conditional prompts use `show_if`:
  - Boolean: `"show_if": true|false`
  - Equals: `"show_if": { "id": "mode", "equals": "asset" }`
  - Hidden questions are not asked and are not required.

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
- Relative `--local-wasm` inputs are resolved from your current working directory, then normalized to the flow directory.
- `--pin` hashes local wasm or resolves remote tags to digests; stored in `*.ygtc.resolve.json`.

Wizard mode notes:
- `--wizard-mode` supports `default|setup|update|remove`.
- `--wizard-mode upgrade` is accepted as a deprecated alias for `update` across 0.6.x. The CLI warns on stderr and includes a non-fatal deprecation diagnostic in JSON output.
- `greentic-flow` does not enforce host capability permissions. Enforcement is runtime/operator-owned; this CLI only surfaces capability summaries from `describe` when available.

Safety/inspection:
- `--dry-run` prints the updated flow without writing; `--validate-only` plans/validates without changing files.

### update-step
Re-materialize an existing node using its sidecar binding. Prefills with current payload; merges answers; preserves routing unless overridden.

```
greentic-flow update-step --flow flows/main.ygtc --step hello \
  --answers '{"input":"hi again"}' --routing-reply
```

Requires a sidecar entry for the node; errors if missing (suggests `bind-component` or re-run add-step). `--non-interactive` merges provided answers/prefill and fails if required fields are still missing. `--operation` can rename the op key. Use `--routing-out`, `--routing-reply`, `--routing-next`, `--routing-multi-to`, or `--routing-json` to override routing.

Config mode reads `dev_flows.default` from the component manifest alongside the bound wasm (or cached remote component) to re-materialize the payload before applying overrides.
- If the selected dev_flow defines questions, update-step prompts interactively for missing required values unless `--non-interactive` is set. `show_if` rules are honored.
- Wizard mode names are `default|setup|update|remove`; `upgrade` remains a deprecated alias for `update` in 0.6.x and emits a warning.

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
When a node is bound to a local component that provides `config_schema` in its manifest, the node payload is validated against that schema.

### answers
Emit JSON Schema + example answers for a component operation without prompting.

```
greentic-flow answers --component <oci|path> --operation <op> --name <prefix> [--out-dir <dir>] [--mode default|config]
```

- Reads `component.manifest.json` from the component path or cached remote artifact.
- Selects `dev_flows.<operation>.graph` for questions; `--mode config` uses `dev_flows.custom`.
- Falls back to `dev_flows.default` if the requested flow is missing.
- Writes `<prefix>.schema.json` and `<prefix>.example.json`; example validates against schema.

### doctor-answers
Validate answers JSON against a schema.

```
greentic-flow doctor-answers --schema answers.schema.json --answers answers.json [--json]
```

- Exits 0 when answers validate; exits 1 with validation errors.
- `--json` emits `{ "ok": true|false, "errors": [...] }`.

## Output reference
- add-step/update-step/delete-step/bind-component print a summary line; flows are written unless `--dry-run`/`--validate-only`.
- Sidecar (`*.ygtc.resolve.json`): schema_version=1; `nodes.{id}.source` contains `kind` (`local` or `remote`), `path` or `reference`, and optional `digest` when `--pin` is used.
- doctor `--json` output matches `LintJsonOutput` (ok flag, diagnostics, bundle metadata).
- Wizard JSON outputs include `diagnostics` when a deprecated mode alias is used (for example `upgrade -> update`).

## Validation and warnings
- Flows must be YGTc v2 (one op key per node, routing shorthand allowed). Legacy `component.exec` is accepted on read but emitted as v2.
- add-step rejects tool/placeholder outputs, missing NEXT_NODE_PLACEHOLDER (config mode), and missing operations.
- All write paths validate against the schema and routing rules; failures abort without writing.

## CI usage
- Run `ci/local_check.sh` (or `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`) in CI.
- Use `greentic-flow doctor` in pipelines to enforce schema validity on committed flows.
