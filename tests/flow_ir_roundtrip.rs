use greentic_flow::{compile_flow, flow_ir::parse_flow_to_ir, loader::load_ygtc_from_str};

#[test]
fn flow_ir_roundtrip_preserves_structure() {
    let yaml = include_str!("data/config_flow.ygtc");
    let original = load_ygtc_from_str(yaml).expect("load");

    let ir = parse_flow_to_ir(yaml).expect("parse ir");
    let doc = ir.to_doc().expect("to doc");

    assert_eq!(doc.id, original.id);
    assert_eq!(doc.flow_type, original.flow_type);
    assert_eq!(doc.nodes.len(), original.nodes.len());

    // Ensure the round-tripped doc still compiles.
    compile_flow(doc).expect("compile");
}
