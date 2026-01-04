use serde_json::{Map, Value};

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

    if let Some(tool) = map.remove("tool") {
        map = unwrap_tool(tool, map)?;
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

    let (component_id, payload) = component.ok_or_else(|| FlowError::Internal {
        message: "node must contain a component key".to_string(),
        location: FlowErrorLocation::at_path("node".to_string()),
    })?;

    let routes = parse_routes(routing.unwrap_or(Value::Array(Vec::new())))?;

    Ok(NormalizedNode {
        component_id,
        pack_alias,
        operation,
        payload,
        routing: routes,
    })
}

fn unwrap_tool(tool: Value, mut existing: Map<String, Value>) -> Result<Map<String, Value>> {
    let tool_map = tool.as_object().ok_or_else(|| FlowError::Internal {
        message: "node.tool must be an object".to_string(),
        location: FlowErrorLocation::at_path("node.tool".to_string()),
    })?;

    let component_id = tool_map
        .get("component")
        .and_then(Value::as_str)
        .ok_or_else(|| FlowError::Internal {
            message: "node.tool missing component".to_string(),
            location: FlowErrorLocation::at_path("node.tool.component".to_string()),
        })?;

    if existing.contains_key(component_id) {
        return Err(FlowError::Internal {
            message: format!("node already contains component '{component_id}'"),
            location: FlowErrorLocation::at_path("node".to_string()),
        });
    }

    let mut payload = Map::new();
    let mut pack_alias: Option<Value> = None;
    let mut operation: Option<Value> = None;
    for (k, v) in tool_map {
        match k.as_str() {
            "component" => {}
            "pack_alias" => {
                let alias = v.as_str().ok_or_else(|| FlowError::Internal {
                    message: "node.tool.pack_alias must be a string".to_string(),
                    location: FlowErrorLocation::at_path("node.tool.pack_alias".to_string()),
                })?;
                pack_alias = Some(Value::String(alias.to_string()));
            }
            "operation" => {
                let op = v.as_str().ok_or_else(|| FlowError::Internal {
                    message: "node.tool.operation must be a string".to_string(),
                    location: FlowErrorLocation::at_path("node.tool.operation".to_string()),
                })?;
                operation = Some(Value::String(op.to_string()));
            }
            _ => {
                payload.insert(k.clone(), v.clone());
            }
        }
    }

    existing.insert(component_id.to_string(), Value::Object(payload));
    if let Some(alias) = pack_alias {
        existing.insert("pack_alias".to_string(), alias);
    }
    if let Some(op) = operation {
        existing.insert("operation".to_string(), op);
    }

    Ok(existing)
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
