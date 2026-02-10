pub mod id;
pub mod modes;
pub mod normalize;
pub mod rewire;
pub mod validate;

use indexmap::IndexMap;
use serde_json::Value;
use std::{fs, path::Path};

use crate::{
    component_catalog::ComponentCatalog,
    component_catalog::ManifestCatalog,
    config_flow::run_config_flow,
    error::{FlowError, FlowErrorLocation, Result},
    flow_ir::{FlowIr, NodeIr, Route},
    loader::load_ygtc_from_str,
    model::FlowDoc,
};

use self::{
    id::{generate_node_id, is_placeholder_value},
    normalize::normalize_node_map,
    rewire::{apply_threaded_routing, rewrite_placeholder_routes},
    validate::validate_schema_and_flow,
};

#[derive(Debug, Clone)]
pub struct AddStepSpec {
    pub after: Option<String>,
    pub node_id_hint: Option<String>,
    pub node: Value,
    pub allow_cycles: bool,
    pub require_placeholder: bool,
}

#[derive(Debug, Clone)]
pub struct AddStepPlan {
    pub anchor: String,
    pub new_node: NodeIr,
    pub anchor_old_routing: Vec<Route>,
    pub insert_before_entrypoint: bool,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub code: &'static str,
    pub message: String,
    pub location: Option<String>,
}

fn looks_like_component_id(hint: &str) -> bool {
    let trimmed = hint.trim();
    if trimmed.contains('.') {
        return true;
    }
    let parts: Vec<&str> = trimmed.split('_').filter(|p| !p.is_empty()).collect();
    parts.len() >= 3
}

