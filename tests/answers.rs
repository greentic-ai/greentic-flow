use greentic_flow::answers::write_answers;
use greentic_types::cbor::canonical;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use tempfile::tempdir;

#[test]
fn write_answers_writes_json_and_cbor() {
    let dir = tempdir().unwrap();
    let mut answers = BTreeMap::new();
    answers.insert("name".to_string(), Value::String("Widget".to_string()));
    answers.insert("count".to_string(), Value::Number(3.into()));

    let paths = write_answers(
        dir.path(),
        "flow-main",
        "node-1",
        "default",
        &answers,
        false,
    )
    .unwrap();

    let json_text = fs::read_to_string(paths.json).unwrap();
    let json_value: Value = serde_json::from_str(&json_text).unwrap();
    assert_eq!(json_value["name"], "Widget");
    assert_eq!(json_value["count"], 3);

    let cbor_bytes = fs::read(paths.cbor).unwrap();
    let cbor_value: Value = canonical::from_cbor(&cbor_bytes).unwrap();
    assert_eq!(cbor_value, json_value);
}
