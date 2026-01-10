use crate::{
    error::{FlowError, FlowErrorLocation, Result},
    flow_ir::Route,
    splice::NEXT_NODE_PLACEHOLDER,
};

pub fn rewrite_placeholder_routes(
    provided: Vec<Route>,
    fallback: &[Route],
    allow_cycles: bool,
    anchor: &str,
    require_placeholder: bool,
) -> std::result::Result<Vec<Route>, String> {
    let mut out = Vec::new();
    let mut replaced = false;
    for route in provided {
        if let Some(to) = &route.to
            && to == NEXT_NODE_PLACEHOLDER
        {
            replaced = true;
            for f in fallback {
                if !allow_cycles && f.to.as_deref() == Some(anchor) {
                    return Err("routing would introduce a cycle back to anchor".to_string());
                }
                out.push(f.clone());
            }
            continue;
        }
        if !allow_cycles && route.to.as_deref() == Some(anchor) {
            return Err("routing would introduce a cycle back to anchor".to_string());
        }
        out.push(route);
    }

    if !replaced && require_placeholder {
        return Err(
            "Config flow output missing NEXT_NODE_PLACEHOLDER; cannot preserve anchor routing semantics."
                .to_string(),
        );
    }

    if !replaced && require_placeholder {
        for f in fallback {
            if !allow_cycles && f.to.as_deref() == Some(anchor) {
                return Err("routing would introduce a cycle back to anchor".to_string());
            }
        }
        out.extend_from_slice(fallback);
    }
    Ok(out)
}

pub fn apply_threaded_routing(
    new_node_id: &str,
    prior_routes: &[Route],
    allow_cycles: bool,
    anchor: &str,
) -> Result<Vec<Route>> {
    if !allow_cycles {
        for r in prior_routes {
            if r.to.as_deref() == Some(anchor) {
                return Err(FlowError::Routing {
                    node_id: anchor.to_string(),
                    message: "inserting step would create a cycle back to anchor".to_string(),
                    location: FlowErrorLocation::at_path(format!("nodes.{anchor}.routing")),
                });
            }
        }
    }

    Ok(vec![Route {
        to: Some(new_node_id.to_string()),
        ..Route::default()
    }])
}
