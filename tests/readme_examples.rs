use greentic_flow::{flow_ir::FlowIr, loader::load_ygtc_from_path};

#[test]
fn example_hello_flow_is_valid() {
    let path = std::path::Path::new("docs/examples/hello.ygtc");
    let doc = load_ygtc_from_path(path).expect("load example hello");
    let ir = FlowIr::from_doc(doc).expect("to ir");
    assert_eq!(ir.id, "hello-flow");
    let start = ir.entrypoints.get("default").cloned().unwrap_or_default();
    assert_eq!(start, "start");
    let node = ir.nodes.get("start").expect("start node");
    assert_eq!(node.operation, "templating.handlebars");
    assert!(node.routing.iter().any(|r| r.out));
}

#[test]
fn example_component_flow_is_valid() {
    let path = std::path::Path::new("docs/examples/hello_with_component.ygtc");
    let doc = load_ygtc_from_path(path).expect("load component example");
    let ir = FlowIr::from_doc(doc).expect("to ir");
    assert_eq!(ir.id, "hello-component");
    let node = ir.nodes.get("hello-world").expect("hello-world node");
    assert_eq!(node.operation, "handle_message");
    assert!(node.routing.iter().any(|r| r.out));
}
