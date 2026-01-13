use crate::{
    error::{FlowError, FlowErrorLocation, Result},
    loader,
};
use blake3::Hasher;
use greentic_types::Flow;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

const INLINE_SOURCE_LABEL: &str = "<inline>";
const EMBEDDED_SCHEMA: &str = include_str!("../schemas/ygtc.flow.schema.json");
const DEFAULT_SCHEMA_LABEL: &str = "https://raw.githubusercontent.com/greentic-ai/greentic-flow/refs/heads/master/schemas/ygtc.flow.schema.json";

pub type NodeId = String;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComponentPin {
    pub name: String,
    pub version_req: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeRef {
    pub node_id: String,
    pub component: ComponentPin,
    pub schema_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FlowBundle {
    pub id: String,
    pub kind: String,
    pub entry: String,
    pub yaml: String,
    pub json: Value,
    pub hash_blake3: String,
    pub nodes: Vec<NodeRef>,
}

/// Canonicalize a JSON value by sorting object keys recursively.
pub fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            let mut ordered = serde_json::Map::with_capacity(map.len());
            for key in keys {
                ordered.insert(key.clone(), canonicalize_json(&map[key]));
            }
            Value::Object(ordered)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize_json).collect()),
        _ => value.clone(),
    }
}

/// Compute a lowercase hex-encoded BLAKE3 hash for the provided bytes.
pub fn blake3_hex(bytes: impl AsRef<[u8]>) -> String {
    let mut hasher = Hasher::new();
    hasher.update(bytes.as_ref());
    let hash = hasher.finalize();
    hash.to_hex().to_string()
}

/// Extract component pins from the IR.
pub fn extract_component_pins(flow: &Flow) -> Vec<(NodeId, ComponentPin)> {
    flow.nodes
        .iter()
        .map(|(node_id, node)| {
            let component_name = if node.component.id.as_str() == "component.exec" {
                node.component
                    .operation
                    .clone()
                    .unwrap_or_else(|| "component.exec".to_string())
            } else {
                node.component.id.as_str().to_string()
            };
            (
                node_id.to_string(),
                ComponentPin {
                    name: component_name,
                    version_req: "*".to_string(),
                },
            )
        })
        .collect()
}

/// Load YAML into a canonical [`FlowBundle`] using the embedded schema.
pub fn load_and_validate_bundle(yaml: &str, source: Option<&Path>) -> Result<FlowBundle> {
    load_and_validate_bundle_with_schema_text(
        yaml,
        EMBEDDED_SCHEMA,
        DEFAULT_SCHEMA_LABEL.to_string(),
        None,
        source,
    )
    .map(|(bundle, _)| bundle)
}

pub fn load_and_validate_bundle_with_flow(
    yaml: &str,
    source: Option<&Path>,
) -> Result<(FlowBundle, Flow)> {
    load_and_validate_bundle_with_schema_text(
        yaml,
        EMBEDDED_SCHEMA,
        DEFAULT_SCHEMA_LABEL.to_string(),
        None,
        source,
    )
}

pub fn load_and_validate_bundle_with_schema_text(
    yaml: &str,
    schema_text: &str,
    schema_label: impl Into<String>,
    schema_path: Option<&Path>,
    source: Option<&Path>,
) -> Result<(FlowBundle, Flow)> {
    let schema_label = schema_label.into();
    let source_label = source
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| INLINE_SOURCE_LABEL.to_string());

    let flow_doc = loader::load_with_schema_text(
        yaml,
        schema_text,
        schema_label,
        schema_path,
        source_label.clone(),
        source,
    )?;

    let flow_json = serde_json::to_value(&flow_doc).map_err(|e| FlowError::Internal {
        message: format!("flow serialization: {e}"),
        location: FlowErrorLocation::at_path(source_label.clone()).with_source_path(source),
    })?;
    let canonical_json = canonicalize_json(&flow_json);
    let json_bytes = serde_json::to_vec(&canonical_json).map_err(|e| FlowError::Internal {
        message: format!("canonical json encode: {e}"),
        location: FlowErrorLocation::at_path(source_label.clone()).with_source_path(source),
    })?;
    let hash_blake3 = blake3_hex(&json_bytes);

    let flow = crate::compile_flow(flow_doc.clone())?;
    let bundle = build_bundle_from_parts(&flow_doc, &flow, yaml, canonical_json, hash_blake3);

    Ok((bundle, flow))
}

fn build_bundle_from_parts(
    doc: &crate::model::FlowDoc,
    flow: &Flow,
    yaml: &str,
    canonical_json: Value,
    hash_blake3: String,
) -> FlowBundle {
    let entry = resolve_entry(doc);
    let nodes = extract_component_pins(flow)
        .into_iter()
        .map(|(node_id, component)| NodeRef {
            node_id,
            component,
            schema_id: None,
        })
        .collect();

    FlowBundle {
        id: doc.id.clone(),
        kind: doc.flow_type.clone(),
        entry,
        yaml: yaml.to_string(),
        json: canonical_json,
        hash_blake3,
        nodes,
    }
}

fn resolve_entry(doc: &crate::model::FlowDoc) -> String {
    if let Some(entry) = &doc.start {
        return entry.clone();
    }
    if doc.nodes.contains_key("in") {
        return "in".to_string();
    }
    doc.nodes.keys().next().cloned().unwrap_or_default()
}
