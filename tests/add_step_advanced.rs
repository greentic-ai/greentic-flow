use greentic_flow::{
    add_step::{AddStepSpec, apply_and_validate, plan_add_step},
    component_catalog::{ComponentMetadata, MemoryCatalog},
    flow_ir::parse_flow_to_ir,
    splice::NEXT_NODE_PLACEHOLDER,
};
use serde_json::json;

fn catalog_echo() -> MemoryCatalog {
    let mut catalog = MemoryCatalog::default();
    catalog.insert(ComponentMetadata {
        id: "qa.process".to_string(),
        required_fields: Vec::new(),
    });
    catalog.insert(ComponentMetadata {
        id: "ai.greentic.echo".to_string(),
        required_fields: Vec::new(),
    });
    catalog
}

#[test]
fn default_anchor_is_start_and_rewires() {
    let flow = r#"id: main
type: messaging
start: start
nodes:
  start:
    qa.process: {}
    routing:
      - to: a
  a:
    qa.process: {}
    routing:
      - out: true
"#;
    let ir = parse_flow_to_ir(flow).expect("parse");
    let catalog = catalog_echo();

    let spec = AddStepSpec {
        after: None,
        node_id_hint: Some("hello-world".to_string()),
        node: json!({
            "ai.greentic.echo": { "message": "hi" },
            "routing": [ { "to": NEXT_NODE_PLACEHOLDER } ]
        }),
        allow_cycles: false,
        require_placeholder: true,
    };

    let plan = plan_add_step(&ir, spec, &catalog).expect("plan");
    assert_eq!(plan.anchor, "start");
    let updated = apply_and_validate(&ir, plan, &catalog, false).expect("apply");

    let entry = updated.entrypoints.get("default").unwrap();
    assert_eq!(entry, "hello-world");

    let start = updated.nodes.get("start").unwrap();
    assert_eq!(start.routing.len(), 1);
    assert_eq!(start.routing[0].to.as_deref(), Some("a"));

    let inserted = updated.nodes.get("hello-world").unwrap();
    assert_eq!(inserted.routing.len(), 1);
    assert_eq!(inserted.routing[0].to.as_deref(), Some("start"));
}

#[test]
fn deterministic_anchor_without_start() {
    let flow = r#"id: main
type: messaging
nodes:
  b:
    qa.process: {}
    routing:
      - to: end
  a:
    qa.process: {}
    routing:
      - to: b
  end:
    qa.process: {}
    routing:
      - out: true
"#;
    let ir = parse_flow_to_ir(flow).expect("parse");
    let catalog = catalog_echo();

    let spec = AddStepSpec {
        after: None,
        node_id_hint: Some("inserted".to_string()),
        node: json!({
            "ai.greentic.echo": {},
            "routing": [ { "to": NEXT_NODE_PLACEHOLDER } ]
        }),
        allow_cycles: false,
        require_placeholder: true,
    };

    let plan = plan_add_step(&ir, spec, &catalog).expect("plan");
    assert_eq!(plan.anchor, "b", "first node in order should be anchor");
}

#[test]
fn terminal_anchor_routes_expand_placeholder() {
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
    let catalog = catalog_echo();

    let spec = AddStepSpec {
        after: Some("start".to_string()),
        node_id_hint: Some("mid".to_string()),
        node: json!({
            "ai.greentic.echo": {},
            "routing": [ { "to": NEXT_NODE_PLACEHOLDER } ]
        }),
        allow_cycles: false,
        require_placeholder: true,
    };

    let updated = apply_and_validate(
        &ir,
        plan_add_step(&ir, spec, &catalog).unwrap(),
        &catalog,
        false,
    )
    .expect("apply");

    let start = updated.nodes.get("start").unwrap();
    assert_eq!(start.routing.len(), 1);
    assert_eq!(start.routing[0].to.as_deref(), Some("mid"));

    let inserted = updated.nodes.get("mid").unwrap();
    assert_eq!(inserted.routing.len(), 1);
    assert!(inserted.routing[0].out);
}

