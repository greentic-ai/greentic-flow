use std::collections::BTreeMap;

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
    pub kind: String,
    pub entrypoints: IndexMap<String, String>,
    pub nodes: IndexMap<String, NodeIr>,
}

#[derive(Debug, Clone)]
pub struct NodeIr {
    pub id: String,
    pub kind: NodeKind,
    pub routing: Vec<Route>,
}

#[derive(Debug, Clone)]
pub enum NodeKind {
    Component(ComponentRef),
    Questions {
        fields: Value,
    },
    Template {
        template: String,
    },
    Other {
        component_id: String,
        payload: Value,
    },
}

#[derive(Debug, Clone)]
pub struct ComponentRef {
    pub component_id: String,
    pub pack_alias: Option<String>,
    pub operation: Option<String>,
    pub payload: Value,
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
        let entrypoints = resolve_entrypoints(&doc);
        let mut nodes = IndexMap::new();
        for (id, node_doc) in doc.nodes {
            let routing = parse_routing(&node_doc, &id)?;
            let kind = match node_doc.component.as_str() {
                "questions" => NodeKind::Questions {
                    fields: node_doc.payload.clone(),
                },
                "template" => {
                    let template =
                        node_doc
                            .payload
                            .as_str()
                            .ok_or_else(|| FlowError::Internal {
                                message: "template node payload must be a string".to_string(),
                                location: FlowErrorLocation::at_path(format!(
                                    "nodes.{id}.template"
                                )),
                            })?;
                    NodeKind::Template {
                        template: template.to_string(),
                    }
                }
                other => NodeKind::Component(ComponentRef {
                    component_id: other.to_string(),
                    pack_alias: node_doc.pack_alias.clone(),
                    operation: node_doc.operation.clone(),
                    payload: node_doc.payload.clone(),
                }),
            };
            nodes.insert(
                id.clone(),
                NodeIr {
                    id: id.clone(),
                    kind,
                    routing,
                },
            );
        }

        Ok(FlowIr {
            id: doc.id,
            kind: doc.flow_type,
            entrypoints,
            nodes,
        })
    }

    pub fn to_doc(&self) -> Result<FlowDoc> {
        let mut nodes: BTreeMap<String, NodeDoc> = BTreeMap::new();
        for (id, node_ir) in &self.nodes {
            let (component, payload, pack_alias, operation, raw) = match &node_ir.kind {
                NodeKind::Component(comp) => {
                    let mut raw = BTreeMap::new();
                    raw.insert(comp.component_id.clone(), comp.payload.clone());
                    if let Some(alias) = &comp.pack_alias {
                        raw.insert("pack_alias".to_string(), Value::String(alias.clone()));
                    }
                    if let Some(op) = &comp.operation {
                        raw.insert("operation".to_string(), Value::String(op.clone()));
                    }
                    (
                        comp.component_id.clone(),
                        comp.payload.clone(),
                        comp.pack_alias.clone(),
                        comp.operation.clone(),
                        raw,
                    )
                }
                NodeKind::Questions { fields } => {
                    let mut raw = BTreeMap::new();
                    raw.insert("questions".to_string(), fields.clone());
                    ("questions".to_string(), fields.clone(), None, None, raw)
                }
                NodeKind::Template { template } => {
                    let mut raw = BTreeMap::new();
                    raw.insert("template".to_string(), Value::String(template.clone()));
                    (
                        "template".to_string(),
                        Value::String(template.clone()),
                        None,
                        None,
                        raw,
                    )
                }
                NodeKind::Other {
                    component_id,
                    payload,
                } => {
                    let mut raw = BTreeMap::new();
                    raw.insert(component_id.clone(), payload.clone());
                    (component_id.clone(), payload.clone(), None, None, raw)
                }
            };

            let routing_value =
                serde_json::to_value(&node_ir.routing).map_err(|e| FlowError::Internal {
                    message: format!("serialize routing for node '{id}': {e}"),
                    location: FlowErrorLocation::at_path(format!("nodes.{id}.routing")),
                })?;

            nodes.insert(
                id.clone(),
                NodeDoc {
                    component,
                    pack_alias,
                    operation,
                    payload,
                    routing: routing_value,
                    output: None,
                    telemetry: None,
                    raw,
                },
            );
        }

        Ok(FlowDoc {
            id: self.id.clone(),
            title: None,
            description: None,
            flow_type: self.kind.clone(),
            start: self.entrypoints.get("default").cloned(),
            parameters: Value::Object(Map::new()),
            tags: Vec::new(),
            entrypoints: BTreeMap::new(),
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

    let routes: Vec<RouteDoc> = if node.routing.is_null() {
        Vec::new()
    } else {
        serde_json::from_value(node.routing.clone()).map_err(|e| FlowError::Internal {
            message: format!("routing decode for node '{node_id}': {e}"),
            location: FlowErrorLocation::at_path(format!("nodes.{node_id}.routing")),
        })?
    };

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
