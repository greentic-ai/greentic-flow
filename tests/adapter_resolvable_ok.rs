use greentic_flow::{
    compile_flow, lint::lint_with_registry, loader::load_ygtc_from_str, registry::AdapterCatalog,
};

#[test]
fn adapter_nodes_resolve_with_registry() {
    let yaml = std::fs::read_to_string("tests/data/flow_ok.ygtc").unwrap();
    let doc = load_ygtc_from_str(&yaml).unwrap();
    let flow = compile_flow(doc).unwrap();
    let catalog = AdapterCatalog::load_from_file("tests/data/registry_ok.json").unwrap();

    let errors = lint_with_registry(&flow, &catalog);

    assert!(errors.is_empty(), "expected no lint errors, got {errors:?}");
}