#[test]
fn multi_route_metadata_preserved() {
    let flow = r#"id: main
type: messaging
start: anchor
nodes:
  anchor:
    qa.process: {}
    routing:
      - status: Ok
        to: ok_path
      - status: Err
        to: err_path
      - reply: true
        to: reply_path
      - out: true
  ok_path:
    qa.process: {}
    routing:
      - out: true
  err_path:
    qa.process: {}
    routing:
      - out: true
  reply_path:
    qa.process: {}
    routing:
      - out: true
"#;
    let ir = parse_flow_to_ir(flow).expect("parse");
    let catalog = catalog_echo();

    let spec = AddStepSpec {
        after: Some("anchor".to_string()),
        node_id_hint: Some("inserted".to_string()),
        node: json!({
            "ai.greentic.echo": {},
            "routing": [ { "to": NEXT_NODE_PLACEHOLDER } ]
        }),
        allow_cycles: false,
        require_placeholder: true,
    };

    let updated = apply_and_validate(
        &ir,
        plan_add_step(&ir, spec, &catalog).unwrap(),
        &catalog,
        false,
    )
    .expect("apply");

    let anchor = updated.nodes.get("anchor").unwrap();
    assert_eq!(anchor.routing.len(), 1);
    assert_eq!(anchor.routing[0].to.as_deref(), Some("inserted"));

    let inserted = updated.nodes.get("inserted").unwrap();
    assert_eq!(inserted.routing.len(), 4);
    assert_eq!(inserted.routing[0].status.as_deref(), Some("Ok"));
    assert_eq!(inserted.routing[0].to.as_deref(), Some("ok_path"));
    assert_eq!(inserted.routing[1].status.as_deref(), Some("Err"));
    assert_eq!(inserted.routing[1].to.as_deref(), Some("err_path"));
    assert!(inserted.routing[2].reply);
    assert_eq!(inserted.routing[2].to.as_deref(), Some("reply_path"));
    assert!(inserted.routing[3].out);
}

#[test]
fn insert_in_middle_of_chain() {
    let flow = r#"id: main
type: messaging
start: a
nodes:
  a:
    qa.process: {}
    routing:
      - to: b
  b:
    qa.process: {}
    routing:
      - to: c
  c:
    qa.process: {}
    routing:
      - out: true
"#;
    let ir = parse_flow_to_ir(flow).expect("parse");
    let catalog = catalog_echo();
    let spec = AddStepSpec {
        after: Some("b".to_string()),
        node_id_hint: Some("mid".to_string()),
        node: json!({
            "ai.greentic.echo": {},
            "routing": [ { "to": NEXT_NODE_PLACEHOLDER } ]
        }),
        allow_cycles: false,
        require_placeholder: true,
    };

    let updated = apply_and_validate(
        &ir,
        plan_add_step(&ir, spec, &catalog).unwrap(),
        &catalog,
        false,
    )
    .expect("apply");

    let b = updated.nodes.get("b").unwrap();
    assert_eq!(b.routing.len(), 1);
    assert_eq!(b.routing[0].to.as_deref(), Some("mid"));

    let mid = updated.nodes.get("mid").unwrap();
    assert_eq!(mid.routing.len(), 1);
    assert_eq!(mid.routing[0].to.as_deref(), Some("c"));
}

#[test]
fn insert_after_terminal_node() {
    let flow = r#"id: main
type: messaging
start: a
nodes:
  a:
    qa.process: {}
    routing:
      - to: b
  b:
    qa.process: {}
    routing:
      - out: true
"#;
    let ir = parse_flow_to_ir(flow).expect("parse");
    let catalog = catalog_echo();
    let spec = AddStepSpec {
        after: Some("b".to_string()),
        node_id_hint: Some("tail".to_string()),
        node: json!({
            "ai.greentic.echo": {},
            "routing": [ { "to": NEXT_NODE_PLACEHOLDER } ]
        }),
        allow_cycles: false,
        require_placeholder: true,
    };

    let updated = apply_and_validate(
        &ir,
        plan_add_step(&ir, spec, &catalog).unwrap(),
        &catalog,
        false,
    )
    .expect("apply");

    let b = updated.nodes.get("b").unwrap();
    assert_eq!(b.routing.len(), 1);
    assert_eq!(b.routing[0].to.as_deref(), Some("tail"));

    let tail = updated.nodes.get("tail").unwrap();
    assert_eq!(tail.routing.len(), 1);
    assert!(tail.routing[0].out);
}

