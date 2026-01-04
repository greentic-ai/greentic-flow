use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

fn default_parameters() -> Value {
    Value::Object(Default::default())
}

fn default_entrypoints() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn default_routing() -> Value {
    Value::Array(Vec::new())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowDoc {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub flow_type: String,
    #[serde(default)]
    pub start: Option<String>,
    #[serde(default = "default_parameters")]
    pub parameters: Value,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_entrypoints")]
    pub entrypoints: BTreeMap<String, Value>,
    pub nodes: BTreeMap<String, NodeDoc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeDoc {
    #[serde(skip_serializing, skip_deserializing, default)]
    pub component: String,
    #[serde(skip_serializing, skip_deserializing, default)]
    pub pack_alias: Option<String>,
    #[serde(skip_serializing, skip_deserializing, default)]
    pub operation: Option<String>,
    #[serde(skip_serializing, skip_deserializing, default)]
    pub payload: Value,
    #[serde(skip_serializing, skip_deserializing, default)]
    pub output: Option<Value>,
    #[serde(default = "default_routing")]
    pub routing: Value,
    #[serde(skip_serializing, skip_deserializing, default)]
    pub telemetry: Option<TelemetryDoc>,
    #[serde(flatten, default)]
    pub raw: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TelemetryDoc {
    #[serde(default)]
    pub span_name: Option<String>,
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
    #[serde(default)]
    pub sampling: Option<String>,
}
