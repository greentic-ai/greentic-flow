use std::path::Path;

use serde_json::{Map, Value, json};

use crate::{
    component_catalog::ComponentCatalog, config_flow::run_config_flow, error::Result,
    loader::load_ygtc_from_str_with_schema,
};

use super::normalize::normalize_node_map;

#[derive(Debug, Clone)]
pub enum AddStepModeInput {
    Default {
        component_id: String,
        pack_alias: Option<String>,
        operation: Option<String>,
        payload: Value,
        routing: Option<Value>,
    },
    Config {
        config_flow: String,
        schema_path: Box<Path>,
        answers: Map<String, Value>,
    },
}

pub fn materialize_node(
    mode: AddStepModeInput,
    _catalog: &dyn ComponentCatalog,
) -> Result<(Option<String>, Value)> {
    match mode {
        AddStepModeInput::Default {
            component_id,
            pack_alias,
            operation,
            payload,
            routing,
        } => {
            let mut node = serde_json::Map::new();
            node.insert(component_id.clone(), payload);
            if let Some(alias) = pack_alias {
                node.insert("pack_alias".to_string(), Value::String(alias));
            }
            if let Some(op) = operation {
                node.insert("operation".to_string(), Value::String(op));
            }
            if let Some(routing) = routing {
                node.insert("routing".to_string(), routing);
            } else {
                node.insert(
                    "routing".to_string(),
                    json!([{ "to": crate::splice::NEXT_NODE_PLACEHOLDER }]),
                );
            }
            let value = Value::Object(node.clone());
            // Ensure shape is valid up front.
            let _ = normalize_node_map(value.clone())?;
            Ok((None, value))
        }
        AddStepModeInput::Config {
            config_flow,
            schema_path,
            answers,
        } => {
            let _doc = load_ygtc_from_str_with_schema(&config_flow, &schema_path)?; // schema validation
            let output = run_config_flow(&config_flow, &schema_path, &answers)?;
            let normalized = normalize_node_map(output.node.clone())?;
            let mut hint = Some(output.node_id.clone());
            if normalized.component_id.is_empty() {
                hint = None;
            }
            Ok((hint, output.node))
        }
    }
}
