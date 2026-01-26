use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::{
    component_catalog::ComponentCatalog, config_flow::run_config_flow, error::Result,
    loader::load_ygtc_from_str_with_schema,
};

use super::normalize::normalize_node_map;

#[derive(Debug, Clone)]
pub enum AddStepModeInput {
    Default {
        operation: String,
        payload: Value,
        routing: Option<Value>,
    },
    Config {
        config_flow: String,
        schema_path: Box<Path>,
        answers: Map<String, Value>,
        manifest_id: Option<String>,
        manifest_path: Option<PathBuf>,
    },
}

pub fn materialize_node(
    mode: AddStepModeInput,
    _catalog: &dyn ComponentCatalog,
) -> Result<(Option<String>, Value)> {
    match mode {
        AddStepModeInput::Default {
            operation,
            payload,
            routing,
        } => {
            let mut node = serde_json::Map::new();
            node.insert(operation.clone(), payload);
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
            let normalized = normalize_node_map(value.clone())?;
            Ok((Some(normalized.operation.clone()), Value::Object(node)))
        }
        AddStepModeInput::Config {
            config_flow,
            schema_path,
            answers,
            manifest_id,
            manifest_path,
        } => {
            let _doc = load_ygtc_from_str_with_schema(&config_flow, &schema_path)?; // schema validation
            let output = run_config_flow(&config_flow, &schema_path, &answers, manifest_id)?;
            let normalized = normalize_node_map(output.node.clone())?;
            let mut hint = Some(output.node_id.clone());
            if normalized.operation.is_empty() {
                hint = None;
            }
            let _ = manifest_path;
            Ok((hint, output.node))
        }
    }
}