fn simplify_component_name(raw: &str) -> Option<String> {
    let mut candidate = raw.trim();
    if candidate.is_empty() {
        return None;
    }
    if let Some(last) = candidate.rsplit(['/', '\\']).next() {
        candidate = last;
    }
    if let Some((base, _)) = candidate.split_once('@') {
        candidate = base;
    }
    if let Some((base, _)) = candidate.split_once(':') {
        candidate = base;
    }
    if let Some(last) = candidate.rsplit('.').next() {
        candidate = last;
    }
    let underscore_parts: Vec<&str> = candidate.split('_').filter(|p| !p.is_empty()).collect();
    if underscore_parts.len() >= 3 {
        candidate = underscore_parts[underscore_parts.len() - 1];
    }
    let normalized = candidate.replace('_', "-");
    if normalized.trim().is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn component_name_from_node(node: &Value) -> Option<String> {
    let obj = node.as_object()?;
    if let Some(exec) = obj.get("component.exec")
        && let Some(component) = exec.get("component").and_then(Value::as_str)
    {
        return simplify_component_name(component);
    }
    if let Some(component) = obj.get("component").and_then(Value::as_str) {
        return simplify_component_name(component);
    }
    None
}

pub fn normalize_node_id_hint(hint: Option<String>, node: &Value) -> Option<String> {
    let derived = component_name_from_node(node);
    match (hint.as_deref(), derived) {
        (_, None) => hint,
        (None, Some(name)) => Some(name),
        (Some(existing), Some(name)) => {
            if existing.trim().is_empty()
                || is_placeholder_value(existing)
                || looks_like_component_id(existing)
            {
                return Some(name);
            }
            Some(existing.to_string())
        }
    }
}

pub fn plan_add_step(
    flow: &FlowIr,
    spec: AddStepSpec,
    _catalog: &dyn ComponentCatalog,
) -> std::result::Result<AddStepPlan, Vec<Diagnostic>> {
    let mut diags = Vec::new();

    let anchor_source = match resolve_anchor(flow, spec.after.as_deref()) {
        Ok(anchor) => anchor,
        Err(msg) => {
            diags.push(Diagnostic {
                code: "ADD_STEP_ANCHOR_MISSING",
                message: msg,
                location: Some("nodes".to_string()),
            });
            return Err(diags);
        }
    };
    let mut insert_before_entrypoint = false;
    if spec.after.is_none()
        && let Some((_, target)) = flow.entrypoints.get_index(0)
        && target == &anchor_source
    {
        insert_before_entrypoint = true;
    }
    let anchor = anchor_source;

    if let Some(hint) = spec.node_id_hint.as_deref()
        && is_placeholder_value(hint)
    {
        diags.push(Diagnostic {
            code: "ADD_STEP_NODE_ID_PLACEHOLDER",
            message: format!(
                "Config flow emitted placeholder node id '{hint}'; update greentic-component to emit the component name."
            ),
            location: Some("add_step.node_id".to_string()),
        });
        return Err(diags);
    }

    let normalized = match normalize_node_map(spec.node.clone()) {
        Ok(node) => node,
        Err(e) => {
            diags.push(Diagnostic {
                code: "ADD_STEP_NODE_INVALID",
                message: e.to_string(),
                location: Some("add_step.node".to_string()),
            });
            return Err(diags);
        }
    };

    let anchor_old_routing = if let Some(anchor_node) = flow.nodes.get(&anchor) {
        anchor_node.routing.clone()
    } else if flow.nodes.is_empty() {
        Vec::new()
    } else {
        return Err(vec![Diagnostic {
            code: "ADD_STEP_ANCHOR_MISSING",
            message: format!("anchor node '{}' not found", anchor),
            location: Some("nodes".to_string()),
        }]);
    };

    let hint = spec
        .node_id_hint
        .as_deref()
        .or(Some(normalized.operation.as_str()));
    let new_node_id = generate_node_id(hint, &anchor, flow.nodes.keys().map(|k| k.as_str()));

    let routing = rewrite_placeholder_routes(
        normalized.routing.clone(),
        &anchor_old_routing,
        spec.allow_cycles,
        &anchor,
        spec.require_placeholder,
    )
    .map_err(|msg| {
        vec![Diagnostic {
            code: "ADD_STEP_ROUTING_INVALID",
            message: msg,
            location: Some(format!("nodes.{new_node_id}.routing")),
        }]
    })?;

    if routing.is_empty() {
        return Err(vec![Diagnostic {
            code: "ADD_STEP_ROUTING_MISSING",
            message: "add-step requires at least one routing target; use --routing-* or include routing in config flow output".to_string(),
            location: Some(format!("nodes.{new_node_id}.routing")),
        }]);
    }

    let new_node = NodeIr {
        id: new_node_id.clone(),
        operation: normalized.operation.clone(),
        payload: normalized.payload.clone(),
        output: serde_json::Value::Object(Default::default()),
        routing,
        telemetry: normalized.telemetry.clone(),
    };

    Ok(AddStepPlan {
        anchor,
        new_node,
        anchor_old_routing,
        insert_before_entrypoint,
    })
}

pub fn apply_plan(flow: &FlowIr, plan: AddStepPlan, allow_cycles: bool) -> Result<FlowIr> {
    let mut nodes: IndexMap<String, NodeIr> = flow.nodes.clone();
    if nodes.contains_key(&plan.new_node.id) {
        return Err(FlowError::Internal {
            message: format!("node '{}' already exists", plan.new_node.id),
            location: FlowErrorLocation::at_path(format!("nodes.{}", plan.new_node.id)),
        });
    }

    if nodes.is_empty() {
        let mut entrypoints = IndexMap::new();
        entrypoints.insert("default".to_string(), plan.new_node.id.clone());
        nodes.insert(plan.new_node.id.clone(), plan.new_node);
        return Ok(FlowIr {
            id: flow.id.clone(),
            title: flow.title.clone(),
            description: flow.description.clone(),
            kind: flow.kind.clone(),
            start: flow.start.clone(),
            parameters: flow.parameters.clone(),
            tags: flow.tags.clone(),
            schema_version: flow.schema_version,
            entrypoints,
            meta: flow.meta.clone(),
            nodes,
        });
    }

    if plan.insert_before_entrypoint {
        // Insert new node before the entrypoint target: keep anchor routing, retarget entrypoints.
        let mut new_nodes = IndexMap::new();
        for (id, node) in nodes.into_iter() {
            if id == plan.anchor {
                let mut new_node = plan.new_node.clone();
                new_node.routing = vec![Route {
                    to: Some(plan.anchor.clone()),
                    ..Route::default()
                }];
                new_nodes.insert(new_node.id.clone(), new_node);
            }
            new_nodes.insert(id.clone(), node);
        }

        let mut entrypoints = flow.entrypoints.clone();
        for (_name, target) in entrypoints.iter_mut() {
            if target == &plan.anchor {
                *target = plan.new_node.id.clone();
            }
        }

        return Ok(FlowIr {
            id: flow.id.clone(),
            title: flow.title.clone(),
            description: flow.description.clone(),
            kind: flow.kind.clone(),
            start: flow.start.clone(),
            parameters: flow.parameters.clone(),
            tags: flow.tags.clone(),
            schema_version: flow.schema_version,
            entrypoints,
            meta: flow.meta.clone(),
            nodes: new_nodes,
        });
    }

    let mut reordered = IndexMap::new();
    let mut anchor_found = false;
    for (id, node) in nodes.into_iter() {
        if id == plan.anchor {
            anchor_found = true;
            let mut anchor = node.clone();
            anchor.routing = apply_threaded_routing(
                &plan.new_node.id,
                &plan.anchor_old_routing,
                allow_cycles,
                &plan.anchor,
            )?;
            reordered.insert(id.clone(), anchor);
            reordered.insert(plan.new_node.id.clone(), plan.new_node.clone());
        } else {
            reordered.insert(id.clone(), node);
        }
    }

    if !anchor_found {
        return Err(FlowError::Internal {
            message: format!("anchor '{}' not found", plan.anchor),
            location: FlowErrorLocation::at_path(format!("nodes.{}", plan.anchor)),
        });
    }

    Ok(FlowIr {
        id: flow.id.clone(),
        title: flow.title.clone(),
        description: flow.description.clone(),
        kind: flow.kind.clone(),
        start: flow.start.clone(),
        parameters: flow.parameters.clone(),
        tags: flow.tags.clone(),
        schema_version: flow.schema_version,
        entrypoints: flow.entrypoints.clone(),
        meta: flow.meta.clone(),
        nodes: reordered,
    })
}

pub fn validate_flow(flow: &FlowIr, _catalog: &dyn ComponentCatalog) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    if let Some((name, target)) = flow.entrypoints.get_index(0)
        && !flow.nodes.contains_key(target)
    {
        diags.push(Diagnostic {
            code: "ENTRYPOINT_MISSING",
            message: format!("entrypoint '{}' targets unknown node '{}'", name, target),
            location: Some(format!("entrypoints.{name}")),
        });
    }

    for (id, node) in &flow.nodes {
        for route in &node.routing {
            if let Some(to) = &route.to
                && !flow.nodes.contains_key(to)
            {
                diags.push(Diagnostic {
                    code: "ROUTE_TARGET_MISSING",
                    message: format!("node '{}' routes to unknown node '{}'", id, to),
                    location: Some(format!("nodes.{id}.routing")),
                });
            }
        }
        if node.operation.trim().is_empty() {
            diags.push(Diagnostic {
                code: "OPERATION_REQUIRED",
                message: format!("node '{}' missing operation name", id),
                location: Some(format!("nodes.{id}")),
            });
        }
        if node.payload.is_null() {
            diags.push(Diagnostic {
                code: "PAYLOAD_REQUIRED",
                message: format!("node '{}' payload must not be null", id),
                location: Some(format!("nodes.{id}")),
            });
        }
    }

    diags
}

