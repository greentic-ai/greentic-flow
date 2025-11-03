# Greentic Flow

Generic schema, loader, and intermediate representation for YGTC flows composed of self-describing component nodes.

## Quickstart
```rust
use greentic_flow::{loader::load_ygtc_from_str, resolve::resolve_parameters, to_ir};
use std::path::Path;

let yaml = std::fs::read_to_string("fixtures/weather_bot.ygtc")?;
let flow = load_ygtc_from_str(&yaml, Path::new("schemas/ygtc.flow.schema.json"))?;
let ir = to_ir(flow)?;
let node = ir.nodes.get("forecast_weather").unwrap();
let resolved = resolve_parameters(&node.payload_expr, &ir.parameters, "nodes.forecast_weather")?;
# Ok::<_, greentic_flow::error::FlowError>(())
```

## Design Highlights
- JSON Schema (`schemas/ygtc.flow.schema.json`) enforces exactly one component key per node plus optional routing metadata.
- Loader converts YAML documents to `FlowDoc`, validates against the schema, extracts component metadata, and performs basic graph checks.
- IR (`FlowIR`) keeps nodes generic and serde-friendly so runtimes can post-process component payloads while exposing `NodeKind` classification for adapters.
- `NodeKind::Adapter` recognises node component strings shaped as `<namespace>.<adapter>.<operation>` and keeps the trailing segments joined so nested operations are preserved.
- `resolve::resolve_parameters` pre-resolves only `parameters.*` references, leaving other runtime bindings intact.
- `start` is optional; if omitted and an `in` node exists, the loader defaults `start` to `in`.

## Adapter Registry Format
Adapter-backed nodes can be linted against an on-disk catalog that maps `<namespace>.<adapter>` pairs to the operations they expose. The registry is JSON by default, with optional TOML support via the `toml` feature.

```json
{
  "adapters": {
    "messaging.telegram": ["sendMessage", "editMessage"],
    "email.google": ["send", "draft.create"]
  }
}
```

With the registry loaded, the `adapter_resolvable` rule reports any node whose component string cannot be found in the catalog.

## Development
- `cargo fmt --check`
- `cargo clippy -D warnings`
- `cargo test`

Fixtures under `fixtures/` mirror common success and failure scenarios.

## CLI

Run `cargo run --bin ygtc-lint -- <paths>` to validate flows. Example:

```
cargo run --bin ygtc-lint -- fixtures --schema schemas/ygtc.flow.schema.json
```

To enable adapter linting, provide `--registry`:

```
cargo run --bin ygtc-lint -- \
  --schema schemas/ygtc.flow.schema.json \
  --registry tests/data/registry_ok.json \
  tests/data/flow_ok.ygtc
```

The CLI recursively walks any directories provided, only inspecting files with a `.ygtc` extension. Schema validation always runs; adapter checks are additive when a registry is supplied.

## Environment
- `OTEL_EXPORTER_OTLP_ENDPOINT` (default `http://localhost:4317`) targets your collector.
- `RUST_LOG` controls log verbosity; e.g. `greentic_flow=info`.
- `OTEL_RESOURCE_ATTRIBUTES=deployment.environment=dev` tags spans with the active environment.

## Maintenance Notes
- Keep shared primitives flowing through `greentic-types` and `greentic-interfaces`.
- Prefer zero-copy patterns and stay within safe Rust (`#![forbid(unsafe_code)]` is enabled).
- Update the adapter registry fixtures under `tests/data/` when new adapters or operations are introduced.

## Releases & Publishing
- Crate versions are sourced directly from each crate's `Cargo.toml`.
- Every push to `master` compares the previous commit; if a crate version changed, a tag `<crate-name>-v<semver>` is created and pushed automatically.
- The publish workflow runs on the tagged commit and attempts to publish all changed crates to crates.io using `katyo/publish-crates@v2`.
- Publishing is idempotent: if the version already exists on crates.io, the workflow succeeds without error.
