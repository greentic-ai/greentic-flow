use crate::{ir::NodeKind, ir::classify_node_type, registry::AdapterCatalog};
use greentic_types::Flow;

#[derive(Clone, Debug, Default)]
pub struct AdapterResolvableRule;

impl AdapterResolvableRule {
    pub fn check(flow: &Flow, catalog: &AdapterCatalog) -> Vec<String> {
        let mut errors = Vec::new();
        for (idx, (node_id, node)) in flow.nodes.iter().enumerate() {
            let comp_str = if let Some(op) = &node.component.operation {
                if node.component.id.as_str() == "component.exec" {
                    op.clone()
                } else {
                    format!("{}.{}", node.component.id, op)
                }
            } else {
                node.component.id.to_string()
            };
            match classify_node_type(&comp_str) {
                NodeKind::Adapter {
                    namespace,
                    adapter,
                    operation,
                } => {
                    if !catalog.contains(&namespace, &adapter, &operation) {
                        errors.push(format!(
                            "adapter_resolvable: node #{idx} ('{node_id}') component '{}' missing adapter '{}.{}' operation '{}'",
                            comp_str, namespace, adapter, operation
                        ));
                    }
                }
                NodeKind::Builtin(_) => {}
            }
        }
        errors
    }
}
