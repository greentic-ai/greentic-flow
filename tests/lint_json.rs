use assert_cmd::Command;
use serde_json::Value;
use tempfile::tempdir;

#[test]
fn json_mode_emits_bundle() {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("greentic-flow"));
    let assert = cmd
        .arg("doctor")
        .arg("--json")
        .arg("fixtures/weather_bot.ygtc")
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let payload: Value = serde_json::from_str(&stdout).unwrap();
    assert!(payload["ok"].as_bool().unwrap());
    assert_eq!(payload["bundle"]["id"].as_str(), Some("weather_bot"));
    assert!(payload["hash_blake3"].as_str().is_some());
}

#[test]
fn json_mode_reports_schema_errors() {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("greentic-flow"));
    let assert = cmd
        .arg("doctor")
        .arg("--json")
        .arg("tests/data/flow_missing_type.ygtc")
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let payload: Value = serde_json::from_str(&stdout).unwrap();
    assert!(!payload["ok"].as_bool().unwrap());
    let errors = payload["errors"].as_array().expect("errors array");
    assert!(!errors.is_empty(), "expected errors to be reported");
}

#[test]
fn json_mode_stdin_reports_schema_pointer() {
    let stdin_flow = std::fs::read_to_string("tests/data/flow_missing_type.ygtc").unwrap();
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("greentic-flow"));
    let assert = cmd
        .arg("doctor")
        .arg("--json")
        .arg("--stdin")
        .write_stdin(stdin_flow)
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let payload: Value = serde_json::from_str(&stdout).unwrap();
    assert!(!payload["ok"].as_bool().unwrap());
    let errors = payload["errors"].as_array().expect("errors array");
    assert!(!errors.is_empty(), "expected errors to be reported");
}

#[test]
fn json_mode_reports_sidecar_errors() {
    let dir = tempdir().unwrap();
    let flow_path = dir.path().join("flow.ygtc");
    let sidecar_path = flow_path.with_extension("ygtc.resolve.json");
    let wasm_path = dir.path().join("comp.wasm");
    std::fs::write(&wasm_path, b"wasm-bytes").unwrap();
    std::fs::write(
        &flow_path,
        r#"id: main
type: messaging
schema_version: 2
nodes:
  keep:
    op: {}
    routing: out
"#,
    )
    .unwrap();
    std::fs::write(
        &sidecar_path,
        r#"{"schema_version":1,"flow":"flow.ygtc","nodes":{"keep":{"source":{"kind":"local","path":"comp.wasm"}},"stale":{"source":{"kind":"local","path":"comp.wasm"}}}}"#,
    )
    .unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("greentic-flow"));
    let assert = cmd
        .arg("doctor")
        .arg("--json")
        .arg(&flow_path)
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let payload: Value = serde_json::from_str(&stdout).unwrap();
    assert!(!payload["ok"].as_bool().unwrap());
    let errors = payload["errors"].as_array().expect("errors array");
    assert!(errors.iter().any(|e| {
        e.get("message")
            .and_then(Value::as_str)
            .map(|m| m.contains("unused sidecar entries"))
            .unwrap_or(false)
    }));
}