#[test]
fn name_collision_suffixes_deterministically() {
    let flow = r#"id: main
type: messaging
start: start
nodes:
  start:
    qa.process: {}
    routing:
      - to: hello-world
  hello-world:
    qa.process: {}
    routing:
      - out: true
"#;
    let ir = parse_flow_to_ir(flow).expect("parse");
    let catalog = catalog_echo();

    let spec = AddStepSpec {
        after: Some("start".to_string()),
        node_id_hint: Some("hello-world".to_string()),
        node: json!({
            "ai.greentic.echo": {},
            "routing": [ { "to": NEXT_NODE_PLACEHOLDER } ]
        }),
        allow_cycles: false,
        require_placeholder: true,
    };

    let plan = plan_add_step(&ir, spec, &catalog).expect("plan");
    assert_eq!(plan.new_node.id, "hello-world__2");
}

#[test]
fn multi_to_placeholder_expansion() {
    let flow = r#"id: main
type: messaging
start: start
nodes:
  start:
    qa.process: {}
    routing:
      - to: a
      - to: b
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
    let catalog = catalog_echo();

    let spec = AddStepSpec {
        after: Some("start".to_string()),
        node_id_hint: Some("inserted".to_string()),
        node: json!({
            "ai.greentic.echo": {},
            "routing": [ { "to": NEXT_NODE_PLACEHOLDER } ]
        }),
        allow_cycles: false,
        require_placeholder: true,
    };

    let updated = apply_and_validate(
        &ir,
        plan_add_step(&ir, spec, &catalog).unwrap(),
        &catalog,
        false,
    )
    .expect("apply");

    let inserted = updated.nodes.get("inserted").unwrap();
    assert_eq!(inserted.routing.len(), 2);
    assert_eq!(inserted.routing[0].to.as_deref(), Some("a"));
    assert_eq!(inserted.routing[1].to.as_deref(), Some("b"));
}

#[test]
fn missing_placeholder_is_rejected() {
    let flow = r#"id: main
type: messaging
start: start
nodes:
  start:
    qa.process: {}
    routing:
      - to: a
  a:
    qa.process: {}
    routing:
      - out: true
"#;
    let ir = parse_flow_to_ir(flow).expect("parse");
    let catalog = catalog_echo();
    let spec = AddStepSpec {
        after: Some("start".to_string()),
        node_id_hint: Some("inserted".to_string()),
        node: json!({
            "ai.greentic.echo": {},
            "routing": [ { "to": "a" } ]
        }),
        allow_cycles: false,
        require_placeholder: true,
    };

    let plan = plan_add_step(&ir, spec, &catalog);
    assert!(plan.is_err(), "routing without placeholder must fail");
}

#[test]
fn determinism_check() {
    let flow = r#"id: main
type: messaging
start: start
nodes:
  start:
    qa.process: {}
    routing:
      - out: true
"#;
    let catalog = catalog_echo();

    let spec = || AddStepSpec {
        after: Some("start".to_string()),
        node_id_hint: Some("mid".to_string()),
        node: json!({
            "ai.greentic.echo": {},
            "routing": [ { "to": NEXT_NODE_PLACEHOLDER } ]
        }),
        allow_cycles: false,
        require_placeholder: true,
    };

    let ir1 = parse_flow_to_ir(flow).expect("parse first");
    let first = apply_and_validate(
        &ir1,
        plan_add_step(&ir1, spec(), &catalog).unwrap(),
        &catalog,
        false,
    )
    .expect("apply first")
    .to_doc()
    .unwrap();

    let ir2 = parse_flow_to_ir(flow).expect("parse second");
    let second = apply_and_validate(
        &ir2,
        plan_add_step(&ir2, spec(), &catalog).unwrap(),
        &catalog,
        false,
    )
    .expect("apply second")
    .to_doc()
    .unwrap();

    let left = serde_json::to_value(&first).unwrap();
    let right = serde_json::to_value(&second).unwrap();
    assert_eq!(left, right, "add-step must be deterministic");
}

#[test]
fn legacy_tool_output_rejected() {
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
    let catalog = catalog_echo();
    let spec = AddStepSpec {
        after: Some("start".to_string()),
        node_id_hint: Some("bad".to_string()),
        node: json!({
            "tool": { "component": "ai.greentic.echo", "operation": "run" },
            "routing": [ { "to": NEXT_NODE_PLACEHOLDER } ]
        }),
        allow_cycles: false,
        require_placeholder: true,
    };

    let plan = plan_add_step(&ir, spec, &catalog);
    assert!(plan.is_err(), "tool output must be rejected");
}
