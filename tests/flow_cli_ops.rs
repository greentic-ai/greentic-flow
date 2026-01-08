use assert_cmd::prelude::*;
use greentic_flow::loader::load_ygtc_from_path;
use serde_yaml_bw::Value;
use std::{fs, path::Path, process::Command};
use tempfile::tempdir;

fn read_yaml(path: &Path) -> Value {
    serde_yaml_bw::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn new_writes_v2_empty_flow() {
    let dir = tempdir().unwrap();
    let flow_path = dir.path().join("flow.ygtc");

    Command::new(assert_cmd::cargo::cargo_bin!("greentic-flow"))
        .arg("new")
        .arg("--flow")
        .arg(&flow_path)
        .arg("--id")
        .arg("main")
        .arg("--type")
        .arg("messaging")
        .arg("--description")
        .arg("test flow")
        .assert()
        .success();

    let doc = load_ygtc_from_path(&flow_path).expect("load flow");
    assert_eq!(doc.id, "main");
    assert_eq!(doc.flow_type, "messaging");
    assert_eq!(doc.schema_version, Some(2));
    assert!(doc.nodes.is_empty());
}

#[test]
fn add_step_on_legacy_writes_v2_and_shorthand() {
    let dir = tempdir().unwrap();
    let flow_path = dir.path().join("flow.ygtc");
    fs::write(
        &flow_path,
        r#"id: main
type: messaging
nodes:
  start:
    component.exec:
      component: ai.greentic.echo
      input: {}
    operation: run
    routing:
      - out: true
"#,
    )
    .unwrap();

    Command::new(assert_cmd::cargo::cargo_bin!("greentic-flow"))
        .arg("add-step")
        .arg("--flow")
        .arg(&flow_path)
        .arg("--mode")
        .arg("default")
        .arg("--operation")
        .arg("handle_message")
        .arg("--payload")
        .arg(r#"{"msg":"hi"}"#)
        .arg("--routing")
        .arg(r#"[{"to":"NEXT_NODE_PLACEHOLDER"}]"#)
        .arg("--after")
        .arg("start")
        .arg("--write")
        .assert()
        .success();

    let yaml = fs::read_to_string(&flow_path).unwrap();
    assert!(!yaml.contains("component.exec"));
    assert!(yaml.contains("routing: out"));
}

#[test]
fn update_metadata_changes_name_only() {
    let dir = tempdir().unwrap();
    let flow_path = dir.path().join("flow.ygtc");
    fs::write(
        &flow_path,
        r#"id: main
title: old
type: messaging
schema_version: 2
start: hello
nodes:
  hello:
    op:
      field: keep
    routing: out
"#,
    )
    .unwrap();

    Command::new(assert_cmd::cargo::cargo_bin!("greentic-flow"))
        .arg("update")
        .arg("--flow")
        .arg(&flow_path)
        .arg("--name")
        .arg("new-name")
        .assert()
        .success();

    let yaml = fs::read_to_string(&flow_path).unwrap();
    assert!(yaml.contains("title: new-name"));
    assert!(yaml.contains("field: keep"));
    assert!(yaml.contains("routing: out"));
}

#[test]
fn update_type_on_empty_flow_succeeds() {
    let dir = tempdir().unwrap();
    let flow_path = dir.path().join("flow.ygtc");
    Command::new(assert_cmd::cargo::cargo_bin!("greentic-flow"))
        .arg("new")
        .arg("--flow")
        .arg(&flow_path)
        .arg("--id")
        .arg("main")
        .arg("--type")
        .arg("messaging")
        .assert()
        .success();

    Command::new(assert_cmd::cargo::cargo_bin!("greentic-flow"))
        .arg("update")
        .arg("--flow")
        .arg(&flow_path)
        .arg("--type")
        .arg("events")
        .assert()
        .success();

    let doc = load_ygtc_from_path(&flow_path).expect("load flow");
    assert_eq!(doc.flow_type, "events");
}

#[test]
fn update_type_on_non_empty_fails() {
    let dir = tempdir().unwrap();
    let flow_path = dir.path().join("flow.ygtc");
    fs::write(
        &flow_path,
        r#"id: main
type: messaging
schema_version: 2
start: hello
nodes:
  hello:
    op: {}
    routing: out
"#,
    )
    .unwrap();

    Command::new(assert_cmd::cargo::cargo_bin!("greentic-flow"))
        .arg("update")
        .arg("--flow")
        .arg(&flow_path)
        .arg("--type")
        .arg("events")
        .assert()
        .failure();
}

#[test]
fn update_fails_when_missing_file() {
    let dir = tempdir().unwrap();
    let flow_path = dir.path().join("missing.ygtc");
    Command::new(assert_cmd::cargo::cargo_bin!("greentic-flow"))
        .arg("update")
        .arg("--flow")
        .arg(&flow_path)
        .arg("--name")
        .arg("noop")
        .assert()
        .failure();
}

#[test]
fn doctor_uses_embedded_schema_by_default() {
    let dir = tempdir().unwrap();
    let flow_path = dir.path().join("flow.ygtc");
    fs::write(
        &flow_path,
        r#"id: main
type: messaging
schema_version: 2
nodes: {}
parameters: {}
tags: []
entrypoints: {}
"#,
    )
    .unwrap();

    Command::new(assert_cmd::cargo::cargo_bin!("greentic-flow"))
        .arg("doctor")
        .arg(&flow_path)
        .assert()
        .success();
}

#[test]
fn update_step_preserves_when_no_answers() {
    let dir = tempdir().unwrap();
    let flow_path = dir.path().join("flow.ygtc");
    fs::write(
        &flow_path,
        r#"id: main
type: messaging
schema_version: 2
nodes:
  hello:
    op:
      field: old
    routing: out
"#,
    )
    .unwrap();

    Command::new(assert_cmd::cargo::cargo_bin!("greentic-flow"))
        .arg("update-step")
        .arg("--flow")
        .arg(&flow_path)
        .arg("--step")
        .arg("hello")
        .arg("--mode")
        .arg("config")
        .arg("--non-interactive")
        .arg("--write")
        .assert()
        .success();

    let yaml = read_yaml(&flow_path);
    let nodes = yaml
        .get("nodes")
        .and_then(Value::as_mapping)
        .expect("nodes map");
    let hello = nodes
        .get(Value::from("hello"))
        .unwrap()
        .as_mapping()
        .unwrap();
    let op = hello.get(Value::from("op")).unwrap().as_mapping().unwrap();
    assert_eq!(op.get(Value::from("field")).unwrap().as_str(), Some("old"));
    assert_eq!(
        hello.get(Value::from("routing")).unwrap().as_str(),
        Some("out")
    );
}

#[test]
fn update_step_overrides_payload_and_routing() {
    let dir = tempdir().unwrap();
    let flow_path = dir.path().join("flow.ygtc");
    fs::write(
        &flow_path,
        r#"id: main
type: messaging
schema_version: 2
nodes:
  hello:
    op:
      field: old
    routing: out
"#,
    )
    .unwrap();

    Command::new(assert_cmd::cargo::cargo_bin!("greentic-flow"))
        .arg("update-step")
        .arg("--flow")
        .arg(&flow_path)
        .arg("--step")
        .arg("hello")
        .arg("--answers")
        .arg(r#"{"field":"new","extra":1}"#)
        .arg("--routing")
        .arg("reply")
        .arg("--write")
        .assert()
        .success();

    let yaml = read_yaml(&flow_path);
    let nodes = yaml
        .get("nodes")
        .and_then(Value::as_mapping)
        .expect("nodes map");
    let hello = nodes
        .get(Value::from("hello"))
        .unwrap()
        .as_mapping()
        .unwrap();
    let op = hello.get(Value::from("op")).unwrap().as_mapping().unwrap();
    assert_eq!(op.get(Value::from("field")).unwrap().as_str(), Some("new"));
    assert_eq!(
        hello.get(Value::from("routing")).unwrap().as_str(),
        Some("reply")
    );
}

#[test]
fn delete_step_splices_single_predecessor() {
    let dir = tempdir().unwrap();
    let flow_path = dir.path().join("flow.ygtc");
    fs::write(
        &flow_path,
        r#"id: main
type: messaging
schema_version: 2
nodes:
  a:
    hop: {}
    routing:
      - to: mid
  mid:
    op: {}
    routing: out
"#,
    )
    .unwrap();

    Command::new(assert_cmd::cargo::cargo_bin!("greentic-flow"))
        .arg("delete-step")
        .arg("--flow")
        .arg(&flow_path)
        .arg("--step")
        .arg("mid")
        .arg("--write")
        .assert()
        .success();

    let yaml = read_yaml(&flow_path);
    let nodes = yaml
        .get("nodes")
        .and_then(Value::as_mapping)
        .expect("nodes map");
    assert!(!nodes.contains_key(Value::from("mid")));
    let a = nodes.get(Value::from("a")).unwrap().as_mapping().unwrap();
    if let Some(r) = a.get(Value::from("routing")) {
        if let Some(s) = r.as_str() {
            assert_eq!(s, "out");
        } else if let Some(seq) = r.as_sequence() {
            assert!(
                seq.is_empty()
                    || seq
                        .iter()
                        .any(|v| v.get("out").and_then(Value::as_bool) == Some(true))
            );
        }
    }
}

#[test]
fn delete_step_errors_on_multiple_predecessors() {
    let dir = tempdir().unwrap();
    let flow_path = dir.path().join("flow.ygtc");
    fs::write(
        &flow_path,
        r#"id: main
type: messaging
schema_version: 2
nodes:
  a:
    hop: {}
    routing:
      - to: mid
  b:
    hop: {}
    routing:
      - to: mid
  mid:
    op: {}
    routing:
      - to: end
  end:
    noop: {}
    routing: out
"#,
    )
    .unwrap();

    Command::new(assert_cmd::cargo::cargo_bin!("greentic-flow"))
        .arg("delete-step")
        .arg("--flow")
        .arg(&flow_path)
        .arg("--step")
        .arg("mid")
        .assert()
        .failure();
}

#[test]
fn delete_step_splice_all_predecessors() {
    let dir = tempdir().unwrap();
    let flow_path = dir.path().join("flow.ygtc");
    fs::write(
        &flow_path,
        r#"id: main
type: messaging
schema_version: 2
nodes:
  a:
    hop: {}
    routing:
      - to: mid
  b:
    hop: {}
    routing:
      - to: mid
  mid:
    op: {}
    routing:
      - to: end
  end:
    noop: {}
    routing: out
"#,
    )
    .unwrap();

    Command::new(assert_cmd::cargo::cargo_bin!("greentic-flow"))
        .arg("delete-step")
        .arg("--flow")
        .arg(&flow_path)
        .arg("--step")
        .arg("mid")
        .arg("--if-multiple-predecessors")
        .arg("splice-all")
        .arg("--write")
        .assert()
        .success();

    let yaml = read_yaml(&flow_path);
    let nodes = yaml
        .get("nodes")
        .and_then(Value::as_mapping)
        .expect("nodes map");
    assert!(!nodes.contains_key(Value::from("mid")));
    for pred in ["a", "b"] {
        let n = nodes.get(Value::from(pred)).unwrap().as_mapping().unwrap();
        let routing = n.get(Value::from("routing")).unwrap();
        if let Some(arr) = routing.as_sequence() {
            assert_eq!(
                arr[0].get("to").and_then(Value::as_str).expect("to target"),
                "end"
            );
        } else if let Some(s) = routing.as_str() {
            assert_eq!(s, "out");
        } else {
            panic!("unexpected routing shape");
        }
    }
}
