use greentic_flow::{
    add_step::{AddStepSpec, apply_plan, plan_add_step, validate_flow},
    compile_flow,
    component_catalog::{ComponentMetadata, MemoryCatalog},
    flow_ir::parse_flow_to_ir,
    loader::load_ygtc_from_str,
    splice::NEXT_NODE_PLACEHOLDER,
};
use serde_json::{json, to_value};

#[test]
fn add_step_golden_flow() {
    let input = include_str!("golden/add_step/input.ygtc");
    let expected = include_str!("golden/add_step/expected.ygtc");

    let ir = parse_flow_to_ir(input).expect("parse input ir");

    let mut catalog = MemoryCatalog::default();
    catalog.insert(ComponentMetadata {
        id: "qa.process".to_string(),
        required_fields: Vec::new(),
    });
    catalog.insert(ComponentMetadata {
        id: "ai.greentic.echo".to_string(),
        required_fields: vec!["message".to_string()],
    });

    let spec = AddStepSpec {
        after: Some("start".to_string()),
        node_id_hint: Some("mid".to_string()),
        node: json!({
            "ai.greentic.echo": { "message": "hello" },
            "routing": [ { "to": NEXT_NODE_PLACEHOLDER } ]
        }),
        allow_cycles: false,
        require_placeholder: true,
    };

    let plan = plan_add_step(&ir, spec, &catalog).expect("plan success");
    let updated = apply_plan(&ir, plan, false).expect("apply");
    let diags = validate_flow(&updated, &catalog);
    assert!(
        diags.is_empty(),
        "expected no diagnostics, got: {:?}",
        diags
    );

    let updated_doc = updated.to_doc().expect("to doc");
    let updated_flow = compile_flow(updated_doc).expect("compile updated flow");

    let expected_doc = load_ygtc_from_str(expected).expect("load expected");
    let expected_flow = compile_flow(expected_doc).expect("compile expected");

    let left = to_value(&updated_flow).expect("serialize updated flow");
    let right = to_value(&expected_flow).expect("serialize expected flow");
    assert_eq!(left, right, "flow did not match golden output");
}
