use greentic_flow::splice::splice_node_after;
use serde_yaml_bw::Value as YamlValue;

#[test]
fn splice_after_rewires_routing() {
    let flow = r#"id: main
title: Welcome
description: Minimal starter flow
type: messaging
start: start

nodes:
  start:
    templating.handlebars:
      text: "Hello from greentic-pack starter!"
    routing:
      - out: true
"#;

    let new_node: YamlValue = serde_yaml_bw::from_str(
        r#"tool:
  component: ai.greentic.hello-world
routing:
  - to: NEXT_NODE_PLACEHOLDER
"#,
    )
    .unwrap();

    let updated_yaml = splice_node_after(flow, "hello", new_node, "start").unwrap();
    let doc: YamlValue = serde_yaml_bw::from_str(&updated_yaml).unwrap();
    assert_eq!(doc.get("id"), Some(&ystr("main")));

    let nodes = doc
        .get("nodes")
        .and_then(YamlValue::as_mapping)
        .expect("nodes mapping");
    let start = nodes
        .get(ystr("start"))
        .and_then(YamlValue::as_mapping)
        .expect("start node");
    let start_routes = start
        .get(ystr("routing"))
        .and_then(YamlValue::as_sequence)
        .expect("start routing");
    assert_eq!(start_routes.len(), 1);
    let start_route = start_routes[0].as_mapping().expect("route map");
    assert_eq!(start_route.get(ystr("to")), Some(&ystr("hello")));

    let hello = nodes
        .get(ystr("hello"))
        .and_then(YamlValue::as_mapping)
        .expect("hello node");
    let hello_routes = hello
        .get(ystr("routing"))
        .and_then(YamlValue::as_sequence)
        .expect("hello routing");
    assert_eq!(hello_routes.len(), 1);
    let hello_route = hello_routes[0].as_mapping().expect("route map");
    assert_eq!(hello_route.get(ystr("out")), Some(&ybool(true)));
}

fn ystr(value: &str) -> YamlValue {
    serde_yaml_bw::to_value(value).expect("string yaml value")
}

fn ybool(value: bool) -> YamlValue {
    serde_yaml_bw::to_value(value).expect("bool yaml value")
}
