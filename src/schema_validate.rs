use ciborium::value::Value as CborValue;
use greentic_types::schemas::common::schema_ir::{AdditionalProperties, SchemaIr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone)]
pub struct SchemaDiagnostic {
    pub code: &'static str,
    pub severity: Severity,
    pub message: String,
    pub path: String,
}

pub fn validate_value_against_schema(
    schema: &SchemaIr,
    value: &CborValue,
) -> Vec<SchemaDiagnostic> {
    let mut diags = Vec::new();
    validate_inner(schema, value, "$", &mut diags);
    diags
}

fn validate_inner(
    schema: &SchemaIr,
    value: &CborValue,
    path: &str,
    diags: &mut Vec<SchemaDiagnostic>,
) {
    match schema {
        SchemaIr::Object {
            properties,
            required,
            additional,
        } => validate_object(properties, required, additional, value, path, diags),
        SchemaIr::Array {
            items,
            min_items,
            max_items,
        } => validate_array(items, *min_items, *max_items, value, path, diags),
        SchemaIr::String {
            min_len,
            max_len,
            regex,
            format,
        } => validate_string(
            *min_len,
            *max_len,
            regex.as_deref(),
            format.as_deref(),
            value,
            path,
            diags,
        ),
        SchemaIr::Int { min, max } => validate_int(*min, *max, value, path, diags),
        SchemaIr::Float { min, max } => validate_float(*min, *max, value, path, diags),
        SchemaIr::Bool => require_kind("boolean", matches!(value, CborValue::Bool(_)), path, diags),
        SchemaIr::Null => require_kind("null", matches!(value, CborValue::Null), path, diags),
        SchemaIr::Bytes => require_kind("bytes", matches!(value, CborValue::Bytes(_)), path, diags),
        SchemaIr::Enum { values } => validate_enum(values, value, path, diags),
        SchemaIr::OneOf { variants } => validate_one_of(variants, value, path, diags),
        SchemaIr::Ref { id } => {
            diags.push(SchemaDiagnostic {
                code: "SCHEMA_REF_UNSUPPORTED",
                severity: Severity::Error,
                message: format!("schema ref '{}' is not supported", id),
                path: path.to_string(),
            });
        }
    }
}

fn require_kind(kind: &str, ok: bool, path: &str, diags: &mut Vec<SchemaDiagnostic>) {
    if !ok {
        diags.push(SchemaDiagnostic {
            code: "SCHEMA_TYPE_MISMATCH",
            severity: Severity::Error,
            message: format!("expected {kind} at {path}"),
            path: path.to_string(),
        });
    }
}

fn validate_object(
    properties: &std::collections::BTreeMap<String, SchemaIr>,
    required: &[String],
    additional: &AdditionalProperties,
    value: &CborValue,
    path: &str,
    diags: &mut Vec<SchemaDiagnostic>,
) {
    let map = match value {
        CborValue::Map(entries) => entries,
        _ => {
            require_kind("object", false, path, diags);
            return;
        }
    };

    let mut values: std::collections::BTreeMap<String, &CborValue> =
        std::collections::BTreeMap::new();
    for (k, v) in map {
        match k {
            CborValue::Text(s) => {
                values.insert(s.clone(), v);
            }
            _ => {
                diags.push(SchemaDiagnostic {
                    code: "SCHEMA_INVALID_KEY",
                    severity: Severity::Error,
                    message: format!("non-string object key at {path}"),
                    path: path.to_string(),
                });
            }
        }
    }

    for key in required {
        if !values.contains_key(key) {
            diags.push(SchemaDiagnostic {
                code: "SCHEMA_REQUIRED_MISSING",
                severity: Severity::Error,
                message: format!("missing required field '{key}' at {path}"),
                path: format!("{path}.{key}"),
            });
        }
    }

    for (key, val) in values {
        if let Some(prop_schema) = properties.get(&key) {
            validate_inner(prop_schema, val, &format!("{path}.{key}"), diags);
            continue;
        }
        match additional {
            AdditionalProperties::Allow => {}
            AdditionalProperties::Forbid => {
                diags.push(SchemaDiagnostic {
                    code: "SCHEMA_ADDITIONAL_FORBIDDEN",
                    severity: Severity::Error,
                    message: format!("additional property '{key}' not allowed at {path}"),
                    path: format!("{path}.{key}"),
                });
            }
            AdditionalProperties::Schema(schema) => {
                validate_inner(schema, val, &format!("{path}.{key}"), diags);
            }
        }
    }
}

