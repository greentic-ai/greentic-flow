use anyhow::{Result, anyhow};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

pub const META_NAMESPACE: &str = "greentic";

pub fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn ensure_object(value: &mut Option<Value>) -> &mut serde_json::Map<String, Value> {
    if !matches!(value, Some(Value::Object(_))) {
        *value = Some(Value::Object(serde_json::Map::new()));
    }
    match value.as_mut().unwrap() {
        Value::Object(map) => map,
        _ => unreachable!(),
    }
}

fn ensure_child_map<'a>(
    parent: &'a mut serde_json::Map<String, Value>,
    key: &str,
) -> &'a mut serde_json::Map<String, Value> {
    let entry = parent
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    match entry {
        Value::Object(map) => map,
        _ => {
            *entry = Value::Object(serde_json::Map::new());
            match entry {
                Value::Object(map) => map,
                _ => unreachable!(),
            }
        }
    }
}

pub fn ensure_greentic_meta(meta: &mut Option<Value>) -> &mut serde_json::Map<String, Value> {
    let root = ensure_object(meta);
    ensure_child_map(root, META_NAMESPACE)
}

pub fn set_component_entry(
    meta: &mut Option<Value>,
    node_id: &str,
    component_id: &str,
    abi_version: &str,
    digest: Option<&str>,
    exported_ops: &[String],
    contract: Option<&ComponentContractMeta>,
) {
    let greentic = ensure_greentic_meta(meta);
    let components = ensure_child_map(greentic, "components");
    let mut added_at = now_epoch_seconds();
    if let Some(Value::Object(existing)) = components.get(node_id)
        && let Some(Value::Number(number)) = existing.get("added_at")
        && let Some(value) = number.as_u64()
    {
        added_at = value;
    }
    let mut entry = serde_json::Map::new();
    entry.insert(
        "component_id".to_string(),
        Value::String(component_id.to_string()),
    );
    entry.insert(
        "abi_version".to_string(),
        Value::String(abi_version.to_string()),
    );
    if let Some(contract) = contract {
        entry.insert(
            "describe_hash".to_string(),
            Value::String(contract.describe_hash.clone()),
        );
        entry.insert(
            "operation_id".to_string(),
            Value::String(contract.operation_id.clone()),
        );
        entry.insert(
            "schema_hash".to_string(),
            Value::String(contract.schema_hash.clone()),
        );
        if let Some(version) = &contract.component_version {
            entry.insert(
                "component_version".to_string(),
                Value::String(version.clone()),
            );
        }
        if let Some(world) = &contract.world {
            entry.insert("world".to_string(), Value::String(world.clone()));
        }
        if let Some(config_schema_cbor) = &contract.config_schema_cbor {
            entry.insert(
                "config_schema_cbor".to_string(),
                Value::String(config_schema_cbor.clone()),
            );
        }
    }
    if let Some(d) = digest {
        entry.insert("resolved_digest".to_string(), Value::String(d.to_string()));
    }
    entry.insert(
        "exported_ops_seen".to_string(),
        Value::Array(
            exported_ops
                .iter()
                .map(|s| Value::String(s.clone()))
                .collect(),
        ),
    );
    entry.insert(
        "added_at".to_string(),
        Value::Number(serde_json::Number::from(added_at)),
    );
    entry.insert(
        "updated_at".to_string(),
        Value::Number(serde_json::Number::from(now_epoch_seconds())),
    );
    components.insert(node_id.to_string(), Value::Object(entry));
}

pub struct ComponentContractMeta {
    pub describe_hash: String,
    pub operation_id: String,
    pub schema_hash: String,
    pub component_version: Option<String>,
    pub world: Option<String>,
    pub config_schema_cbor: Option<String>,
}

pub fn clear_component_entry(meta: &mut Option<Value>, node_id: &str) {
    let Some(Value::Object(root)) = meta else {
        return;
    };
    let Some(Value::Object(greentic)) = root.get_mut(META_NAMESPACE) else {
        return;
    };
    if let Some(Value::Object(components)) = greentic.get_mut("components") {
        components.remove(node_id);
    }
    if let Some(Value::Object(secrets)) = greentic.get_mut("secrets_hints") {
        secrets.remove(node_id);
    }
    if let Some(Value::Object(bindings)) = greentic.get_mut("bindings_hints") {
        bindings.remove(node_id);
    }
}

pub fn ensure_hints_empty(meta: &mut Option<Value>, node_id: &str) {
    let greentic = ensure_greentic_meta(meta);
    {
        let secrets = ensure_child_map(greentic, "secrets_hints");
        secrets
            .entry(node_id.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
    }
    {
        let bindings = ensure_child_map(greentic, "bindings_hints");
        bindings
            .entry(node_id.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
    }
}

pub fn find_node_for_component(meta: &Option<Value>, component_id: &str) -> Result<String> {
    let Some(Value::Object(root)) = meta else {
        return Err(anyhow!("flow metadata missing; provide --step"));
    };
    let Some(Value::Object(greentic)) = root.get(META_NAMESPACE) else {
        return Err(anyhow!("flow metadata missing; provide --step"));
    };
    let Some(Value::Object(components)) = greentic.get("components") else {
        return Err(anyhow!("flow metadata missing; provide --step"));
    };
    let mut matches = Vec::new();
    for (node_id, entry) in components {
        if let Value::Object(obj) = entry
            && obj
                .get("component_id")
                .and_then(Value::as_str)
                .is_some_and(|id| id == component_id)
        {
            matches.push(node_id.clone());
        }
    }
    match matches.len() {
        0 => Err(anyhow!(
            "no node found for component id '{component_id}'; provide --step"
        )),
        1 => Ok(matches.remove(0)),
        _ => Err(anyhow!(
            "multiple nodes found for component id '{component_id}'; provide --step"
        )),
    }
}
