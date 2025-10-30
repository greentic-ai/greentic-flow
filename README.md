# Greentic Flow

Generic schema, loader, and intermediate representation for YGTC flows composed of self-describing component nodes.

## Features
- JSON Schema (`schemas/ygtc.flow.schema.json`) enforces exactly one component key per node plus optional routing metadata.
- Loader converts YAML documents to `FlowDoc`, validates against the schema, extracts component metadata, and performs basic graph checks.
- IR (`FlowIR`) keeps nodes generic and serde-friendly so runtimes can post-process component payloads.
- `resolve::resolve_parameters` pre-resolves only `parameters.*` references, leaving other runtime bindings intact.
- `start` is optional; if omitted and an `in` node exists, the loader defaults `start` to `in`.

## Usage
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
