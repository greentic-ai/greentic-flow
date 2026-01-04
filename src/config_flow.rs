use std::path::Path;

use lazy_static::lazy_static;
use regex::Regex;
use serde_json::{Map, Value};

use crate::{
    add_step::normalize::normalize_node_map,
    compile_flow,
    error::{FlowError, FlowErrorLocation, Result},
    loader::load_ygtc_from_str_with_schema,
};

/// Result of executing a config flow: a node identifier and the node object to insert.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigFlowOutput {
    pub node_id: String,
    pub node: Value,
}

/// Execute a minimal, single-pass config-flow harness.
///
/// Supported components:
/// - `questions`: seeds state values from provided answers or defaults.
/// - `template`: renders the template payload, replacing `{{state.key}}` placeholders inside strings.
///
/// The flow ends when a `template` node is executed. Routing follows the first non-out route if
/// present, otherwise stops.
pub fn run_config_flow(
    yaml: &str,
    schema_path: &Path,
    answers: &Map<String, Value>,
) -> Result<ConfigFlowOutput> {
    let doc = load_ygtc_from_str_with_schema(yaml, schema_path)?;
    let flow = compile_flow(doc.clone())?;
    let mut state = answers.clone();

    let mut current = resolve_entry(&doc);
    let mut visited = 0usize;
    while visited < flow.nodes.len().saturating_add(4) {
        visited += 1;
        let node_id = greentic_types::NodeId::new(current.as_str()).map_err(|e| {
            FlowError::InvalidIdentifier {
                kind: "node",
                value: current.clone(),
                detail: e.to_string(),
                location: FlowErrorLocation::at_path(format!("nodes.{current}")),
            }
        })?;
        let node = flow
            .nodes
            .get(&node_id)
            .ok_or_else(|| FlowError::Internal {
                message: format!("node '{current}' missing during config flow execution"),
                location: FlowErrorLocation::at_path(format!("nodes.{current}")),
            })?;

        match node.component.id.as_str() {
            "questions" => {
                apply_questions(&node.input.mapping, &mut state)?;
            }
            "template" => {
                let payload = render_template(&node.input.mapping, &state)?;
                return extract_config_output(payload);
            }
            other => {
                return Err(FlowError::Internal {
                    message: format!("unsupported component '{other}' in config flow"),
                    location: FlowErrorLocation::at_path(format!("nodes.{current}")),
                });
            }
        }

        current = match &node.routing {
            greentic_types::Routing::Next { node_id } => node_id.as_str().to_string(),
            greentic_types::Routing::End | greentic_types::Routing::Reply => {
                return Err(FlowError::Internal {
                    message: "config flow terminated without reaching template node".to_string(),
                    location: FlowErrorLocation::at_path("nodes".to_string()),
                });
            }
            greentic_types::Routing::Branch { .. } | greentic_types::Routing::Custom(_) => {
                return Err(FlowError::Internal {
                    message: "unsupported routing shape in config flow".to_string(),
                    location: FlowErrorLocation::at_path(format!("nodes.{current}.routing")),
                });
            }
        }
    }

    Err(FlowError::Internal {
        message: "config flow exceeded traversal limit".to_string(),
        location: FlowErrorLocation::at_path("nodes".to_string()),
    })
}

fn resolve_entry(doc: &crate::model::FlowDoc) -> String {
    if let Some(start) = &doc.start {
        return start.clone();
    }
    if doc.nodes.contains_key("in") {
        return "in".to_string();
    }
    doc.nodes
        .keys()
        .next()
        .cloned()
        .unwrap_or_else(|| "in".to_string())
}

fn apply_questions(payload: &Value, state: &mut Map<String, Value>) -> Result<()> {
    let fields = payload
        .get("fields")
        .and_then(Value::as_array)
        .ok_or_else(|| FlowError::Internal {
            message: "questions node missing fields array".to_string(),
            location: FlowErrorLocation::at_path("questions.fields".to_string()),
        })?;

    for field in fields {
        let id = field
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| FlowError::Internal {
                message: "questions field missing id".to_string(),
                location: FlowErrorLocation::at_path("questions.fields".to_string()),
            })?;
        if state.contains_key(id) {
            continue;
        }
        if let Some(default) = field.get("default") {
            state.insert(id.to_string(), default.clone());
        } else {
            return Err(FlowError::Internal {
                message: format!("missing answer for '{id}'"),
                location: FlowErrorLocation::at_path(format!("questions.fields.{id}")),
            });
        }
    }
    Ok(())
}

fn render_template(payload: &Value, state: &Map<String, Value>) -> Result<Value> {
    let template_str = payload.as_str().ok_or_else(|| FlowError::Internal {
        message: "template node payload must be a string".to_string(),
        location: FlowErrorLocation::at_path("template".to_string()),
    })?;
    let mut value: Value = serde_json::from_str(template_str).map_err(|e| FlowError::Internal {
        message: format!("template JSON parse error: {e}"),
        location: FlowErrorLocation::at_path("template".to_string()),
    })?;
    substitute_state(&mut value, state)?;
    Ok(value)
}

lazy_static! {
    static ref STATE_RE: Regex = Regex::new(r"^\{\{\s*state\.([A-Za-z_]\w*)\s*\}\}$").unwrap();
}

fn substitute_state(target: &mut Value, state: &Map<String, Value>) -> Result<()> {
    match target {
        Value::String(s) => {
            if let Some(caps) = STATE_RE.captures(s) {
                let key = caps.get(1).unwrap().as_str();
                let val = state.get(key).ok_or_else(|| FlowError::Internal {
                    message: format!("state value for '{key}' not found"),
                    location: FlowErrorLocation::at_path(format!("state.{key}")),
                })?;
                *target = val.clone();
            }
            Ok(())
        }
        Value::Array(items) => {
            for item in items {
                substitute_state(item, state)?;
            }
            Ok(())
        }
        Value::Object(map) => {
            for value in map.values_mut() {
                substitute_state(value, state)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn extract_config_output(value: Value) -> Result<ConfigFlowOutput> {
    let node_id = value
        .get("node_id")
        .and_then(Value::as_str)
        .ok_or_else(|| FlowError::Internal {
            message: "config flow output missing node_id".to_string(),
            location: FlowErrorLocation::at_path("node_id".to_string()),
        })?
        .to_string();
    let node = value
        .get("node")
        .cloned()
        .ok_or_else(|| FlowError::Internal {
            message: "config flow output missing node".to_string(),
            location: FlowErrorLocation::at_path("node".to_string()),
        })
        .and_then(normalize_node_shape)?;
    Ok(ConfigFlowOutput { node_id, node })
}

fn normalize_node_shape(node: Value) -> Result<Value> {
    let normalized = normalize_node_map(node)?;
    let mut map = Map::new();
    map.insert(normalized.component_id.clone(), normalized.payload.clone());
    if let Some(alias) = normalized.pack_alias {
        map.insert("pack_alias".to_string(), Value::String(alias));
    }
    if let Some(op) = normalized.operation {
        map.insert("operation".to_string(), Value::String(op));
    }
    let routing_value =
        serde_json::to_value(&normalized.routing).map_err(|e| FlowError::Internal {
            message: format!("serialize routing: {e}"),
            location: FlowErrorLocation::at_path("node.routing".to_string()),
        })?;
    map.insert("routing".to_string(), routing_value);

    Ok(Value::Object(map))
}
