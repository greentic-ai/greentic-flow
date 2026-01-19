use std::path::Path;

use serde_json::{Map, Value};

use crate::{
    compile_flow,
    error::{FlowError, FlowErrorLocation, Result},
    loader::load_ygtc_from_str_with_schema,
    template::TemplateRenderer,
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
/// - `template`: renders the template payload with simple Handlebars helpers and `{{state.key}}`
///   interpolation.
///
/// The flow ends when a `template` node is executed. Routing follows the first non-out route if
/// present, otherwise stops.
pub fn run_config_flow(
    yaml: &str,
    schema_path: &Path,
    answers: &Map<String, Value>,
    manifest_id: Option<String>,
) -> Result<ConfigFlowOutput> {
    let normalized_yaml = normalize_config_flow_yaml(yaml)?;
    let doc = load_ygtc_from_str_with_schema(&normalized_yaml, schema_path)?;
    let flow = compile_flow(doc.clone())?;
    let mut state = answers.clone();
    let renderer = TemplateRenderer::new(manifest_id);

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
                let payload = render_template(&node.input.mapping, &state, &renderer, &current)?;
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

/// Load config flow YAML from disk, applying type normalization before execution.
pub fn run_config_flow_from_path(
    path: &Path,
    schema_path: &Path,
    answers: &Map<String, Value>,
    manifest_id: Option<String>,
) -> Result<ConfigFlowOutput> {
    let text = std::fs::read_to_string(path).map_err(|e| FlowError::Internal {
        message: format!("read config flow {}: {e}", path.display()),
        location: FlowErrorLocation::at_path(path.display().to_string())
            .with_source_path(Some(path)),
    })?;
    run_config_flow(&text, schema_path, answers, manifest_id)
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

fn render_template(
    payload: &Value,
    state: &Map<String, Value>,
    renderer: &TemplateRenderer,
    node_id: &str,
) -> Result<Value> {
    let template_str = payload.as_str().ok_or_else(|| FlowError::Internal {
        message: "template node payload must be a string".to_string(),
        location: FlowErrorLocation::at_path("template".to_string()),
    })?;
    renderer.render_json(template_str, state, node_id)
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
        })?;
    if node.get("tool").is_some() {
        return Err(FlowError::Internal {
            message: "Legacy tool emission is not supported. Update greentic-component to emit component.exec nodes without tool."
                .to_string(),
            location: FlowErrorLocation::at_path("node.tool".to_string()),
        });
    }
    if crate::add_step::id::is_placeholder_value(&node_id) {
        return Err(FlowError::Internal {
            message: format!(
                "Config flow emitted placeholder node id '{node_id}'; update greentic-component to emit the component name."
            ),
            location: FlowErrorLocation::at_path("node_id".to_string()),
        });
    }
    Ok(ConfigFlowOutput { node_id, node })
}

fn normalize_config_flow_yaml(yaml: &str) -> Result<String> {
    let mut value: Value = serde_yaml_bw::from_str(yaml).map_err(|e| FlowError::Yaml {
        message: e.to_string(),
        location: FlowErrorLocation::at_path("config_flow".to_string()),
    })?;
    if let Some(map) = value.as_object_mut() {
        match map.get("type") {
            Some(Value::String(_)) => {}
            _ => {
                map.insert(
                    "type".to_string(),
                    Value::String("component-config".to_string()),
                );
            }
        }
    }
    serde_yaml_bw::to_string(&value).map_err(|e| FlowError::Internal {
        message: format!("normalize config flow: {e}"),
        location: FlowErrorLocation::at_path("config_flow".to_string()),
    })
}