pub fn diagnostics_to_error(diags: Vec<Diagnostic>) -> Result<()> {
    if diags.is_empty() {
        return Ok(());
    }
    let combined = diags
        .into_iter()
        .map(|d| format!("{}: {}", d.code, d.message))
        .collect::<Vec<_>>()
        .join("; ");
    Err(FlowError::Internal {
        message: combined,
        location: FlowErrorLocation::at_path("add_step".to_string()),
    })
}

fn resolve_anchor(flow: &FlowIr, after: Option<&str>) -> std::result::Result<String, String> {
    if let Some(id) = after {
        if flow.nodes.contains_key(id) {
            return Ok(id.to_string());
        }
        return Err(format!("anchor node '{}' not found", id));
    }

    if flow.nodes.is_empty() {
        // Empty flow: no anchor needed; apply_plan will insert the first node and set entrypoint.
        return Ok(String::new());
    }

    if let Some(entry) = flow.entrypoints.get_index(0) {
        return Ok(entry.1.clone());
    }

    if let Some(first) = flow.nodes.keys().next() {
        return Ok(first.clone());
    }

    Err("flow has no nodes to anchor insertion".to_string())
}

pub fn apply_and_validate(
    flow: &FlowIr,
    plan: AddStepPlan,
    catalog: &dyn ComponentCatalog,
    allow_cycles: bool,
) -> Result<FlowIr> {
    let updated = apply_plan(flow, plan, allow_cycles)?;
    validate_schema_and_flow(&updated, catalog)?;
    Ok(updated)
}

/// Return ordered anchor candidates for UX: entrypoint target first (if present), then remaining nodes in insertion order.
pub fn anchor_candidates(flow: &FlowIr) -> Vec<String> {
    let mut seen = IndexMap::new();
    if let Some((_name, target)) = flow.entrypoints.get_index(0) {
        seen.insert(target.clone(), ());
    }
    for id in flow.nodes.keys() {
        seen.entry(id.clone()).or_insert(());
    }
    seen.keys().cloned().collect()
}

/// Execute a config flow and insert its emitted node into the target flow.
pub fn add_step_from_config_flow(
    flow_yaml: &str,
    config_flow_path: &Path,
    schema_path: &Path,
    manifests: &[impl AsRef<Path>],
    after: Option<String>,
    answers: &serde_json::Map<String, Value>,
    allow_cycles: bool,
) -> Result<FlowDoc> {
    let flow_doc = load_ygtc_from_str(flow_yaml)?;
    let flow_ir = FlowIr::from_doc(flow_doc)?;
    let catalog = ManifestCatalog::load_from_paths(manifests);

    let config_yaml = fs::read_to_string(config_flow_path).map_err(|e| FlowError::Internal {
        message: format!("read config flow {}: {e}", config_flow_path.display()),
        location: FlowErrorLocation::at_path(config_flow_path.display().to_string())
            .with_source_path(Some(config_flow_path)),
    })?;
    let output = run_config_flow(&config_yaml, schema_path, answers, None)?;
    let node_id_hint = normalize_node_id_hint(Some(output.node_id.clone()), &output.node);

    let spec = AddStepSpec {
        after,
        node_id_hint,
        node: output.node.clone(),
        allow_cycles,
        require_placeholder: true,
    };

    let plan =
        plan_add_step(&flow_ir, spec, &catalog).map_err(|diags| {
            match diagnostics_to_error(diags) {
                Ok(_) => FlowError::Internal {
                    message: "add_step diagnostics unexpectedly empty".to_string(),
                    location: FlowErrorLocation::at_path("add_step".to_string()),
                },
                Err(e) => e,
            }
        })?;
    let updated = apply_and_validate(&flow_ir, plan, &catalog, allow_cycles)?;
    updated.to_doc()
}
