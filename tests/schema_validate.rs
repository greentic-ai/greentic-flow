use ciborium::value::Value as CborValue;
use greentic_flow::schema_validate::{Severity, validate_value_against_schema};
use greentic_types::schemas::common::schema_ir::{AdditionalProperties, SchemaIr};

#[test]
fn schema_validate_reports_required_missing() {
    let schema = SchemaIr::Object {
        properties: [(
            "name".to_string(),
            SchemaIr::String {
                min_len: None,
                max_len: None,
                regex: None,
                format: None,
            },
        )]
        .into_iter()
        .collect(),
        required: vec!["name".to_string()],
        additional: AdditionalProperties::Allow,
    };
    let value = CborValue::Map(Vec::new());
    let diags = validate_value_against_schema(&schema, &value);
    assert!(diags.iter().any(|d| d.code == "SCHEMA_REQUIRED_MISSING"));
}

#[test]
fn schema_validate_reports_type_mismatch() {
    let schema = SchemaIr::String {
        min_len: None,
        max_len: None,
        regex: None,
        format: None,
    };
    let value = CborValue::Bool(true);
    let diags = validate_value_against_schema(&schema, &value);
    assert!(diags.iter().any(|d| d.code == "SCHEMA_TYPE_MISMATCH"));
}

#[test]
fn schema_validate_forbids_additional_properties() {
    let schema = SchemaIr::Object {
        properties: std::collections::BTreeMap::new(),
        required: Vec::new(),
        additional: AdditionalProperties::Forbid,
    };
    let value = CborValue::Map(vec![(
        CborValue::Text("extra".to_string()),
        CborValue::Bool(true),
    )]);
    let diags = validate_value_against_schema(&schema, &value);
    assert!(
        diags
            .iter()
            .any(|d| d.code == "SCHEMA_ADDITIONAL_FORBIDDEN")
    );
}

#[test]
fn schema_validate_warns_on_regex() {
    let schema = SchemaIr::String {
        min_len: None,
        max_len: None,
        regex: Some("^foo$".to_string()),
        format: None,
    };
    let value = CborValue::Text("foo".to_string());
    let diags = validate_value_against_schema(&schema, &value);
    assert!(
        diags
            .iter()
            .any(|d| d.code == "SCHEMA_REGEX_UNSUPPORTED" && d.severity == Severity::Warning)
    );
}
