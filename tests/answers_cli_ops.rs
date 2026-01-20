use assert_cmd::cargo::cargo_bin_cmd;
use jsonschema::Draft;
use predicates::prelude::*;
use serde_json::json;
use std::{fs, path::PathBuf};
use tempfile::tempdir;

fn manifest_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("manifests")
        .join(name)
}

fn validate_with_schema(schema: &serde_json::Value, instance: &serde_json::Value) -> bool {
    let compiled = jsonschema::options()
        .with_draft(Draft::Draft202012)
        .build(schema)
        .expect("compile schema");
    compiled.validate(instance).is_ok()
}

#[test]
fn answers_generates_schema_and_example_and_validates() {
    let dir = tempdir().unwrap();
    let schema_path = dir.path().join("answers.schema.json");
    let example_path = dir.path().join("answers.example.json");

    cargo_bin_cmd!("greentic-flow")
        .arg("answers")
        .arg("--component")
        .arg(manifest_path("component.manifest.json"))
        .arg("--operation")
        .arg("default")
        .arg("--name")
        .arg("answers")
        .arg("--out-dir")
        .arg(dir.path())
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    assert!(schema_path.exists());
    assert!(example_path.exists());

    let schema: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&schema_path).unwrap()).unwrap();
    let example: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&example_path).unwrap()).unwrap();

    assert!(validate_with_schema(&schema, &example));

    cargo_bin_cmd!("greentic-flow")
        .arg("doctor-answers")
        .arg("--schema")
        .arg(&schema_path)
        .arg("--answers")
        .arg(&example_path)
        .assert()
        .success();
}

#[test]
fn doctor_answers_rejects_invalid_answers() {
    let dir = tempdir().unwrap();
    let schema_path = dir.path().join("answers.schema.json");
    let example_path = dir.path().join("answers.example.json");
    let invalid_path = dir.path().join("invalid.json");

    cargo_bin_cmd!("greentic-flow")
        .arg("answers")
        .arg("--component")
        .arg(manifest_path("component.manifest.json"))
        .arg("--operation")
        .arg("default")
        .arg("--name")
        .arg("answers")
        .arg("--out-dir")
        .arg(dir.path())
        .assert()
        .success();

    fs::write(&invalid_path, "{}\n").unwrap();

    let output = cargo_bin_cmd!("greentic-flow")
        .arg("doctor-answers")
        .arg("--schema")
        .arg(&schema_path)
        .arg("--answers")
        .arg(&invalid_path)
        .arg("--json")
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let payload: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(payload.get("ok"), Some(&json!(false)));
    let errors = payload
        .get("errors")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(!errors.is_empty());
    assert!(
        errors
            .iter()
            .filter_map(|v| v.as_str())
            .any(|msg| msg.contains("required"))
    );

    cargo_bin_cmd!("greentic-flow")
        .arg("doctor-answers")
        .arg("--schema")
        .arg(&schema_path)
        .arg("--answers")
        .arg(&example_path)
        .assert()
        .success();
}

#[test]
fn doctor_answers_enforces_conditionals() {
    let dir = tempdir().unwrap();
    let schema_path = dir.path().join("conditional.schema.json");
    let invalid_path = dir.path().join("invalid.json");

    cargo_bin_cmd!("greentic-flow")
        .arg("answers")
        .arg("--component")
        .arg(manifest_path("component-conditional.manifest.json"))
        .arg("--operation")
        .arg("default")
        .arg("--name")
        .arg("conditional")
        .arg("--out-dir")
        .arg(dir.path())
        .assert()
        .success();

    fs::write(&invalid_path, r#"{"card_source":"asset"}"#).unwrap();

    let output = cargo_bin_cmd!("greentic-flow")
        .arg("doctor-answers")
        .arg("--schema")
        .arg(&schema_path)
        .arg("--answers")
        .arg(&invalid_path)
        .arg("--json")
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let payload: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(payload.get("ok"), Some(&json!(false)));
    let errors = payload
        .get("errors")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        errors
            .iter()
            .filter_map(|v| v.as_str())
            .any(|msg| msg.contains("asset_path"))
    );
}
