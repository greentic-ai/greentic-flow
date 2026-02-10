use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    error::{FlowError, FlowErrorLocation, Result},
    loader::load_ygtc_from_str,
    model::{FlowDoc, NodeDoc},
};

/// Typed intermediate representation for flows, suitable for planning edits before
/// rendering back into YGTC YAML.
#[derive(Debug, Clone)]
pub struct FlowIr {
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub kind: String,
    pub start: Option<String>,
    pub parameters: Value,
    pub tags: Vec<String>,
    pub schema_version: Option<u32>,
    pub entrypoints: IndexMap<String, String>,
    pub meta: Option<Value>,
    pub nodes: IndexMap<String, NodeIr>,
}

#[derive(Debug, Clone)]
pub struct NodeIr {
    pub id: String,
    pub operation: String,
    pub payload: Value,
    pub output: Value,
    pub routing: Vec<Route>,
    pub telemetry: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub out: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub reply: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl FlowIr {
    pub fn from_doc(doc: FlowDoc) -> Result<Self> {
        let schema_version = doc.schema_version;
        let entrypoints = resolve_entrypoints(&doc);
        let mut nodes = IndexMap::new();
        for (id, node_doc) in doc.nodes {
            let (operation, payload) = extract_operation(&node_doc, &id)?;
            let routing = parse_routing(&node_doc, &id)?;
            let output = node_doc
                .raw
                .get("output")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            nodes.insert(
                id.clone(),
                NodeIr {
                    id: id.clone(),
                    operation,
                    payload,
                    output,
                    routing,
                    telemetry: node_doc
                        .telemetry
                        .clone()
                        .and_then(|t| serde_json::to_value(t).ok()),
                },
            );
        }

        Ok(FlowIr {
            id: doc.id,
            title: doc.title,
            description: doc.description,
            kind: doc.flow_type,
            start: doc.start,
            parameters: doc.parameters,
            tags: doc.tags,
            schema_version,
            entrypoints,
            meta: doc.meta,
            nodes,
        })
    }

    pub fn to_doc(&self) -> Result<FlowDoc> {
        let mut nodes: IndexMap<String, NodeDoc> = IndexMap::new();
        for (id, node_ir) in &self.nodes {
            let mut raw = IndexMap::new();
            raw.insert(node_ir.operation.clone(), node_ir.payload.clone());
            if !node_ir.output.is_object()
                || !node_ir
                    .output
                    .as_object()
                    .map(|m| m.is_empty())
                    .unwrap_or(false)
            {
                raw.insert("output".to_string(), node_ir.output.clone());
            }
            let routing_value =
                serde_json::to_value(&node_ir.routing).map_err(|e| FlowError::Internal {
                    message: format!("serialize routing for node '{id}': {e}"),
                    location: FlowErrorLocation::at_path(format!("nodes.{id}.routing")),
                })?;
            let routing_yaml = if node_ir.routing.len() == 1
                && node_ir.routing[0].out
                && node_ir.routing[0].to.is_none()
                && !node_ir.routing[0].reply
                && node_ir.routing[0].status.is_none()
            {
                Value::String("out".to_string())
            } else if node_ir.routing.len() == 1
                && node_ir.routing[0].reply
                && node_ir.routing[0].to.is_none()
                && !node_ir.routing[0].out
                && node_ir.routing[0].status.is_none()
            {
                Value::String("reply".to_string())
            } else {
                routing_value
            };
            nodes.insert(
                id.clone(),
                NodeDoc {
                    routing: routing_yaml,
                    telemetry: node_ir
                        .telemetry
                        .as_ref()
                        .and_then(|t| serde_json::from_value(t.clone()).ok()),
                    operation: Some(node_ir.operation.clone()),
                    payload: node_ir.payload.clone(),
                    raw,
                },
            );
        }

        let mut entrypoints = IndexMap::new();
        for (name, target) in &self.entrypoints {
            if name == "default" {
                continue;
            }
            entrypoints.insert(name.clone(), Value::String(target.clone()));
        }

        let start = self
            .entrypoints
            .get("default")
            .cloned()
            .or_else(|| self.start.clone());

        Ok(FlowDoc {
            id: self.id.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            flow_type: self.kind.clone(),
            start,
            parameters: self.parameters.clone(),
            tags: self.tags.clone(),
            schema_version: self.schema_version,
            entrypoints,
            meta: self.meta.clone(),
            nodes,
        })
    }
}

fn resolve_entrypoints(doc: &FlowDoc) -> IndexMap<String, String> {
    let mut entries = IndexMap::new();
    if let Some(start) = &doc.start {
        entries.insert("default".to_string(), start.clone());
    } else if doc.nodes.contains_key("in") {
        entries.insert("default".to_string(), "in".to_string());
    } else if let Some(first) = doc.nodes.keys().next() {
        entries.insert("default".to_string(), first.clone());
    }
    for (k, v) in &doc.entrypoints {
        if let Some(target) = v.as_str() {
            entries.insert(k.clone(), target.to_string());
        }
    }
    entries
}

fn parse_routing(node: &NodeDoc, node_id: &str) -> Result<Vec<Route>> {
    if node.routing.is_null() {
        return Ok(Vec::new());
    }
    if let Some(s) = node.routing.as_str() {
        return match s {
            "out" => Ok(vec![Route {
                out: true,
                ..Route::default()
            }]),
            "reply" => Ok(vec![Route {
                reply: true,
                ..Route::default()
            }]),
            other => Err(FlowError::Routing {
                node_id: node_id.to_string(),
                message: format!("unsupported routing shorthand '{other}'"),
                location: FlowErrorLocation::at_path(format!("nodes.{node_id}.routing")),
            }),
        };
    }
    #[derive(serde::Deserialize)]
    struct RouteDoc {
        #[serde(default)]
        to: Option<String>,
        #[serde(default)]
        out: Option<bool>,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        reply: Option<bool>,
    }

    let routes: Vec<RouteDoc> =
        serde_json::from_value(node.routing.clone()).map_err(|e| FlowError::Internal {
            message: format!("routing decode for node '{node_id}': {e}"),
            location: FlowErrorLocation::at_path(format!("nodes.{node_id}.routing")),
        })?;

    Ok(routes
        .into_iter()
        .map(|r| Route {
            to: r.to,
            out: r.out.unwrap_or(false),
            status: r.status,
            reply: r.reply.unwrap_or(false),
        })
        .collect())
}

/// Helper for tests: load YAML text straight into Flow IR.
pub fn parse_flow_to_ir(yaml: &str) -> Result<FlowIr> {
    let doc = load_ygtc_from_str(yaml)?;
    FlowIr::from_doc(doc)
}

fn extract_operation(node: &NodeDoc, node_id: &str) -> Result<(String, Value)> {
    let reserved = [
        "routing",
        "telemetry",
        "output",
        "retry",
        "timeout",
        "when",
        "annotations",
        "meta",
    ];
    if let Some(exec) = node.raw.get("component.exec") {
        let op = node
            .raw
            .get("operation")
            .and_then(Value::as_str)
            .or(node.operation.as_deref())
            .unwrap_or("");
        if op.trim().is_empty() {
            return Err(FlowError::Internal {
                message: format!("node '{node_id}' missing operation key"),
                location: FlowErrorLocation::at_path(format!("nodes.{node_id}")),
            });
        }
        return Ok((op.to_string(), exec.clone()));
    }
    let mut op_key: Option<String> = None;
    let mut payload: Option<Value> = None;
    for (k, v) in &node.raw {
        if reserved.contains(&k.as_str()) {
            continue;
        }
        if op_key.is_some() {
            return Err(FlowError::Internal {
                message: format!(
                    "node '{node_id}' must have exactly one operation key, found multiple"
                ),
                location: FlowErrorLocation::at_path(format!("nodes.{node_id}")),
            });
        }
        op_key = Some(k.clone());
        payload = Some(v.clone());
    }
    if let (Some(k), Some(v)) = (op_key, payload) {
        return Ok((k, v));
    }

    if let Some(op) = &node.operation {
        return Ok((op.clone(), node.payload.clone()));
    }

    Err(FlowError::Internal {
        message: format!("node '{node_id}' missing operation key"),
        location: FlowErrorLocation::at_path(format!("nodes.{node_id}")),
    })
}
