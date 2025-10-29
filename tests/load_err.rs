use greentic_flow::loader::load_ygtc_from_str;
use std::path::Path;

#[test]
fn two_components_is_error() {
    let yaml = std::fs::read_to_string("fixtures/invalid_node_shape.ygtc").unwrap();
    let err = load_ygtc_from_str(&yaml, Path::new("schemas/ygtc.flow.schema.json")).unwrap_err();
    let msg = format!("{}", err);
    assert!(msg.contains("must contain exactly one component key"));
}
