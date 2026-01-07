use serde_json::Value;

use crate::{
    error::{FlowError, FlowErrorLocation, Result},
    flow_ir::Route,
};

#[derive(Debug, Clone)]
pub struct NormalizedNode {
    pub operation: String,
    pub payload: Value,
    pub routing: Vec<Route>,
    pub telemetry: Option<Value>,
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

    let mut op_key: Option<String> = None;
    let mut op_value: Option<Value> = None;
    let mut routing: Option<Value> = None;
    let mut telemetry: Option<Value> = None;

    for (key, val) in map.clone() {
        match key.as_str() {
            "routing" => {
                routing = Some(val.clone());
                map.remove(&key);
            }
            "telemetry" => {
                telemetry = Some(val.clone());
                map.remove(&key);
            }
            _ => {}
        }
    }

    // Legacy path: component.exec + operation
    if map.contains_key("component.exec") {
        let mut op = value
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if op.trim().is_empty()
            && let Some(payload_op) = map
                .get("component.exec")
                .and_then(|v| v.get("operation"))
                .and_then(Value::as_str)
        {
            op = payload_op.to_string();
        }
        if op.trim().is_empty() {
            return Err(FlowError::Internal {
                message: "component.exec requires a non-empty operation".to_string(),
                location: FlowErrorLocation::at_path("node.operation".to_string()),
            });
        }
        let payload = map
            .remove("component.exec")
            .unwrap_or(Value::Object(Default::default()));
        let payload = if let Some(obj) = payload.as_object()
            && obj.contains_key("operation")
        {
            let mut obj = obj.clone();
            obj.remove("operation");
            Value::Object(obj)
        } else {
            payload
        };
        let routes = parse_routes(routing.unwrap_or(Value::Array(Vec::new())))?;
        return Ok(NormalizedNode {
            operation: op,
            payload,
            routing: routes,
            telemetry,
        });
    }

    for (key, val) in map {
        if op_key.is_some() {
            return Err(FlowError::Internal {
                message: "node must have exactly one operation key".to_string(),
                location: FlowErrorLocation::at_path("node".to_string()),
            });
        }
        op_key = Some(key);
        op_value = Some(val);
    }

    // Legacy path: component.exec + operation
    if op_key.is_none() && value.get("component.exec").is_some() {
        let op = value
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if op.trim().is_empty() {
            return Err(FlowError::Internal {
                message: "component.exec requires a non-empty operation".to_string(),
                location: FlowErrorLocation::at_path("node.operation".to_string()),
            });
        }
        let payload = value
            .get("component.exec")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        op_key = Some(op);
        op_value = Some(payload);
    }

    let operation = op_key.ok_or_else(|| FlowError::Internal {
        message: "node must contain exactly one operation key".to_string(),
        location: FlowErrorLocation::at_path("node".to_string()),
    })?;

    let payload = op_value.unwrap_or(Value::Object(Default::default()));

    let routes = parse_routes(routing.unwrap_or(Value::Array(Vec::new())))?;

    Ok(NormalizedNode {
        operation,
        payload,
        routing: routes,
        telemetry,
    })
}

fn parse_routes(raw: Value) -> Result<Vec<Route>> {
    if raw.is_null() {
        return Ok(Vec::new());
    }

    if let Some(shorthand) = raw.as_str() {
        return match shorthand {
            "out" => Ok(vec![Route {
                out: true,
                ..Route::default()
            }]),
            "reply" => Ok(vec![Route {
                reply: true,
                ..Route::default()
            }]),
            other => Err(FlowError::Internal {
                message: format!("unsupported routing shorthand '{other}'"),
                location: FlowErrorLocation::at_path("routing".to_string()),
            }),
        };
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
