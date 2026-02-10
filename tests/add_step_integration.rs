use std::{env, path::PathBuf};

use greentic_flow::{
    add_step::{AddStepSpec, apply_plan, plan_add_step, validate_flow},
    component_catalog::{ComponentCatalog, ComponentMetadata, ManifestCatalog},
    flow_ir::{FlowIr, NodeIr, Route},
    splice::NEXT_NODE_PLACEHOLDER,
};
use indexmap::indexmap;
use serde_json::{Map, Value, json};

#[test]
fn add_step_with_real_manifest_catalog() {
    let manifest_path = match env::var("ADD_STEP_REAL_MANIFEST") {
        Ok(path) => PathBuf::from(path),
        Err(_) => {
            eprintln!("skip: set ADD_STEP_REAL_MANIFEST to run real-pack integration");
            return;
        }
    };
    let component_id = match env::var("ADD_STEP_REAL_COMPONENT") {
        Ok(id) => id,
        Err(_) => {
            eprintln!("skip: set ADD_STEP_REAL_COMPONENT to run real-pack integration");
            return;
        }
    };

    let catalog = ManifestCatalog::load_from_paths(&[manifest_path]);
    let Some(meta) = catalog.resolve(&component_id) else {
        eprintln!(
            "skip: component '{}' not found in manifest catalog",
            component_id
        );
        return;
    };

    let payload = required_payload(&meta);
    let mut nodes = indexmap::IndexMap::new();
    nodes.insert(
        "start".to_string(),
        NodeIr {
            id: "start".to_string(),
            operation: "op".to_string(),
            payload: payload.clone(),
            output: serde_json::Value::Object(Default::default()),
            routing: vec![Route {
                to: Some("end".to_string()),
                ..Route::default()
            }],
            telemetry: None,
        },
    );
    nodes.insert(
        "end".to_string(),
        NodeIr {
            id: "end".to_string(),
            operation: "op".to_string(),
            payload: payload.clone(),
            output: serde_json::Value::Object(Default::default()),
            routing: vec![Route {
                out: true,
                ..Route::default()
            }],
            telemetry: None,
        },
    );

    let flow = FlowIr {
        id: "real-flow".to_string(),
        title: None,
        description: None,
        kind: "messaging".to_string(),
        start: None,
        parameters: serde_json::Value::Object(Default::default()),
        tags: Vec::new(),
        schema_version: Some(2),
        entrypoints: indexmap! {"default".to_string() => "start".to_string()},
        meta: None,
        nodes,
    };

    let spec = AddStepSpec {
        after: Some("start".to_string()),
        node_id_hint: Some("mid".to_string()),
        node: json!({
            component_id.clone(): payload.clone(),
            "routing": [ { "to": NEXT_NODE_PLACEHOLDER } ],
        }),
        allow_cycles: false,
        require_placeholder: true,
    };

    let plan = match plan_add_step(&flow, spec, &catalog) {
        Ok(plan) => plan,
        Err(diags) => {
            panic!("plan failed: {:?}", diags);
        }
    };

    let updated = apply_plan(&flow, plan, false).expect("apply");
    let diags = validate_flow(&updated, &catalog);
    assert!(
        diags.is_empty(),
        "expected validated flow, got diagnostics: {:?}",
        diags
    );
}

fn required_payload(meta: &ComponentMetadata) -> Value {
    // Build a payload with placeholder strings for each required key.
    let mut map = Map::new();
    for key in &meta.required_fields {
        map.insert(key.clone(), Value::String("placeholder".to_string()));
    }
    Value::Object(map)
}
