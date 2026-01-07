use std::path::Path;

use greentic_flow::{
    add_step::{add_step_from_config_flow, anchor_candidates},
    flow_ir::FlowIr,
};
use serde_json::Map;
use tempfile::NamedTempFile;

fn write_temp(path_suffix: &str, contents: &str) -> NamedTempFile {
    let file = NamedTempFile::with_suffix(path_suffix).expect("temp file");
    std::fs::write(file.path(), contents).expect("write temp");
    file
}

#[test]
fn add_step_from_config_flow_inserts_and_rewires() {
    let pack_flow = r#"id: main
type: messaging
start: start
nodes:
  start:
    templating.handlebars:
      text: Hello
    routing:
      - out: true
"#;

    let config_flow = r#"id: cfg
type: component-config
start: in
nodes:
  in:
    questions:
      fields: []
    routing:
      - to: emit
  emit:
    template: |
      {
        "node_id": "hello-world",
        "node": {
          "component.exec": {
            "component": "ai.greentic.hello-world",
            "input": { "input": "hi" }
          },
          "operation": "handle_message",
          "routing": [ { "to": "NEXT_NODE_PLACEHOLDER" } ]
        }
      }
"#;

    let manifest = r#"{
  "id": "component.exec",
  "config_schema": { "required": [] }
}"#;
    let templating_manifest = r#"{
  "id": "templating.handlebars",
  "config_schema": { "required": [] }
}"#;

    let config_file = write_temp(".ygtc", config_flow);
    let manifest_file = write_temp(".json", manifest);
    let templating_file = write_temp(".json", templating_manifest);

    let answers = Map::new();
    let updated = add_step_from_config_flow(
        pack_flow,
        config_file.path(),
        Path::new("schemas/ygtc.flow.schema.json"),
        &[manifest_file.path(), templating_file.path()],
        Some("start".to_string()),
        &answers,
        false,
    )
    .expect("apply add-step from config flow");

    let ir = FlowIr::from_doc(updated).expect("to ir");
    let start = ir.nodes.get("start").expect("start node");
    assert_eq!(start.routing.len(), 1);
    assert_eq!(start.routing[0].to.as_deref(), Some("hello-world"));

    let inserted = ir.nodes.get("hello-world").expect("inserted node");
    assert_eq!(inserted.routing.len(), 1);
    assert!(inserted.routing[0].out);
}

#[test]
fn anchor_candidates_orders_entrypoint_first() {
    let flow = r#"id: main
type: messaging
start: b
nodes:
  b:
    templating.handlebars:
      text: B
    routing:
      - to: a
  a:
    templating.handlebars:
      text: A
    routing:
      - out: true
"#;
    let ir = greentic_flow::flow_ir::parse_flow_to_ir(flow).expect("parse");
    let anchors = anchor_candidates(&ir);
    assert_eq!(anchors, vec!["b".to_string(), "a".to_string()]);
}
