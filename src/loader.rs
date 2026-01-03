use crate::{
    error::{FlowError, FlowErrorLocation, Result, SchemaErrorDetail},
    model::{FlowDoc, TelemetryDoc},
    path_safety::normalize_under_root,
    util::is_valid_component_key,
};
use jsonschema::Draft;
use serde::Deserialize;
use serde_json::Value;
use serde_yaml_bw::Location as YamlLocation;
use std::{collections::BTreeMap, fs, path::Path};

const INLINE_SOURCE: &str = "<inline>";
const DEFAULT_SCHEMA_LABEL: &str = "https://raw.githubusercontent.com/greentic-ai/greentic-flow/refs/heads/master/schemas/ygtc.flow.schema.json";
const EMBEDDED_SCHEMA: &str = include_str!("../schemas/ygtc.flow.schema.json");

/// Load YGTC YAML from a string using the embedded schema.
pub fn load_ygtc_from_str(yaml: &str) -> Result<FlowDoc> {
    load_with_schema_text(
        yaml,
        EMBEDDED_SCHEMA,
        DEFAULT_SCHEMA_LABEL.to_string(),
        None,
        INLINE_SOURCE,
        None,
    )
}

/// Load YGTC YAML from a file path using the embedded schema.
pub fn load_ygtc_from_path(path: &Path) -> Result<FlowDoc> {
    let content = fs::read_to_string(path).map_err(|e| FlowError::Internal {
        message: format!("failed to read {}: {e}", path.display()),
        location: FlowErrorLocation::at_path(path.display().to_string())
            .with_source_path(Some(path)),
    })?;
    load_with_schema_text(
        &content,
        EMBEDDED_SCHEMA,
        DEFAULT_SCHEMA_LABEL.to_string(),
        None,
        path.display().to_string(),
        Some(path),
    )
}

/// Load YGTC YAML from a string using a schema file on disk.
pub fn load_ygtc_from_str_with_schema(yaml: &str, schema_path: &Path) -> Result<FlowDoc> {
    load_ygtc_from_str_with_source(yaml, schema_path, INLINE_SOURCE)
}

pub fn load_ygtc_from_str_with_source(
    yaml: &str,
    schema_path: &Path,
    source_label: impl Into<String>,
) -> Result<FlowDoc> {
    let schema_root = std::env::current_dir().map_err(|e| FlowError::Internal {
        message: format!("resolve schema root: {e}"),
        location: FlowErrorLocation::at_path(schema_path.display().to_string())
            .with_source_path(Some(schema_path)),
    })?;
    let safe_schema_path =
        normalize_under_root(&schema_root, schema_path).map_err(|e| FlowError::Internal {
            message: format!("schema path validation for {}: {e}", schema_path.display()),
            location: FlowErrorLocation::at_path(schema_path.display().to_string())
                .with_source_path(Some(schema_path)),
        })?;
    let schema_label = safe_schema_path.display().to_string();
    let schema_text = fs::read_to_string(&safe_schema_path).map_err(|e| FlowError::Internal {
        message: format!("schema read from {schema_label}: {e}"),
        location: FlowErrorLocation::at_path(schema_label.clone())
            .with_source_path(Some(&safe_schema_path)),
    })?;
    load_with_schema_text(
        yaml,
        &schema_text,
        schema_label,
        Some(&safe_schema_path),
        source_label,
        None,
    )
}

pub(crate) fn load_with_schema_text(
    yaml: &str,
    schema_text: &str,
    schema_label: impl Into<String>,
    schema_path: Option<&Path>,
    source_label: impl Into<String>,
    source_path: Option<&Path>,
) -> Result<FlowDoc> {
    let schema_label = schema_label.into();
    let source_label = source_label.into();
    let v_yaml: serde_yaml_bw::Value =
        serde_yaml_bw::from_str(yaml).map_err(|e| FlowError::Yaml {
            message: e.to_string(),
            location: yaml_error_location(&source_label, source_path, e.location()),
        })?;
    let v_json: Value = serde_json::to_value(&v_yaml).map_err(|e| FlowError::Internal {
        message: format!("yaml->json: {e}"),
        location: FlowErrorLocation::at_path(source_label.clone()).with_source_path(source_path),
    })?;
    validate_json(
        &v_json,
        schema_text,
        &schema_label,
        schema_path,
        &source_label,
        source_path,
    )?;

    let mut flow: FlowDoc = serde_yaml_bw::from_str(yaml).map_err(|e| FlowError::Yaml {
        message: e.to_string(),
        location: yaml_error_location(&source_label, source_path, e.location()),
    })?;

    let node_ids: Vec<String> = flow.nodes.keys().cloned().collect();
    for id in &node_ids {
        let node = flow.nodes.get_mut(id).ok_or_else(|| FlowError::Internal {
            message: format!("node '{id}' missing after load"),
            location: node_location(&source_label, source_path, id),
        })?;

        let mut component_kv: Option<(String, Value)> = None;
        let mut routing: Option<Value> = None;
        let mut output: Option<Value> = None;
        let mut telemetry: Option<TelemetryDoc> = None;
        let mut pack_alias: Option<String> = None;
        let mut operation: Option<String> = None;

        for (key, value) in &node.raw {
            match key.as_str() {
                "routing" => {
                    routing = Some(value.clone());
                    continue;
                }
                "output" => {
                    output = Some(value.clone());
                    continue;
                }
                "telemetry" => {
                    telemetry =
                        serde_json::from_value(value.clone()).map_err(|e| FlowError::Internal {
                            message: format!("telemetry decode in node '{id}': {e}"),
                            location: node_location(&source_label, source_path, id),
                        })?;
                    continue;
                }
                "pack_alias" => {
                    pack_alias = value.as_str().map(|s| s.to_string());
                    continue;
                }
                "operation" => {
                    operation = value.as_str().map(|s| s.to_string());
                    continue;
                }
                _ => {}
            }

            if component_kv.is_some() {
                return Err(FlowError::NodeComponentShape {
                    node_id: id.clone(),
                    location: node_location(&source_label, source_path, id),
                });
            }
            component_kv = Some((key.clone(), value.clone()));
        }

        let (component_key, payload) =
            component_kv.ok_or_else(|| FlowError::NodeComponentShape {
                node_id: id.clone(),
                location: node_location(&source_label, source_path, id),
            })?;
        if !is_valid_component_key(&component_key) {
            return Err(FlowError::BadComponentKey {
                component: component_key,
                node_id: id.clone(),
                location: node_location(&source_label, source_path, id),
            });
        }

        node.component = component_key;
        node.payload = payload;
        node.pack_alias = pack_alias;
        node.operation = operation;
        node.output = output;
        if let Some(value) = routing {
            node.routing = value;
        }
        node.telemetry = telemetry;
        node.raw = BTreeMap::new();
    }

    for (from_id, node) in &flow.nodes {
        for route in parse_routes(&node.routing, from_id, &source_label, source_path)? {
            if let Some(to) = &route.to
                && to != "out"
                && !flow.nodes.contains_key(to)
            {
                return Err(FlowError::MissingNode {
                    target: to.clone(),
                    node_id: from_id.clone(),
                    location: routing_location(&source_label, source_path, from_id),
                });
            }
        }
    }

    if flow.start.is_none() && flow.nodes.contains_key("in") {
        flow.start = Some("in".to_string());
    }

    Ok(flow)
}

