use greentic_flow::component_catalog::normalize_manifest_value;
use serde_json::json;

#[test]
fn normalizes_operations_strings() {
    let mut value = json!({
        "id": "ai.greentic.echo",
        "operations": ["run", { "name": "handle" }],
        "config_schema": { "required": ["message"] }
    });

    normalize_manifest_value(&mut value);
    let ops = value
        .get("operations")
        .and_then(|v| v.as_array())
        .expect("operations array");
    assert_eq!(ops[0], json!({"name": "run"}));
    assert_eq!(ops[1], json!({"name": "handle"}));
}
