# Greentic Flow

Human-friendly YGTc v2 flow authoring: create flows, add component steps, keep routing safe, and validate everything with one CLI.

## Why flows?
- **Readable YAML**: node key = node name, one operation key inside, routing shorthand (`out|reply|[...]`).
- **Component-free authoring**: flows stay human; component sources live in a sidecar resolve file.
- **Safe edits**: add/update/delete steps rewrite routing deterministically and validate against the schema.
- **CI-ready**: built-in validator (`doctor`) and binstall-friendly releases.

## Install
- GitHub Releases (binstall-ready): `cargo binstall greentic-flow`
- crates.io (no bundled binaries): `cargo install --locked greentic-flow`
- Direct download: pick the `.tgz` for your target from the latest release and put `greentic-flow` on your `PATH`.

Check the installed CLI version:

```bash
greentic-flow --version
```

## Create your first flow

```bash
greentic-flow new --flow ./hello.ygtc --id hello-flow --type messaging \
  --name "Hello Flow"
```

`new` writes an empty v2 skeleton (`nodes: {}`) so you can start from a clean slate. If you want a ready-to-run “hello” flow, copy the example file we keep in the repo:

```bash
cp docs/examples/hello.ygtc /tmp/hello.ygtc
```

That example (also covered by tests) is small and readable:

```yaml
id: hello-flow
type: messaging
schema_version: 2
start: start
nodes:
  start:
    templating.handlebars:
      text: "Hello from greentic-flow!"
    routing: out
```

## Add a component step (scaffold + build with greentic-component)

First scaffold the component with `greentic-component new --name hello-world --non-interactive` (run it in your desired components directory; it creates `hello-world/` with a manifest), then build it:

```bash
greentic-component new --name hello-world --non-interactive 
greentic-component build --manifest hello-world/component.manifest.json
```

This produces `hello-world/target/wasm32-wasip2/release/hello_world.wasm` and dev_flows with defaults. Then add it to a flow:

```bash
greentic-flow add-step --flow hello.ygtc \
  --node-id hello-world \
  --operation handle_message \
  --payload '{"input":"Hello from hello-world!"}' \
  --routing-out \
  --local-wasm components/hello-world/target/wasm32-wasip2/release/hello_world.wasm
```

This inserts a `hello-world` node (ordering preserved) and writes a sidecar `docs/examples/hello_with_component.ygtc.resolve.json` that binds the node to your local wasm (add `--pin` to hash it). The resulting flow looks like:

Local wasm bindings are stored in the sidecar as `file://<relative/path>` from the flow directory. Relative `--local-wasm` inputs are resolved from your current working directory first.

```yaml
id: hello-component
type: messaging
schema_version: 2
start: hello-world
nodes:
  hello-world:
    handle_message:
      input:
        input: "Hello from hello-world!"
    routing: out
```

## Use public components (remote + pin)

```bash
greentic-flow add-step --flow flows/main.ygtc \
  --node-id templates \
  --operation run --payload '{}' \
  --routing-out \
  --component oci://ghcr.io/greentic-ai/components/templates:0.1.2 \
  --pin
```

The sidecar records the remote reference and resolved digest (`--pin`). Perfect for CI where you want reproducible pulls.

## Update or delete steps safely
- `greentic-flow update-step --flow flows/main.ygtc --step hello --answers '{"input":"hi again"}' --routing-reply`
  - Re-materializes using the sidecar binding, prefills current payload, merges your answers, and rewrites routing to `reply`.
- `greentic-flow delete-step --flow flows/main.ygtc --step mid --strategy splice`
  - Removes `mid` and splices predecessors to the deleted node’s targets; removes the sidecar entry too.

## Wizard and Capability Boundaries
- `greentic-flow` orchestrates wizard calls (`describe -> qa-spec -> apply-answers`) and flow/sidecar updates.
- Capability gating is enforced by the runtime/operator host, not by `greentic-flow`.
- Wizard summaries can display requested/provided capability groups from component `describe` output for operator visibility.
- Wizard mode is `default|setup|update|remove`; legacy `upgrade` is still accepted as a deprecated alias across 0.6.x and will be removed in a future release.

## Validate flows (CI & local)

```
greentic-flow doctor flows/                   # recursive over .ygtc
greentic-flow doctor --json flows/main.ygtc   # machine-readable
```

Uses the embedded `schemas/ygtc.flow.schema.json` by default; add `--registry <adapter_catalog.json>` for adapter linting.

## Deep dives
- CLI details and routing flags: [`docs/cli.md`](docs/cli.md)
- Add-step design and routing rules: [`docs/add_step_design.md`](docs/add_step_design.md)
- Deployment flows: [`docs/deployment-flows.md`](docs/deployment-flows.md)
- Config flow execution: [`docs/add_step_design.md`](docs/add_step_design.md#config-mode)

## Development
- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`

Or run everything: `LOCAL_CHECK_ONLINE=1 ci/local_check.sh`

## Environment
- `OTEL_EXPORTER_OTLP_ENDPOINT` (default `http://localhost:4317`) targets your collector.
- `RUST_LOG` controls log verbosity; e.g. `greentic_flow=info`.
- `OTEL_RESOURCE_ATTRIBUTES=deployment.environment=dev` tags spans with the active environment.

## Maintenance Notes
- Keep shared primitives flowing through `greentic-types` and `greentic-interfaces`.
- Prefer zero-copy patterns and stay within safe Rust (`#![forbid(unsafe_code)]` is enabled).
- Update the adapter registry fixtures under `tests/data/` when new adapters or operations are introduced.
- Dependabot auto-merge is enabled for Cargo updates; repository settings must allow auto-merge and branch protections should list the required checks to gate merges.

## Releases & Publishing
- Crate versions are sourced directly from each crate's `Cargo.toml`.
- Every push to `master` compares the previous commit; if a crate version changed, a tag `<crate-name>-v<semver>` is created and pushed automatically.
- The publish workflow runs on the tagged commit and attempts to publish all changed crates to crates.io using `katyo/publish-crates@v2`.
- Publishing is idempotent: if the version already exists on crates.io, the workflow succeeds without error.
