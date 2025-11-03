use crate::{
    error::{FlowError, Result},
    model::FlowDoc,
    util::COMP_KEY_RE,
};
use jsonschema::Draft;
use serde_json::Value;
use std::{collections::BTreeMap, path::Path};

fn validate_json(doc: &Value, schema_path: &Path) -> Result<()> {
    let schema_text = std::fs::read_to_string(schema_path)
        .map_err(|e| FlowError::Internal(format!("schema read: {e}")))?;
    let schema: Value = serde_json::from_str(&schema_text)
        .map_err(|e| FlowError::Internal(format!("schema parse: {e}")))?;
    let validator = jsonschema::options()
        .with_draft(Draft::Draft202012)
        .build(&schema)
        .map_err(|e| FlowError::Internal(format!("schema compile: {e}")))?;
    let errors: Vec<String> = validator
        .iter_errors(doc)
        .map(|e| format!("at {}: {}", e.instance_path, e))
        .collect();
    if !errors.is_empty() {
        return Err(FlowError::Schema(errors.join("\n")));
    }
    Ok(())
}

pub fn load_ygtc_from_str(yaml: &str, schema_path: &Path) -> Result<FlowDoc> {
    let v_yaml: serde_yaml_bw::Value = serde_yaml_bw::from_str(yaml)
        .map_err(|e| FlowError::Yaml("string".into(), e.to_string()))?;
    let v_json: Value = serde_json::to_value(&v_yaml)
        .map_err(|e| FlowError::Internal(format!("yaml->json: {e}")))?;
    validate_json(&v_json, schema_path)?;

    let mut flow: FlowDoc = serde_yaml_bw::from_str(yaml)
        .map_err(|e| FlowError::Yaml("string".into(), e.to_string()))?;

    let node_ids: Vec<String> = flow.nodes.keys().cloned().collect();
    for id in &node_ids {
        let node = flow
            .nodes
            .get_mut(id)
            .ok_or_else(|| FlowError::Internal(format!("node '{id}' missing after load")))?;

        let mut component_kv: Option<(String, Value)> = None;
        let mut routing: Option<Value> = None;
        for (key, value) in &node.raw {
            if key == "routing" {
                routing = Some(value.clone());
                continue;
            }
            if component_kv.is_some() {
                return Err(FlowError::NodeComponentShape(id.clone()));
            }
            component_kv = Some((key.clone(), value.clone()));
        }

        let (component_key, payload) =
            component_kv.ok_or_else(|| FlowError::NodeComponentShape(id.clone()))?;
        if !COMP_KEY_RE.is_match(&component_key) {
            return Err(FlowError::BadComponentKey(component_key, id.clone()));
        }

        node.component = component_key;
        node.payload = payload;
        if let Some(value) = routing {
            node.routing = serde_json::from_value(value)
                .map_err(|e| FlowError::Internal(format!("routing decode in node '{id}': {e}")))?;
        }
        node.raw = BTreeMap::new();
    }

    for (from_id, node) in &flow.nodes {
        for route in &node.routing {
            if let Some(to) = &route.to
                && to != "out"
                && !flow.nodes.contains_key(to)
            {
                return Err(FlowError::MissingNode(to.clone(), from_id.clone()));
            }
        }
    }

    if flow.start.is_none() && flow.nodes.contains_key("in") {
        flow.start = Some("in".to_string());
    }

    Ok(flow)
}
