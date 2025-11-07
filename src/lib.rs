//! Downstream runtimes must set the current tenant telemetry context via
//! `greentic_types::telemetry::set_current_tenant_ctx` before executing flows
//! (for example, prior to `FlowEngine::run` in the host runner).
#![forbid(unsafe_code)]

pub mod bundle;
pub mod error;
pub mod ir;
pub mod lint;
pub mod loader;
pub mod model;
pub mod registry;
pub mod resolve;
pub mod util;

pub use bundle::{FlowBundle, FlowBundleVersion};

use crate::{
    error::Result,
    ir::{FlowIR, NodeIR, RouteIR},
    model::FlowDoc,
};
use indexmap::IndexMap;

const EMBEDDED_SCHEMA: &str = include_str!("../schemas/ygtc.flow.schema.json");
const EMBEDDED_SCHEMA_LABEL: &str = "<embedded schema>";
const INLINE_SOURCE_LABEL: &str = "<inline>";

/// Load a flow document from YAML using the embedded schema and return a versioned bundle.
pub fn load_and_validate(flow_yaml: &str) -> Result<FlowBundle> {
    load_and_validate_with_source(flow_yaml, INLINE_SOURCE_LABEL)
}

/// Same as [`load_and_validate`] but lets callers label the source for diagnostics.
pub fn load_and_validate_with_source(
    flow_yaml: &str,
    source_label: impl Into<String>,
) -> Result<FlowBundle> {
    let flow = loader::load_with_schema_text(
        flow_yaml,
        EMBEDDED_SCHEMA,
        EMBEDDED_SCHEMA_LABEL.to_string(),
        source_label,
    )?;
    Ok(FlowBundle::new(flow))
}

/// Convert a `FlowDoc` into its compact intermediate representation.
pub fn to_ir(flow: FlowDoc) -> Result<FlowIR> {
    let mut nodes: IndexMap<String, NodeIR> = IndexMap::new();
    for (id, node) in flow.nodes {
        nodes.insert(
            id,
            NodeIR {
                component: node.component,
                payload_expr: node.payload,
                routes: node
                    .routing
                    .into_iter()
                    .map(|route| RouteIR {
                        to: route.to,
                        out: route.out.unwrap_or(false),
                    })
                    .collect(),
            },
        );
    }

    Ok(FlowIR {
        id: flow.id,
        flow_type: flow.flow_type,
        start: flow.start,
        parameters: flow.parameters,
        nodes,
    })
}