fn validate_array(
    items: &SchemaIr,
    min_items: Option<u64>,
    max_items: Option<u64>,
    value: &CborValue,
    path: &str,
    diags: &mut Vec<SchemaDiagnostic>,
) {
    let items_val = match value {
        CborValue::Array(items) => items,
        _ => {
            require_kind("array", false, path, diags);
            return;
        }
    };
    let len = items_val.len() as u64;
    if let Some(min) = min_items
        && len < min
    {
        diags.push(SchemaDiagnostic {
            code: "SCHEMA_ARRAY_MIN_ITEMS",
            severity: Severity::Error,
            message: format!("array length {len} < min_items {min} at {path}"),
            path: path.to_string(),
        });
    }
    if let Some(max) = max_items
        && len > max
    {
        diags.push(SchemaDiagnostic {
            code: "SCHEMA_ARRAY_MAX_ITEMS",
            severity: Severity::Error,
            message: format!("array length {len} > max_items {max} at {path}"),
            path: path.to_string(),
        });
    }
    for (idx, item) in items_val.iter().enumerate() {
        validate_inner(items, item, &format!("{path}[{idx}]"), diags);
    }
}

fn validate_string(
    min_len: Option<u64>,
    max_len: Option<u64>,
    regex: Option<&str>,
    format: Option<&str>,
    value: &CborValue,
    path: &str,
    diags: &mut Vec<SchemaDiagnostic>,
) {
    let text = match value {
        CborValue::Text(s) => s,
        _ => {
            require_kind("string", false, path, diags);
            return;
        }
    };
    let len = text.chars().count() as u64;
    if let Some(min) = min_len
        && len < min
    {
        diags.push(SchemaDiagnostic {
            code: "SCHEMA_STRING_MIN_LEN",
            severity: Severity::Error,
            message: format!("string length {len} < min_len {min} at {path}"),
            path: path.to_string(),
        });
    }
    if let Some(max) = max_len
        && len > max
    {
        diags.push(SchemaDiagnostic {
            code: "SCHEMA_STRING_MAX_LEN",
            severity: Severity::Error,
            message: format!("string length {len} > max_len {max} at {path}"),
            path: path.to_string(),
        });
    }
    if regex.is_some() {
        diags.push(SchemaDiagnostic {
            code: "SCHEMA_REGEX_UNSUPPORTED",
            severity: Severity::Warning,
            message: format!("regex constraint not enforced at {path}"),
            path: path.to_string(),
        });
    }
    if format.is_some() {
        diags.push(SchemaDiagnostic {
            code: "SCHEMA_FORMAT_UNSUPPORTED",
            severity: Severity::Warning,
            message: format!("format constraint not enforced at {path}"),
            path: path.to_string(),
        });
    }
}

fn validate_int(
    min: Option<i64>,
    max: Option<i64>,
    value: &CborValue,
    path: &str,
    diags: &mut Vec<SchemaDiagnostic>,
) {
    let num = match value {
        CborValue::Integer(i) => i128::from(*i),
        _ => {
            require_kind("integer", false, path, diags);
            return;
        }
    };
    if let Some(min) = min
        && num < min as i128
    {
        diags.push(SchemaDiagnostic {
            code: "SCHEMA_INT_MIN",
            severity: Severity::Error,
            message: format!("integer {num} < min {min} at {path}"),
            path: path.to_string(),
        });
    }
    if let Some(max) = max
        && num > max as i128
    {
        diags.push(SchemaDiagnostic {
            code: "SCHEMA_INT_MAX",
            severity: Severity::Error,
            message: format!("integer {num} > max {max} at {path}"),
            path: path.to_string(),
        });
    }
}

fn validate_float(
    min: Option<f64>,
    max: Option<f64>,
    value: &CborValue,
    path: &str,
    diags: &mut Vec<SchemaDiagnostic>,
) {
    let num = match value {
        CborValue::Float(f) => *f,
        CborValue::Integer(i) => i128::from(*i) as f64,
        _ => {
            require_kind("number", false, path, diags);
            return;
        }
    };
    if let Some(min) = min
        && num < min
    {
        diags.push(SchemaDiagnostic {
            code: "SCHEMA_FLOAT_MIN",
            severity: Severity::Error,
            message: format!("number {num} < min {min} at {path}"),
            path: path.to_string(),
        });
    }
    if let Some(max) = max
        && num > max
    {
        diags.push(SchemaDiagnostic {
            code: "SCHEMA_FLOAT_MAX",
            severity: Severity::Error,
            message: format!("number {num} > max {max} at {path}"),
            path: path.to_string(),
        });
    }
}

fn validate_enum(
    values: &[CborValue],
    value: &CborValue,
    path: &str,
    diags: &mut Vec<SchemaDiagnostic>,
) {
    if values.iter().any(|candidate| candidate == value) {
        return;
    }
    diags.push(SchemaDiagnostic {
        code: "SCHEMA_ENUM",
        severity: Severity::Error,
        message: format!("value is not in enum at {path}"),
        path: path.to_string(),
    });
}

fn validate_one_of(
    variants: &[SchemaIr],
    value: &CborValue,
    path: &str,
    diags: &mut Vec<SchemaDiagnostic>,
) {
    for variant in variants {
        let mut local = Vec::new();
        validate_inner(variant, value, path, &mut local);
        if local.iter().all(|d| d.severity != Severity::Error) {
            return;
        }
    }
    diags.push(SchemaDiagnostic {
        code: "SCHEMA_ONE_OF",
        severity: Severity::Error,
        message: format!("value does not match any oneOf variant at {path}"),
        path: path.to_string(),
    });
}
