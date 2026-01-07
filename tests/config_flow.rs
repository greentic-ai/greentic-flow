use greentic_flow::{compile_flow, config_flow::run_config_flow, loader::load_ygtc_from_str};
use greentic_types::NodeId;
use serde_json::{Map, Value, json};

#[test]
fn config_flow_loads_and_emits_contract_payload() {
    let yaml = std::fs::read_to_string("tests/data/config_flow.ygtc").unwrap();
    let doc = load_ygtc_from_str(&yaml).unwrap();
    assert_eq!(doc.flow_type, "component-config");

    let flow = compile_flow(doc).unwrap();
    let ask = flow
        .nodes
        .get(&NodeId::new("ask_config").unwrap())
        .expect("ask_config node");
    assert_eq!(ask.component.id.as_str(), "questions");
    assert!(
        ask.input
            .mapping
            .pointer("/fields")
            .and_then(Value::as_array)
            .map(|fields| !fields.is_empty())
            .unwrap_or(false)
    );

    let emit = flow
        .nodes
        .get(&NodeId::new("emit_config").unwrap())
        .expect("emit_config node");
    assert_eq!(emit.component.id.as_str(), "template");
    let template_str = emit
        .input
        .mapping
        .as_str()
        .expect("template payload is a string");
    let rendered: Value =
        serde_json::from_str(template_str).expect("template payload should be valid JSON");
    let node_id = rendered
        .get("node_id")
        .and_then(Value::as_str)
        .expect("node_id present");
    assert_eq!(node_id, "qa_step");
    let node = rendered
        .get("node")
        .and_then(Value::as_object)
        .expect("node object");
    assert!(node.contains_key("qa.process"));
}

#[test]
fn config_flow_harness_substitutes_state() {
    let yaml = std::fs::read_to_string("tests/data/config_flow.ygtc").unwrap();
    let mut answers = Map::new();
    answers.insert("welcome_template".to_string(), json!("Howdy"));
    answers.insert("temperature".to_string(), json!(0.5));

    let output = run_config_flow(
        &yaml,
        std::path::Path::new("schemas/ygtc.flow.schema.json"),
        &answers,
    )
    .unwrap();

    assert_eq!(output.node_id, "qa_step");
    let qa = output
        .node
        .get("qa.process")
        .and_then(Value::as_object)
        .unwrap();
    assert_eq!(qa.get("welcome_template"), Some(&json!("Howdy")));
    assert_eq!(qa.get("temperature"), Some(&json!(0.5)));
}

#[test]
fn config_flow_rejects_tool_nodes() {
    let yaml = r#"id: tool-node
type: component-config
nodes:
  emit_config:
    template: |
      {
        "node_id": "COMPONENT_STEP",
        "node": {
          "tool": {
            "component": "ai.greentic.hello",
            "pack_alias": "my-pack",
            "operation": "process",
            "message": "{{state.message}}",
            "flag": true
          },
          "routing": [
            { "to": "NEXT_NODE_PLACEHOLDER" }
          ]
        }
      }
"#;

    let mut answers = Map::new();
    answers.insert("message".to_string(), json!("hi"));

    let result = run_config_flow(
        yaml,
        std::path::Path::new("schemas/ygtc.flow.schema.json"),
        &answers,
    );
    assert!(result.is_err(), "legacy tool nodes must be rejected");
}

#[test]
fn config_flow_missing_type_defaults() {
    let yaml = r#"id: cfg
start: q
nodes:
  q:
    questions:
      fields:
        - id: message
          default: "hi"
          prompt: "message"
          type: "string"
    routing:
      - to: emit
  emit:
    template: |
      { "node_id": "hello", "node": { "component.exec": { "component": "ai.greentic.hello" }, "operation": "go", "routing": [ { "to": "NEXT_NODE_PLACEHOLDER" } ] } }
"#;

    let answers = Map::new();
    let output = run_config_flow(
        yaml,
        std::path::Path::new("schemas/ygtc.flow.schema.json"),
        &answers,
    )
    .expect("config flow should normalize missing type");

    assert_eq!(output.node_id, "hello");
}
