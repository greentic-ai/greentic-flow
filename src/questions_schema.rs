use serde_json::Value;

use crate::questions::{Question, QuestionKind};

pub fn example_for_questions(questions: &[Question]) -> Value {
    let mut answers = std::collections::HashMap::new();
    let mut obj = serde_json::Map::new();
    for question in questions {
        if let Some(default) = question.default.clone() {
            answers.insert(question.id.clone(), default.clone());
            obj.insert(question.id.clone(), default);
        }
    }
    loop {
        let mut progressed = false;
        for question in questions {
            if !question_visible(question, &answers) {
                continue;
            }
            if answers.contains_key(&question.id) {
                continue;
            }
            let value = question
                .default
                .clone()
                .unwrap_or_else(|| default_value_for_question(question));
            answers.insert(question.id.clone(), value.clone());
            obj.insert(question.id.clone(), value);
            progressed = true;
        }
        if !progressed {
            break;
        }
    }
    Value::Object(obj)
}

pub fn schema_for_questions(questions: &[Question]) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    let mut conditionals = Vec::new();

    for question in questions {
        properties.insert(question.id.clone(), schema_for_question(question));
        match &question.show_if {
            None | Some(Value::Bool(true)) => {
                if question.required {
                    required.push(Value::String(question.id.clone()));
                }
            }
            Some(Value::Bool(false)) => {}
            Some(Value::Object(map)) => {
                if question.required {
                    let id = map.get("id").and_then(Value::as_str);
                    let expected = map.get("equals");
                    if let (Some(id), Some(expected)) = (id, expected) {
                        conditionals.push(serde_json::json!({
                            "if": {
                                "properties": { id: { "const": expected } },
                                "required": [id]
                            },
                            "then": {
                                "required": [question.id.clone()]
                            }
                        }));
                    }
                }
            }
            _ => {
                if question.required {
                    required.push(Value::String(question.id.clone()));
                }
            }
        }
    }

    let mut schema = serde_json::Map::new();
    schema.insert(
        "$schema".to_string(),
        Value::String("https://json-schema.org/draft/2020-12/schema".to_string()),
    );
    schema.insert("type".to_string(), Value::String("object".to_string()));
    schema.insert("additionalProperties".to_string(), Value::Bool(false));
    schema.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".to_string(), Value::Array(required));
    }
    if !conditionals.is_empty() {
        schema.insert("allOf".to_string(), Value::Array(conditionals));
    }
    Value::Object(schema)
}

fn schema_for_question(question: &Question) -> Value {
    let mut obj = serde_json::Map::new();
    match question.kind {
        QuestionKind::String => {
            let schema_type = question
                .default
                .as_ref()
                .and_then(json_type_for_value)
                .unwrap_or_else(|| "string".to_string());
            obj.insert("type".to_string(), Value::String(schema_type));
        }
        QuestionKind::Bool => {
            obj.insert("type".to_string(), Value::String("boolean".to_string()));
        }
        QuestionKind::Int => {
            obj.insert("type".to_string(), Value::String("integer".to_string()));
        }
        QuestionKind::Float => {
            obj.insert("type".to_string(), Value::String("number".to_string()));
        }
        QuestionKind::Choice => {
            if question.choices.is_empty() {
                let schema_type = question
                    .default
                    .as_ref()
                    .and_then(json_type_for_value)
                    .unwrap_or_else(|| "string".to_string());
                obj.insert("type".to_string(), Value::String(schema_type));
            } else {
                obj.insert("enum".to_string(), Value::Array(question.choices.clone()));
            }
        }
    }
    if let Some(default) = question.default.clone() {
        obj.insert("default".to_string(), default);
    }
    if !question.prompt.is_empty() {
        obj.insert(
            "description".to_string(),
            Value::String(question.prompt.clone()),
        );
    }
    Value::Object(obj)
}

fn default_value_for_question(question: &Question) -> Value {
    match question.kind {
        QuestionKind::Bool => Value::Bool(false),
        QuestionKind::Int => Value::Number(0.into()),
        QuestionKind::Float => {
            Value::Number(serde_json::Number::from_f64(0.0).unwrap_or_else(|| 0.into()))
        }
        QuestionKind::Choice => question
            .choices
            .first()
            .cloned()
            .unwrap_or_else(|| Value::String(String::new())),
        QuestionKind::String => Value::String(String::new()),
    }
}

fn json_type_for_value(value: &Value) -> Option<String> {
    match value {
        Value::String(_) => Some("string".to_string()),
        Value::Bool(_) => Some("boolean".to_string()),
        Value::Number(num) => {
            if num.is_i64() || num.is_u64() {
                Some("integer".to_string())
            } else {
                Some("number".to_string())
            }
        }
        Value::Array(_) => Some("array".to_string()),
        Value::Object(_) => Some("object".to_string()),
        Value::Null => None,
    }
}

fn question_visible(
    question: &Question,
    answers: &std::collections::HashMap<String, Value>,
) -> bool {
    let Some(show_if) = &question.show_if else {
        return true;
    };
    match show_if {
        Value::Bool(value) => *value,
        Value::Object(map) => {
            let Some(id) = map.get("id").and_then(Value::as_str) else {
                return true;
            };
            let Some(expected) = map.get("equals") else {
                return true;
            };
            let Some(actual) = answers.get(id) else {
                return false;
            };
            actual == expected
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::questions::{Question, QuestionKind};
    use serde_json::json;

    fn validate(schema: &Value, instance: &Value) -> bool {
        let compiled = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .build(schema)
            .expect("compile schema");
        compiled.validate(instance).is_ok()
    }

    #[test]
    fn example_validates_and_is_deterministic() {
        let questions = vec![
            Question {
                id: "mode".to_string(),
                prompt: "Mode".to_string(),
                kind: QuestionKind::Choice,
                required: true,
                default: Some(json!("asset")),
                choices: vec![json!("asset"), json!("url")],
                show_if: None,
                writes_to: None,
            },
            Question {
                id: "asset_path".to_string(),
                prompt: "Asset".to_string(),
                kind: QuestionKind::String,
                required: true,
                default: None,
                choices: Vec::new(),
                show_if: Some(json!({ "id": "mode", "equals": "asset" })),
                writes_to: None,
            },
            Question {
                id: "enabled".to_string(),
                prompt: "Enabled".to_string(),
                kind: QuestionKind::Bool,
                required: false,
                default: Some(json!(true)),
                choices: Vec::new(),
                show_if: None,
                writes_to: None,
            },
        ];

        let schema = schema_for_questions(&questions);
        let example = example_for_questions(&questions);
        let example_again = example_for_questions(&questions);

        assert_eq!(example, example_again);
        assert!(validate(&schema, &example));
    }

    #[test]
    fn schema_marks_unconditional_required_fields() {
        let questions = vec![Question {
            id: "name".to_string(),
            prompt: "Name".to_string(),
            kind: QuestionKind::String,
            required: true,
            default: None,
            choices: Vec::new(),
            show_if: Some(json!(true)),
            writes_to: None,
        }];

        let schema = schema_for_questions(&questions);
        assert_eq!(schema.get("additionalProperties"), Some(&json!(false)));
        assert_eq!(schema.get("type"), Some(&json!("object")));
        assert_eq!(schema.get("required"), Some(&json!(["name"])));
    }
}
