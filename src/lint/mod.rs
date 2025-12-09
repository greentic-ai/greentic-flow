mod adapter_resolvable;

pub use adapter_resolvable::AdapterResolvableRule;

use crate::registry::AdapterCatalog;
use greentic_types::{Flow, NodeId};
use serde_json::Value;

/// Run the built-in lint rules that do not require external data.
pub fn lint_builtin_rules(flow: &Flow) -> Vec<String> {
    let mut errors = Vec::new();
    if let Some(Value::String(default_entry)) = flow.entrypoints.get("default") {
        match NodeId::new(default_entry.as_str()) {
            Ok(id) => {
                if !flow.nodes.contains_key(&id) {
                    errors.push(format!(
                        "start_node_exists: start node '{}' not found in nodes",
                        default_entry
                    ));
                }
            }
            Err(e) => errors.push(format!(
                "start_node_exists: invalid start node '{}' ({e})",
                default_entry
            )),
        }
    }
    errors
}

/// Run all lint rules including adapter resolution backed by a catalog.
pub fn lint_with_registry(flow: &Flow, catalog: &AdapterCatalog) -> Vec<String> {
    let mut errors = lint_builtin_rules(flow);
    errors.extend(AdapterResolvableRule::check(flow, catalog));
    errors
}
