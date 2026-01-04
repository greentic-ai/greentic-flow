use indexmap::IndexMap;
use serde_json::Value;

use crate::{
    component_catalog::ComponentCatalog,
    error::{FlowError, FlowErrorLocation, Result},
    flow_ir::{ComponentRef, FlowIr, NodeIr, NodeKind, Route},
    splice::NEXT_NODE_PLACEHOLDER,
};

#[derive(Debug, Clone)]
pub struct AddStepSpec {
    pub new_id: String,
    pub after: String,
    pub component_id: String,
    pub pack_alias: Option<String>,
    pub operation: Option<String>,
    pub payload: Value,
    pub routing: Option<Vec<Route>>,
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
    if flow.nodes.contains_key(&spec.new_id) {
        diags.push(Diagnostic {
            code: "ADD_STEP_NODE_EXISTS",
            message: format!("node '{}' already exists", spec.new_id),
            location: Some(format!("nodes.{}", spec.new_id)),
        });
    }
    if !flow.nodes.contains_key(&spec.after) {
        diags.push(Diagnostic {
            code: "ADD_STEP_ANCHOR_MISSING",
            message: format!("anchor node '{}' not found", spec.after),
            location: Some(format!("nodes.{}", spec.after)),
        });
    }
    if catalog.resolve(&spec.component_id).is_none() {
        diags.push(Diagnostic {
            code: "ADD_STEP_COMPONENT_UNKNOWN",
            message: format!("component '{}' not found in catalog", spec.component_id),
            location: Some("add_step.component".to_string()),
        });
    }
    if !diags.is_empty() {
        return Err(diags);
    }

    let anchor = flow.nodes.get(&spec.after).expect("anchor present");
    let anchor_old_routing = anchor.routing.clone();
    let routing = rewrite_placeholder(spec.routing.unwrap_or_default(), &anchor_old_routing);

    let new_node = NodeIr {
        id: spec.new_id.clone(),
        kind: NodeKind::Component(ComponentRef {
            component_id: spec.component_id.clone(),
            pack_alias: spec.pack_alias.clone(),
            operation: spec.operation.clone(),
            payload: spec.payload.clone(),
        }),
        routing,
    };

    Ok(AddStepPlan {
        anchor: spec.after,
        new_node,
        anchor_old_routing,
    })
}

pub fn apply_plan(flow: &FlowIr, plan: AddStepPlan) -> FlowIr {
    let mut nodes: IndexMap<String, NodeIr> = flow.nodes.clone();
    if let Some(anchor) = nodes.get_mut(&plan.anchor) {
        anchor.routing = vec![Route {
            to: Some(plan.new_node.id.clone()),
            ..Route::default()
        }];
    }
    nodes.insert(plan.new_node.id.clone(), plan.new_node);

    FlowIr {
        id: flow.id.clone(),
        kind: flow.kind.clone(),
        entrypoints: flow.entrypoints.clone(),
        nodes,
    }
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

fn rewrite_placeholder(routes: Vec<Route>, fallback: &[Route]) -> Vec<Route> {
    let mut out = Vec::new();
    let mut replaced = false;
    for route in routes {
        if let Some(to) = &route.to
            && to == NEXT_NODE_PLACEHOLDER
        {
            replaced = true;
            out.extend_from_slice(fallback);
            continue;
        }
        out.push(route);
    }
    if !replaced && out.is_empty() {
        out.extend_from_slice(fallback);
    }
    out
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
