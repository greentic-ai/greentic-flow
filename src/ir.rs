use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowIR {
    pub id: String,
    pub flow_type: String,
    pub start: Option<String>,
    pub parameters: Value,
    pub nodes: IndexMap<String, NodeIR>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeIR {
    pub component: String,
    pub payload_expr: Value,
    pub routes: Vec<RouteIR>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteIR {
    pub to: Option<String>,
    pub out: bool,
}
