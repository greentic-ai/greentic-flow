use greentic_flow::{compile_flow, lint::lint_builtin_rules, loader::load_ygtc_from_str};

#[test]
fn lint_flags_missing_start_node() {
    let yaml = r#"
id: demo
type: messaging
start: missing
nodes:
  entry:
    qa.process: {}
"#;
    let doc = load_ygtc_from_str(yaml).unwrap();
    let flow = compile_flow(doc).unwrap();
    let errors = lint_builtin_rules(&flow);
    assert!(
        errors.iter().any(|e| e.contains("start node 'missing'")),
        "expected missing start node lint, got {errors:?}"
    );
}

#[test]
fn lint_passes_when_start_node_exists() {
    let yaml = r#"
id: demo
type: messaging
start: entry
nodes:
  entry:
    qa.process: {}
"#;
    let doc = load_ygtc_from_str(yaml).unwrap();
    let flow = compile_flow(doc).unwrap();
    let errors = lint_builtin_rules(&flow);
    assert!(errors.is_empty(), "unexpected lint errors: {errors:?}");
}
