use greentic_flow::{compile_flow, loader::load_ygtc_from_str, resolve::resolve_parameters};
use greentic_types::{FlowKind, NodeId};
use serde_json::json;

#[test]
fn load_weather_ir_and_resolve_params() {
    let yaml = std::fs::read_to_string("fixtures/weather_bot.ygtc").unwrap();
    let doc = load_ygtc_from_str(&yaml).unwrap();
    let flow = compile_flow(doc).unwrap();

    assert_eq!(flow.id.as_str(), "weather_bot");
    assert_eq!(flow.kind, FlowKind::Messaging);
    assert_eq!(flow.entrypoints.get("default"), Some(&json!("in")));

    let fw = flow
        .nodes
        .get(&NodeId::new("forecast_weather").unwrap())
        .unwrap();
    assert_eq!(fw.component.id.as_str(), "component.exec");
    assert_eq!(fw.component.operation.as_deref(), Some("mcp.exec"));

    let resolved = resolve_parameters(
        &fw.input.mapping,
        &flow.metadata.extra,
        "nodes.forecast_weather",
    )
    .unwrap();
    assert_eq!(resolved.pointer("/args/days").unwrap(), &json!(3));
    assert_eq!(
        resolved.pointer("/args/q").unwrap(),
        &json!("in.q_location")
    );
}

#[test]
fn entrypoints_output_and_telemetry_round_trip() {
    let yaml = r#"
id: extras_flow
type: messaging
tags: ["demo"]
entrypoints:
  default: "start"
nodes:
  start:
    qa.process:
      payload: true
    output:
      select: "$.foo"
    routing:
      - out: true
    telemetry:
      span_name: "demo"
      attributes:
        k: v
      sampling: "high"
"#;
    let doc = load_ygtc_from_str(yaml).unwrap();
    let flow = compile_flow(doc).unwrap();
    assert_eq!(flow.entrypoints.get("default"), Some(&json!("start")));
    let node = flow.nodes.get(&NodeId::new("start").unwrap()).unwrap();
    assert_eq!(
        node.output.mapping.pointer("/select"),
        Some(&json!("$.foo"))
    );
    assert_eq!(node.telemetry.span_name.as_deref(), Some("demo"));
    assert_eq!(
        node.telemetry.attributes.get("k").map(String::as_str),
        Some("v")
    );
    assert_eq!(node.telemetry.sampling.as_deref(), Some("high"));
    assert!(flow.metadata.tags.contains("demo"));
}

#[test]
fn branch_and_reply_routing() {
    let yaml = r#"
id: routing_flow
type: messaging
nodes:
  in:
    qa.process:
      payload: true
    routing:
      - status: ok
        to: next
      - to: fallback
  next:
    qa.process:
      payload: true
    routing:
      - reply: true
  fallback:
    qa.process: {}
"#;
    let doc = load_ygtc_from_str(yaml).unwrap();
    let flow = compile_flow(doc).unwrap();
    use greentic_types::Routing;
    match &flow.nodes.get(&NodeId::new("in").unwrap()).unwrap().routing {
        Routing::Branch { on_status, default } => {
            assert!(on_status.contains_key("ok"));
            assert_eq!(on_status.get("ok").unwrap(), &NodeId::new("next").unwrap());
            assert_eq!(default.as_ref(), Some(&NodeId::new("fallback").unwrap()));
        }
        other => panic!("expected branch routing, got {other:?}"),
    }
    match &flow
        .nodes
        .get(&NodeId::new("next").unwrap())
        .unwrap()
        .routing
    {
        Routing::Reply => {}
        other => panic!("expected reply routing, got {other:?}"),
    }
}

#[test]
fn v2_dotted_operation_stays_as_operation() {
    let yaml = r#"
id: dotted_op
type: messaging
schema_version: 2
nodes:
  start:
    templating.handlebars:
      text: "hi"
    routing: out
"#;
    let doc = load_ygtc_from_str(yaml).unwrap();
    let flow = compile_flow(doc).unwrap();
    let node = flow.nodes.get(&NodeId::new("start").unwrap()).unwrap();
    assert_eq!(node.component.id.as_str(), "component.exec");
    assert_eq!(
        node.component.operation.as_deref(),
        Some("templating.handlebars")
    );
}
