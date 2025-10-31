#![forbid(unsafe_code)]

pub mod error;
pub mod ir;
pub mod lint;
pub mod loader;
pub mod model;
pub mod registry;
pub mod resolve;
pub mod util;

use crate::{
    error::Result,
    ir::{FlowIR, NodeIR, RouteIR},
    model::FlowDoc,
};
use indexmap::IndexMap;

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
