use greentic_flow::{
    compile_flow, lint::lint_with_registry, loader::load_ygtc_from_str, registry::AdapterCatalog,
};

#[test]
fn adapter_nodes_fail_with_missing_operations() {
    let yaml = std::fs::read_to_string("tests/data/flow_fail.ygtc").unwrap();
    let doc = load_ygtc_from_str(&yaml).unwrap();
    let flow = compile_flow(doc).unwrap();
    let catalog = AdapterCatalog::load_from_file("tests/data/registry_ok.json").unwrap();

    let errors = lint_with_registry(&flow, &catalog);

    assert_eq!(errors.len(), 2, "expected exactly two lint errors");
    assert!(
        errors
            .iter()
            .any(|e| e.contains("messaging.telegram.deleteUniverse"))
    );
    assert!(errors.iter().any(|e| e.contains("email.google.beamMeUp")));
}
