use crate::{
    error::{FlowError, FlowErrorLocation, Result},
    loader::yaml_error_location,
};
use serde_yaml_bw::{Mapping, Sequence, Value as YamlValue};

const ROUTING_KEY: &str = "routing";
const TO_KEY: &str = "to";

/// Placeholder route destination used in config flows to mark where the caller should splice.
pub const NEXT_NODE_PLACEHOLDER: &str = "NEXT_NODE_PLACEHOLDER";

/// Insert `new_node` after `after_node_id`, rewiring routing so the anchor points to the new node
/// and the new node inherits the anchor's prior routing (or substitutes any `NEXT_NODE_PLACEHOLDER`
/// hops).
pub fn splice_node_after(
    flow_yaml: &str,
    new_node_id: &str,
    new_node: YamlValue,
    after_node_id: &str,
) -> Result<String> {
    let source_label = "<inline>";
    let mut doc: YamlValue = serde_yaml_bw::from_str(flow_yaml).map_err(|e| FlowError::Yaml {
        message: e.to_string(),
        location: yaml_error_location(source_label, None, e.location()),
    })?;
    let doc_map = doc.as_mapping_mut().ok_or_else(|| FlowError::Internal {
        message: "flow document must be a mapping".to_string(),
        location: FlowErrorLocation::at_path(source_label),
    })?;

    let nodes_map = get_mapping_mut(doc_map, "nodes", "nodes")?;

    let new_id_value = yaml_string(new_node_id);
    if nodes_map.contains_key(&new_id_value) {
        return Err(FlowError::Internal {
            message: format!("node '{new_node_id}' already exists"),
            location: FlowErrorLocation::at_path(format!("nodes.{new_node_id}")),
        });
    }

    let anchor_value = nodes_map
        .get_mut(yaml_string(after_node_id))
        .ok_or_else(|| FlowError::Internal {
            message: format!("node '{after_node_id}' not found"),
            location: FlowErrorLocation::at_path(format!("nodes.{after_node_id}")),
        })?;
    let anchor_map = anchor_value
        .as_mapping_mut()
        .ok_or_else(|| FlowError::Internal {
            message: format!("node '{after_node_id}' must be a mapping"),
            location: FlowErrorLocation::at_path(format!("nodes.{after_node_id}")),
        })?;

    let prior_routes = extract_routing(anchor_map, after_node_id)?;

    anchor_map.insert(
        yaml_string(ROUTING_KEY),
        yaml_sequence(vec![route_to(new_node_id)]),
    );

    let mut new_node_map = new_node
        .as_mapping()
        .cloned()
        .ok_or_else(|| FlowError::Internal {
            message: format!("node '{new_node_id}' must be a mapping"),
            location: FlowErrorLocation::at_path(format!("nodes.{new_node_id}")),
        })?;
    let new_routing_value = new_node_map
        .remove(yaml_string(ROUTING_KEY))
        .map(|value| {
            let routes = to_route_list(value, new_node_id)?;
            Ok(rewrite_placeholder(routes, &prior_routes))
        })
        .transpose()?
        .unwrap_or_else(|| yaml_sequence(prior_routes.clone()));
    new_node_map.insert(yaml_string(ROUTING_KEY), new_routing_value);

    nodes_map.insert(new_id_value, YamlValue::Mapping(new_node_map));

    serde_yaml_bw::to_string(&doc).map_err(|e| FlowError::Internal {
        message: format!("serialize updated flow: {e}"),
        location: FlowErrorLocation::at_path(source_label),
    })
}

fn get_mapping_mut<'a>(parent: &'a mut Mapping, key: &str, path: &str) -> Result<&'a mut Mapping> {
    parent
        .get_mut(yaml_string(key))
        .and_then(YamlValue::as_mapping_mut)
        .ok_or_else(|| FlowError::Internal {
            message: format!("flow missing {path} mapping"),
            location: FlowErrorLocation::at_path(path.to_string()),
        })
}

fn extract_routing(node: &Mapping, node_id: &str) -> Result<Vec<YamlValue>> {
    if let Some(value) = node.get(yaml_string(ROUTING_KEY)) {
        to_route_list(value.clone(), node_id)
    } else {
        Ok(Vec::new())
    }
}

fn to_route_list(value: YamlValue, node_id: &str) -> Result<Vec<YamlValue>> {
    match value {
        YamlValue::Sequence(seq) => Ok(seq),
        YamlValue::Null => Ok(Vec::new()),
        _other => Err(FlowError::Routing {
            node_id: node_id.to_string(),
            message: "routing must be an array".to_string(),
            location: FlowErrorLocation::at_path(format!("nodes.{node_id}.routing")),
        }),
    }
}

fn rewrite_placeholder(routes: Vec<YamlValue>, fallback: &[YamlValue]) -> YamlValue {
    let mut out: Vec<YamlValue> = Vec::new();
    let to_key = yaml_string(TO_KEY);
    let placeholder_value = yaml_string(NEXT_NODE_PLACEHOLDER);
    let mut replaced = false;

    for route in routes {
        let mut consumed_placeholder = false;
        if let Some(map) = route.as_mapping()
            && let Some(to) = map.get(&to_key)
            && to == &placeholder_value
        {
            replaced = true;
            consumed_placeholder = true;
            out.extend(fallback.iter().cloned());
        }
        if !consumed_placeholder {
            out.push(route);
        }
    }

    if !replaced && out.is_empty() && !fallback.is_empty() {
        out.extend(fallback.iter().cloned());
    }

    yaml_sequence(out)
}

fn route_to(node_id: &str) -> YamlValue {
    let mut route = Mapping::new();
    route.insert(yaml_string(TO_KEY), yaml_string(node_id));
    YamlValue::Mapping(route)
}

fn yaml_string(value: &str) -> YamlValue {
    YamlValue::String(value.to_string())
}

fn yaml_sequence(elements: Vec<YamlValue>) -> YamlValue {
    YamlValue::Sequence(Sequence::from(elements))
}
