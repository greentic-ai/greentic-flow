use crate::{
    component_catalog::normalize_manifest_value,
    error::{FlowError, FlowErrorLocation, Result},
};
use jsonschema::Draft;
use serde_json::{Map, Value};
use std::{
    fs,
    path::{Path, PathBuf},
};
use url::Url;

const SCHEMA_GUIDANCE: &str = "Define operations[].input_schema with real JSON Schema or define dev_flows.<op> questions/schema.";

#[derive(Clone)]
pub struct SchemaResolution {
    pub component_id: String,
    pub operation: String,
    pub manifest_path: PathBuf,
    pub schema: Option<Value>,
}

impl SchemaResolution {
    fn new(
        component_id: String,
        operation: String,
        manifest_path: PathBuf,
        schema: Option<Value>,
    ) -> Self {
        Self {
            component_id,
            operation,
            manifest_path,
            schema,
        }
    }
}

pub fn resolve_input_schema(manifest_path: &Path, operation: &str) -> Result<SchemaResolution> {
    let text = fs::read_to_string(manifest_path).map_err(|err| FlowError::Internal {
        message: format!("read manifest {}: {err}", manifest_path.display()),
        location: FlowErrorLocation::at_path(manifest_path.display().to_string()),
    })?;
    let mut json: Value = serde_json::from_str(&text).map_err(|err| FlowError::Internal {
        message: format!("parse manifest {}: {err}", manifest_path.display()),
        location: FlowErrorLocation::at_path(manifest_path.display().to_string()),
    })?;
    normalize_manifest_value(&mut json);
    let component_id = json
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let mut schema = json
        .get("operations")
        .and_then(Value::as_array)
        .and_then(|ops| {
            ops.iter()
                .find(|entry| matches_operation(entry, operation))
                .and_then(schema_value)
        });
    if schema.is_none() {
        schema = json.get("config_schema").cloned();
    }
    Ok(SchemaResolution::new(
        component_id,
        operation.to_string(),
        manifest_path.to_path_buf(),
        schema,
    ))
}

fn matches_operation(entry: &Value, operation: &str) -> bool {
    operation_name(entry)
        .map(|name| name == operation)
        .unwrap_or(false)
}

fn operation_name(entry: &Value) -> Option<&str> {
    entry
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| entry.get("operation").and_then(Value::as_str))
        .or_else(|| entry.get("id").and_then(Value::as_str))
}

fn schema_value(entry: &Value) -> Option<Value> {
    for key in ["input_schema", "schema"] {
        if let Some(value) = entry.get(key)
            && !value.is_null()
        {
            return Some(value.clone());
        }
    }
    None
}

pub fn is_effectively_empty_schema(schema: &Value) -> bool {
    match schema {
        Value::Null => true,
        Value::Bool(true) => true,
        Value::Object(map) => {
            if map.is_empty() {
                return true;
            }
            !object_schema_has_constraints(map)
        }
        _ => false,
    }
}

fn object_schema_has_constraints(map: &Map<String, Value>) -> bool {
    for (key, value) in map {
        match key.as_str() {
            "$schema" | "$id" | "description" | "title" | "examples" | "default" => continue,
            "type" => {
                if let Some(t) = value.as_str() {
                    if t != "object" {
                        return true;
                    }
                } else {
                    return true;
                }
            }
            "properties" => {
                if let Some(props) = value.as_object() {
                    if props.is_empty() {
                        continue;
                    }
                    return true;
                }
                return true;
            }
            "required" => {
                if let Some(arr) = value.as_array() {
                    if arr.is_empty() {
                        continue;
                    }
                } else {
                    return true;
                }
                return true;
            }
            "additionalProperties" => match value {
                Value::Bool(false) => return true,
                Value::Bool(true) => continue,
                _ => return true,
            },
            "patternProperties" | "dependentSchemas" | "dependentRequired" | "const" | "enum"
            | "items" | "oneOf" | "anyOf" | "allOf" | "not" | "if" | "then" | "else"
            | "multipleOf" | "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum"
            | "minLength" | "maxLength" | "minItems" | "maxItems" | "contains"
            | "minProperties" | "maxProperties" | "pattern" | "format" | "$ref" | "$defs"
            | "dependencies" => return true,
            _ => {
                return true;
            }
        }
    }
    false
}

pub fn validate_payload_against_schema(ctx: &SchemaResolution, payload: &Value) -> Result<()> {
    let schema = ctx.schema.as_ref().ok_or_else(|| FlowError::Internal {
        message: format!(
            "component_config: schema missing for component '{}' operation '{}'",
            ctx.component_id, ctx.operation
        ),
        location: FlowErrorLocation::at_path(ctx.manifest_path.display().to_string()),
    })?;
    let validator = jsonschema_options_with_base(Some(ctx.manifest_path.as_path()))
        .build(schema)
        .map_err(|err| FlowError::Internal {
            message: format!(
                "component_config: schema compile failed for component '{}': {err}",
                ctx.component_id
            ),
            location: FlowErrorLocation::at_path(ctx.manifest_path.display().to_string()),
        })?;
    let mut errors = Vec::new();
    for err in validator.iter_errors(payload) {
        let pointer = err.instance_path().to_string();
        let pointer = if pointer.is_empty() {
            "/".to_string()
        } else {
            pointer
        };
        errors.push(format!(
            "component_config: payload invalid for component '{}' operation '{}' at {pointer}: {err}",
            ctx.component_id, ctx.operation
        ));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(FlowError::Internal {
            message: errors.join("; "),
            location: FlowErrorLocation::at_path(ctx.manifest_path.display().to_string()),
        })
    }
}

pub fn jsonschema_options_with_base(base_path: Option<&Path>) -> jsonschema::ValidationOptions {
    let mut options = jsonschema::options().with_draft(Draft::Draft202012);
    if let Some(base_uri) = base_uri_for_path(base_path) {
        options = options.with_base_uri(base_uri);
    }
    options
}

fn base_uri_for_path(path: Option<&Path>) -> Option<String> {
    let base_dir = path?.parent()?;
    let canonical_dir = base_dir.canonicalize().ok()?;
    let mut url = Url::from_directory_path(&canonical_dir).ok()?;
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path().trim_end_matches('/')));
    }
    Some(url.to_string())
}

pub fn schema_guidance() -> &'static str {
    SCHEMA_GUIDANCE
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_object_schema_is_empty() {
        assert!(is_effectively_empty_schema(&json!({})));
    }

    #[test]
    fn object_schema_without_constraints_is_empty() {
        assert!(is_effectively_empty_schema(&json!({ "type": "object" })));
    }

    #[test]
    fn object_schema_with_property_is_not_empty() {
        assert!(!is_effectively_empty_schema(&json!({
            "type": "object",
            "properties": { "name": { "type": "string" } }
        })));
    }

    #[test]
    fn object_schema_with_required_is_not_empty() {
        assert!(!is_effectively_empty_schema(&json!({
            "type": "object",
            "required": [ "name" ]
        })));
    }

    #[test]
    fn object_schema_with_oneof_is_not_empty() {
        assert!(!is_effectively_empty_schema(&json!({
            "type": "object",
            "oneOf": [{ "properties": { "a": { "const": 1 } } }]
        })));
    }

    #[test]
    fn additional_properties_false_is_not_empty() {
        assert!(!is_effectively_empty_schema(&json!({
            "type": "object",
            "additionalProperties": false
        })));
    }
}