fn parse_routes(
    raw: &Value,
    node_id: &str,
    source_label: &str,
    source_path: Option<&Path>,
) -> Result<Vec<RouteDoc>> {
    if raw.is_null() {
        return Ok(Vec::new());
    }
    serde_json::from_value::<Vec<RouteDoc>>(raw.clone()).map_err(|e| FlowError::Routing {
        node_id: node_id.to_string(),
        message: e.to_string(),
        location: routing_location(source_label, source_path, node_id),
    })
}

#[derive(Debug, Clone, Deserialize)]
struct RouteDoc {
    #[serde(default)]
    pub to: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub out: Option<bool>,
    #[allow(dead_code)]
    #[serde(default)]
    pub status: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub reply: Option<bool>,
}

fn validate_json(
    doc: &Value,
    schema_text: &str,
    schema_label: &str,
    schema_path: Option<&Path>,
    source_label: &str,
    source_path: Option<&Path>,
) -> Result<()> {
    let schema: Value = serde_json::from_str(schema_text).map_err(|e| FlowError::Internal {
        message: format!("schema parse for {schema_label}: {e}"),
        location: FlowErrorLocation::at_path(schema_label.to_string())
            .with_source_path(schema_path),
    })?;
    let validator = jsonschema::options()
        .with_draft(Draft::Draft202012)
        .build(&schema)
        .map_err(|e| FlowError::Internal {
            message: format!("schema compile for {schema_label}: {e}"),
            location: FlowErrorLocation::at_path(schema_label.to_string())
                .with_source_path(schema_path),
        })?;
    let details: Vec<SchemaErrorDetail> = validator
        .iter_errors(doc)
        .map(|e| {
            let pointer = e.instance_path().to_string();
            let pointer = if pointer.is_empty() {
                "/".to_string()
            } else {
                pointer
            };
            SchemaErrorDetail {
                message: e.to_string(),
                location: FlowErrorLocation::at_path(format!("{source_label}{pointer}"))
                    .with_source_path(source_path)
                    .with_json_pointer(Some(pointer.clone())),
            }
        })
        .collect();
    if !details.is_empty() {
        let message = details
            .iter()
            .map(|detail| {
                let where_str = detail
                    .location
                    .describe()
                    .unwrap_or_else(|| source_label.to_string());
                format!("{where_str}: {}", detail.message)
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Err(FlowError::Schema {
            message,
            details,
            location: FlowErrorLocation::at_path(source_label.to_string())
                .with_source_path(source_path),
        });
    }
    Ok(())
}

fn node_location(
    source_label: &str,
    source_path: Option<&Path>,
    node_id: &str,
) -> FlowErrorLocation {
    FlowErrorLocation::at_path(format!("{source_label}::nodes.{node_id}"))
        .with_source_path(source_path)
}

fn routing_location(
    source_label: &str,
    source_path: Option<&Path>,
    node_id: &str,
) -> FlowErrorLocation {
    FlowErrorLocation::at_path(format!("{source_label}::nodes.{node_id}.routing"))
        .with_source_path(source_path)
}

pub(crate) fn yaml_error_location(
    source_label: &str,
    source_path: Option<&Path>,
    loc: Option<YamlLocation>,
) -> FlowErrorLocation {
    if let Some(loc) = loc {
        FlowErrorLocation::at_path_with_position(
            source_label.to_string(),
            Some(loc.line()),
            Some(loc.column()),
        )
        .with_source_path(source_path)
    } else {
        FlowErrorLocation::at_path(source_label.to_string()).with_source_path(source_path)
    }
}
