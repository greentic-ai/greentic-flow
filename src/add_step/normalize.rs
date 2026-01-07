use serde_json::Value;

use crate::{
    error::{FlowError, FlowErrorLocation, Result},
    flow_ir::Route,
    util::is_valid_component_key,
};

#[derive(Debug, Clone)]
pub struct NormalizedNode {
    pub component_id: String,
    pub pack_alias: Option<String>,
    pub operation: Option<String>,
    pub payload: Value,
    pub routing: Vec<Route>,
}

pub fn normalize_node_map(value: Value) -> Result<NormalizedNode> {
    let mut map = value
        .as_object()
        .cloned()
        .ok_or_else(|| FlowError::Internal {
            message: "node must be an object".to_string(),
            location: FlowErrorLocation::at_path("node".to_string()),
        })?;

    if map.contains_key("tool") {
        return Err(FlowError::Internal {
            message: "Legacy tool emission is not supported. Update greentic-component to emit component.exec nodes without tool."
                .to_string(),
            location: FlowErrorLocation::at_path("node.tool".to_string()),
        });
    }

    let mut component: Option<(String, Value)> = None;
    let mut pack_alias: Option<String> = None;
    let mut operation: Option<String> = None;
    let mut routing: Option<Value> = None;

    for (key, val) in map.clone() {
        match key.as_str() {
            "pack_alias" => {
                pack_alias = Some(
                    val.as_str()
                        .ok_or_else(|| FlowError::Internal {
                            message: "pack_alias must be a string".to_string(),
                            location: FlowErrorLocation::at_path("pack_alias".to_string()),
                        })?
                        .to_string(),
                );
                map.remove(&key);
            }
            "operation" => {
                operation = Some(
                    val.as_str()
                        .ok_or_else(|| FlowError::Internal {
                            message: "operation must be a string".to_string(),
                            location: FlowErrorLocation::at_path("operation".to_string()),
                        })?
                        .to_string(),
                );
                map.remove(&key);
            }
            "routing" => {
                routing = Some(val.clone());
                map.remove(&key);
            }
            _ => {}
        }
    }

    for (key, val) in map {
        if component.is_some() {
            return Err(FlowError::Internal {
                message: "node must have exactly one component key".to_string(),
                location: FlowErrorLocation::at_path(format!("nodes.{key}")),
            });
        }
        if !is_valid_component_key(&key) {
            return Err(FlowError::BadComponentKey {
                component: key,
                node_id: "add_step".to_string(),
                location: FlowErrorLocation::at_path("node".to_string()),
            });
        }
        component = Some((key, val));
    }

    let (component_id, mut payload) = component.ok_or_else(|| FlowError::Internal {
        message: "node must contain a component key".to_string(),
        location: FlowErrorLocation::at_path("node".to_string()),
    })?;

    if component_id == "component.exec"
        && operation.is_none()
        && let Some(map) = payload.as_object_mut()
        && let Some(op) = map.get("operation").and_then(Value::as_str)
    {
        let trimmed = op.trim();
        if !trimmed.is_empty() {
            operation = Some(trimmed.to_string());
            map.remove("operation");
        }
    }

    if component_id == "component.exec" && operation.as_deref().unwrap_or("").is_empty() {
        return Err(FlowError::Internal {
            message: "component.exec requires a non-empty operation".to_string(),
            location: FlowErrorLocation::at_path("node.operation".to_string()),
        });
    }

    let routes = parse_routes(routing.unwrap_or(Value::Array(Vec::new())))?;

    Ok(NormalizedNode {
        component_id,
        pack_alias,
        operation,
        payload,
        routing: routes,
    })
}

fn parse_routes(raw: Value) -> Result<Vec<Route>> {
    if raw.is_null() {
        return Ok(Vec::new());
    }

    let arr = raw.as_array().ok_or_else(|| FlowError::Internal {
        message: "routing must be an array".to_string(),
        location: FlowErrorLocation::at_path("routing".to_string()),
    })?;

    let mut routes = Vec::new();
    for entry in arr {
        let obj = entry.as_object().ok_or_else(|| FlowError::Internal {
            message: "routing entries must be objects".to_string(),
            location: FlowErrorLocation::at_path("routing".to_string()),
        })?;
        for key in obj.keys() {
            match key.as_str() {
                "to" | "out" | "status" | "reply" => {}
                other => {
                    return Err(FlowError::Internal {
                        message: format!("unsupported routing key '{other}'"),
                        location: FlowErrorLocation::at_path("routing".to_string()),
                    });
                }
            }
        }
        routes.push(Route {
            to: obj.get("to").and_then(Value::as_str).map(|s| s.to_string()),
            out: obj.get("out").and_then(Value::as_bool).unwrap_or(false),
            status: obj
                .get("status")
                .and_then(Value::as_str)
                .map(|s| s.to_string()),
            reply: obj.get("reply").and_then(Value::as_bool).unwrap_or(false),
        });
    }

    Ok(routes)
}
