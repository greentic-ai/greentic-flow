pub mod id;
pub mod modes;
pub mod normalize;
pub mod rewire;
pub mod validate;

use indexmap::IndexMap;
use serde_json::Value;

use crate::{
    component_catalog::ComponentCatalog,
    error::{FlowError, FlowErrorLocation, Result},
    flow_ir::{ComponentRef, FlowIr, NodeIr, NodeKind, Route},
};

use self::{
    id::generate_node_id,
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
}

#[derive(Debug, Clone)]
pub struct AddStepPlan {
    pub anchor: String,
    pub new_node: NodeIr,
    pub anchor_old_routing: Vec<Route>,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub code: &'static str,
    pub message: String,
    pub location: Option<String>,
}

pub fn plan_add_step(
    flow: &FlowIr,
    spec: AddStepSpec,
    catalog: &dyn ComponentCatalog,
) -> std::result::Result<AddStepPlan, Vec<Diagnostic>> {
    let mut diags = Vec::new();

    let anchor = match resolve_anchor(flow, spec.after.as_deref()) {
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

    if let Some(meta) = catalog.resolve(&normalized.component_id) {
        for req in meta.required_fields {
            if normalized.payload.get(&req).is_none() {
                diags.push(Diagnostic {
                    code: "COMPONENT_CONFIG_REQUIRED",
                    message: format!(
                        "component '{}' missing required config '{}'",
                        normalized.component_id, req
                    ),
                    location: Some(format!("nodes.{}.{}", normalized.component_id, req)),
                });
            }
        }
    } else {
        diags.push(Diagnostic {
            code: "ADD_STEP_COMPONENT_UNKNOWN",
            message: format!(
                "component '{}' not found in catalog",
                normalized.component_id
            ),
            location: Some("add_step.component".to_string()),
        });
    }

    if !diags.is_empty() {
        return Err(diags);
    }

    let anchor_node = flow.nodes.get(&anchor).expect("anchor exists");
    let anchor_old_routing = anchor_node.routing.clone();

    let new_node_id = generate_node_id(
        spec.node_id_hint.as_deref(),
        &normalized.component_id,
        normalized.operation.as_deref(),
        normalized.pack_alias.as_deref(),
        &anchor,
        flow.nodes.keys().map(|k| k.as_str()),
    );

    let routing = rewrite_placeholder_routes(
        normalized.routing.clone(),
        &anchor_old_routing,
        spec.allow_cycles,
        &anchor,
    )
    .map_err(|msg| {
        vec![Diagnostic {
            code: "ADD_STEP_ROUTING_INVALID",
            message: msg,
            location: Some(format!("nodes.{new_node_id}.routing")),
        }]
    })?;

    let new_node = NodeIr {
        id: new_node_id.clone(),
        kind: NodeKind::Component(ComponentRef {
            component_id: normalized.component_id.clone(),
            pack_alias: normalized.pack_alias.clone(),
            operation: normalized.operation.clone(),
            payload: normalized.payload.clone(),
        }),
        routing,
    };

    Ok(AddStepPlan {
        anchor,
        new_node,
        anchor_old_routing,
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

    let mut anchor = nodes
        .get(&plan.anchor)
        .cloned()
        .ok_or_else(|| FlowError::Internal {
            message: format!("anchor '{}' not found", plan.anchor),
            location: FlowErrorLocation::at_path(format!("nodes.{}", plan.anchor)),
        })?;

    let anchor_routing = apply_threaded_routing(
        &plan.new_node.id,
        &plan.anchor_old_routing,
        allow_cycles,
        &plan.anchor,
    )?;
    anchor.routing = anchor_routing;
    nodes.insert(plan.anchor.clone(), anchor);
    nodes.insert(plan.new_node.id.clone(), plan.new_node);

    Ok(FlowIr {
        id: flow.id.clone(),
        kind: flow.kind.clone(),
        entrypoints: flow.entrypoints.clone(),
        nodes,
    })
}

pub fn validate_flow(flow: &FlowIr, catalog: &dyn ComponentCatalog) -> Vec<Diagnostic> {
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

        match &node.kind {
            NodeKind::Component(comp) => {
                if comp.payload.is_null() {
                    diags.push(Diagnostic {
                        code: "COMPONENT_PAYLOAD_REQUIRED",
                        message: format!(
                            "component '{}' payload must not be null",
                            comp.component_id
                        ),
                        location: Some(format!("nodes.{id}")),
                    });
                }

                if let Some(meta) = catalog.resolve(&comp.component_id) {
                    for req in meta.required_fields {
                        if comp.payload.get(&req).is_none() {
                            diags.push(Diagnostic {
                                code: "COMPONENT_CONFIG_REQUIRED",
                                message: format!(
                                    "component '{}' missing required config '{}'",
                                    comp.component_id, req
                                ),
                                location: Some(format!("nodes.{id}.{req}")),
                            });
                        }
                    }
                } else {
                    diags.push(Diagnostic {
                        code: "COMPONENT_NOT_FOUND",
                        message: format!("component '{}' not found in catalog", comp.component_id),
                        location: Some(format!("nodes.{id}")),
                    });
                }
            }
            NodeKind::Questions { fields } => {
                if fields.get("fields").is_none() {
                    diags.push(Diagnostic {
                        code: "QUESTIONS_FIELDS_REQUIRED",
                        message: "questions node missing fields".to_string(),
                        location: Some(format!("nodes.{id}.questions.fields")),
                    });
                }
            }
            NodeKind::Template { template } => {
                if template.is_empty() {
                    diags.push(Diagnostic {
                        code: "TEMPLATE_EMPTY",
                        message: "template node payload is empty".to_string(),
                        location: Some(format!("nodes.{id}.template")),
                    });
                }
            }
            NodeKind::Other { .. } => {}
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
