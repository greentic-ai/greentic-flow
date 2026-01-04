use greentic_flow::{
    add_step::{AddStepSpec, apply_and_validate, plan_add_step},
    component_catalog::{ComponentMetadata, MemoryCatalog},
    flow_ir::parse_flow_to_ir,
    splice::NEXT_NODE_PLACEHOLDER,
};
use serde_json::json;

fn catalog_with(id: &str, required: Vec<&str>) -> MemoryCatalog {
    let mut catalog = MemoryCatalog::default();
    catalog.insert(ComponentMetadata {
        id: "qa.process".to_string(),
        required_fields: Vec::new(),
    });
    catalog.insert(ComponentMetadata {
        id: id.to_string(),
        required_fields: required.into_iter().map(|s| s.to_string()).collect(),
    });
    catalog
}

#[test]
fn tool_wrapper_normalized_and_schema_valid() {
    let flow = r#"id: main
type: messaging
start: start
nodes:
  start:
    qa.process: {}
    routing:
      - to: end
  end:
    qa.process: {}
    routing:
      - out: true
"#;
    let ir = parse_flow_to_ir(flow).expect("parse");
    let catalog = catalog_with("ai.greentic.hello", vec![]);

    let spec = AddStepSpec {
        after: Some("start".to_string()),
        node_id_hint: Some("mid".to_string()),
        node: json!({
            "tool": { "component": "ai.greentic.hello", "message": "hi" },
            "routing": [ { "to": NEXT_NODE_PLACEHOLDER } ]
        }),
        allow_cycles: false,
    };

    let plan = plan_add_step(&ir, spec, &catalog).expect("plan");
    let updated = apply_and_validate(&ir, plan, &catalog, false).expect("apply+validate");
    let doc = updated.to_doc().expect("to_doc");
    let inserted = doc.nodes.get("mid").expect("mid node");
    assert!(inserted.raw.contains_key("ai.greentic.hello"));
    assert!(!inserted.raw.contains_key("tool"));
}

#[test]
fn routing_semantics_preserved_on_insertion() {
    let flow = r#"id: main
type: messaging
start: start
nodes:
  start:
    qa.process: {}
    routing:
      - status: ok
        to: good
      - reply: true
  good:
    qa.process: {}
    routing:
      - out: true
"#;
    let ir = parse_flow_to_ir(flow).expect("parse");
    let catalog = catalog_with("ai.greentic.echo", vec![]);

    let spec = AddStepSpec {
        after: Some("start".to_string()),
        node_id_hint: Some("echo_step".to_string()),
        node: json!({
            "ai.greentic.echo": { "message": "hi" },
            "routing": [ { "to": NEXT_NODE_PLACEHOLDER } ]
        }),
        allow_cycles: false,
    };

    let plan = plan_add_step(&ir, spec, &catalog).expect("plan");
    let updated = apply_and_validate(&ir, plan, &catalog, false).expect("apply");

    let start = updated.nodes.get("start").unwrap();
    assert_eq!(start.routing.len(), 1);
    assert_eq!(start.routing[0].to.as_deref(), Some("echo_step"));

    let echo = updated.nodes.get("echo_step").unwrap();
    assert_eq!(echo.routing.len(), 2);
    assert_eq!(echo.routing[0].status.as_deref(), Some("ok"));
    assert_eq!(echo.routing[0].to.as_deref(), Some("good"));
    assert!(echo.routing[1].reply);
}

#[test]
fn deterministic_id_generated_when_hint_placeholder() {
    let flow = r#"id: main
type: messaging
start: start
nodes:
  start:
    qa.process: {}
    routing:
      - out: true
"#;
    let ir = parse_flow_to_ir(flow).expect("parse");
    let catalog = catalog_with("ai.greentic.echo", vec![]);

    let spec = AddStepSpec {
        after: Some("start".to_string()),
        node_id_hint: Some("COMPONENT_STEP".to_string()),
        node: json!({
            "ai.greentic.echo": { "message": "hi" },
            "routing": [ { "to": NEXT_NODE_PLACEHOLDER } ]
        }),
        allow_cycles: false,
    };

    let plan = plan_add_step(&ir, spec, &catalog).expect("plan");
    assert_eq!(plan.new_node.id, "ai_greentic_echo__after__start");
}

#[test]
fn invalid_routing_rejected() {
    let flow = r#"id: main
type: messaging
start: start
nodes:
  start:
    qa.process: {}
    routing:
      - out: true
"#;
    let ir = parse_flow_to_ir(flow).expect("parse");
    let catalog = catalog_with("ai.greentic.echo", vec![]);

    let spec = AddStepSpec {
        after: None,
        node_id_hint: None,
        node: json!({
            "ai.greentic.echo": {},
            "routing": [ { "to": "start", "bad": true } ]
        }),
        allow_cycles: false,
    };

    let plan = plan_add_step(&ir, spec, &catalog);
    assert!(plan.is_err());
}

#[test]
fn default_anchor_used_when_missing() {
    let flow = r#"id: main
type: messaging
nodes:
  entry:
    qa.process: {}
    routing:
      - out: true
"#;
    let ir = parse_flow_to_ir(flow).expect("parse");
    let catalog = catalog_with("ai.greentic.echo", vec![]);

    let spec = AddStepSpec {
        after: None,
        node_id_hint: Some("echo".to_string()),
        node: json!({
            "ai.greentic.echo": {},
            "routing": [ { "to": NEXT_NODE_PLACEHOLDER } ]
        }),
        allow_cycles: false,
    };

    let plan = plan_add_step(&ir, spec, &catalog).expect("plan");
    assert_eq!(plan.anchor, "entry");
}

#[test]
fn placeholder_expands_multi_route() {
    let flow = r#"id: main
type: messaging
start: start
nodes:
  start:
    qa.process: {}
    routing:
      - status: ok
        to: a
      - status: fail
        to: b
  a:
    qa.process: {}
    routing:
      - out: true
  b:
    qa.process: {}
    routing:
      - out: true
"#;
    let ir = parse_flow_to_ir(flow).expect("parse");
    let catalog = catalog_with("ai.greentic.echo", vec![]);

    let spec = AddStepSpec {
        after: Some("start".to_string()),
        node_id_hint: Some("echo".to_string()),
        node: json!({
            "ai.greentic.echo": {},
            "routing": [ { "to": NEXT_NODE_PLACEHOLDER } ]
        }),
        allow_cycles: false,
    };

    let plan = plan_add_step(&ir, spec, &catalog).expect("plan");
    let new_routes = &plan.new_node.routing;
    assert_eq!(new_routes.len(), 2);
    assert_eq!(new_routes[0].to.as_deref(), Some("a"));
    assert_eq!(new_routes[1].to.as_deref(), Some("b"));
}
