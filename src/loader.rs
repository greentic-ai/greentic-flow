use crate::{
    component_schema::jsonschema_options_with_base,
    error::{FlowError, FlowErrorLocation, Result, SchemaErrorDetail},
    model::FlowDoc,
    path_safety::normalize_under_root,
};
use serde::Deserialize;
use serde_json::Value;
use serde_yaml_bw::Location as YamlLocation;
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::OnceLock,
};

const INLINE_SOURCE: &str = "<inline>";
const DEFAULT_SCHEMA_LABEL: &str = "https://raw.githubusercontent.com/greentic-ai/greentic-flow/refs/heads/master/schemas/ygtc.flow.schema.json";
const EMBEDDED_SCHEMA: &str = include_str!("../schemas/ygtc.flow.schema.json");

/// Ensure a temporary copy of the embedded flow schema exists and return its path.
pub fn ensure_config_schema_path() -> io::Result<PathBuf> {
    static CONFIG_SCHEMA_PATH: OnceLock<PathBuf> = OnceLock::new();
    if let Some(path) = CONFIG_SCHEMA_PATH.get() {
        return Ok(path.clone());
    }
    let mut path = std::env::temp_dir();
    path.push(format!(
        "greentic-flow-config-schema-{}.json",
        env!("CARGO_PKG_VERSION")
    ));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, EMBEDDED_SCHEMA)?;
    match CONFIG_SCHEMA_PATH.set(path.clone()) {
        Ok(()) => Ok(path),
        Err(_) => Ok(CONFIG_SCHEMA_PATH
            .get()
            .expect("config schema path set")
            .clone()),
    }
}

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
    let safe_schema_path = if schema_path.is_absolute() {
        schema_path.to_path_buf()
    } else {
        normalize_under_root(&schema_root, schema_path).map_err(|e| FlowError::Internal {
            message: format!("schema path validation for {}: {e}", schema_path.display()),
            location: FlowErrorLocation::at_path(schema_path.display().to_string())
                .with_source_path(Some(schema_path)),
        })?
    };
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
    let mut v_yaml: serde_yaml_bw::Value =
        serde_yaml_bw::from_str(yaml).map_err(|e| FlowError::Yaml {
            message: e.to_string(),
            location: yaml_error_location(&source_label, source_path, e.location()),
        })?;
    ensure_nodes_mapping(&mut v_yaml);
    let v_json: Value = serde_json::to_value(&v_yaml).map_err(|e| FlowError::Internal {
        message: format!("yaml->json: {e}"),
        location: FlowErrorLocation::at_path(source_label.clone()).with_source_path(source_path),
    })?;
    let schema_version = v_json
        .get("schema_version")
        .and_then(Value::as_u64)
        .unwrap_or(2);
    let nodes_empty = v_json
        .get("nodes")
        .and_then(Value::as_object)
        .map(|m| m.is_empty())
        .unwrap_or(false);
    let reserved_for_count = [
        "routing",
        "telemetry",
        "output",
        "retry",
        "timeout",
        "when",
        "annotations",
        "meta",
        "operation",
    ];
    let looks_legacy = v_json.get("nodes").and_then(Value::as_object).map(|nodes| {
        nodes.values().any(|n| {
            let (op_count, has_dot_key) = n
                .as_object()
                .map(|obj| {
                    let op_count = obj
                        .keys()
                        .filter(|k| !reserved_for_count.contains(&k.as_str()))
                        .count();
                    let has_dot_key = obj.keys().any(|k| k.contains('.'));
                    (op_count, has_dot_key)
                })
                .unwrap_or((0, false));
            op_count != 1
                || has_dot_key
                || n.get("component.exec").is_some()
                || n.get("operation").is_some()
                || n.get("pack_alias").is_some()
        })
    });
    if v_json.get("type").is_none() {
        return Err(FlowError::Schema {
            message: format!("{source_label}/type: missing required property 'type'"),
            details: vec![SchemaErrorDetail {
                message: "Missing required property 'type'".to_string(),
                location: FlowErrorLocation::at_path(format!("{source_label}/type"))
                    .with_source_path(source_path)
                    .with_json_pointer(Some("/type".to_string())),
            }],
            location: FlowErrorLocation::at_path(source_label.clone())
                .with_source_path(source_path),
        });
    }

    if let Some(nodes) = v_json.get("nodes").and_then(Value::as_object) {
        for id in nodes.keys() {
            let Some(node_val) = nodes.get(id) else {
                continue;
            };
            let Some(obj) = node_val.as_object() else {
                continue;
            };
            let op_count = obj
                .keys()
                .filter(|k| !reserved_for_count.contains(&k.as_str()))
                .count();
            let is_component_exec = obj.contains_key("component.exec");
            let component_combo = is_component_exec && op_count == 2;
            if op_count != 1 && !(component_combo || schema_version < 2) {
                return Err(FlowError::NodeComponentShape {
                    node_id: id.clone(),
                    location: node_location(&source_label, source_path, id),
                });
            }
        }
    }

    if !nodes_empty && schema_version >= 2 && !looks_legacy.unwrap_or(false) {
        validate_json(
            &v_json,
            schema_text,
            &schema_label,
            schema_path,
            &source_label,
            source_path,
        )?;
    }

    let mut flow: FlowDoc = match serde_yaml_bw::from_value(v_yaml.clone()) {
        Ok(doc) => doc,
        Err(e) => {
            validate_json(
                &v_json,
                schema_text,
                &schema_label,
                schema_path,
                &source_label,
                source_path,
            )?;
            return Err(FlowError::Yaml {
                message: e.to_string(),
                location: yaml_error_location(&source_label, source_path, None),
            });
        }
    };
    if flow.schema_version.is_none() {
        flow.schema_version = Some(2);
    }

    let node_ids: Vec<String> = flow.nodes.keys().cloned().collect();
    for id in &node_ids {
        let node = flow.nodes.get_mut(id).ok_or_else(|| FlowError::Internal {
            message: format!("node '{id}' missing after load"),
            location: node_location(&source_label, source_path, id),
        })?;
        let reserved = [
            "routing",
            "telemetry",
            "output",
            "retry",
            "timeout",
            "when",
            "annotations",
            "meta",
            "operation",
        ];
        let op_count = node
            .raw
            .keys()
            .filter(|k| !reserved.contains(&k.as_str()))
            .count();
        let is_component_exec = node.raw.contains_key("component.exec");
        let component_combo = is_component_exec && op_count == 2;
        if op_count != 1 && !(component_combo || flow.schema_version.unwrap_or(1) < 2) {
            return Err(FlowError::NodeComponentShape {
                node_id: id.clone(),
                location: node_location(&source_label, source_path, id),
            });
        }
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
    if let Some(shorthand) = raw.as_str() {
        return match shorthand {
            "out" => Ok(vec![RouteDoc {
                to: Some("out".to_string()),
                out: Some(true),
                status: None,
                reply: None,
            }]),
            "reply" => Ok(vec![RouteDoc {
                to: None,
                out: None,
                status: None,
                reply: Some(true),
            }]),
            other => Err(FlowError::Routing {
                node_id: node_id.to_string(),
                message: format!("invalid routing shorthand '{other}'"),
                location: routing_location(source_label, source_path, node_id),
            }),
        };
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
    let validator = jsonschema_options_with_base(schema_path)
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

fn ensure_nodes_mapping(doc: &mut serde_yaml_bw::Value) {
    let Some(mapping) = doc.as_mapping_mut() else {
        return;
    };
    let nodes_key = serde_yaml_bw::Value::String("nodes".to_string());
    match mapping.get_mut(&nodes_key) {
        Some(existing) => {
            if existing.is_null() {
                *existing = serde_yaml_bw::Value::Mapping(serde_yaml_bw::Mapping::new());
            }
        }
        None => {
            mapping.insert(
                nodes_key,
                serde_yaml_bw::Value::Mapping(serde_yaml_bw::Mapping::new()),
            );
        }
    }
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
