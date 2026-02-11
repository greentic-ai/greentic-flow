use crate::error::{FlowError, FlowErrorLocation, Result};
use crate::i18n::{I18nCatalog, resolve_text};
use greentic_types::schemas::component::v0_6_0::{ComponentQaSpec, QuestionKind};
use qa_spec::FormSpec;
use qa_spec::spec::question::{QuestionSpec, QuestionType};
use serde_json::{Map, Number, Value};
use std::collections::HashMap;
use std::io::{self, Write};

pub fn warn_unknown_keys(answers: &HashMap<String, Value>, spec: &ComponentQaSpec) {
    let mut known = std::collections::BTreeSet::new();
    for question in &spec.questions {
        known.insert(question.id.as_str());
    }
    let mut unknown = Vec::new();
    for key in answers.keys() {
        if !known.contains(key.as_str()) {
            unknown.push(key.clone());
        }
    }
    if !unknown.is_empty() {
        eprintln!(
            "warning: answers include unknown keys: {}",
            unknown.join(", ")
        );
    }
}

pub fn run_interactive(
    spec: &ComponentQaSpec,
    catalog: &I18nCatalog,
    locale: &str,
    mut answers: HashMap<String, Value>,
) -> Result<HashMap<String, Value>> {
    let form = component_spec_to_form(spec, catalog, locale);
    println!("{}", resolve_text(&spec.title, catalog, locale));
    if let Some(desc) = spec.description.as_ref() {
        println!("{}", resolve_text(desc, catalog, locale));
    }

    for question in &spec.questions {
        if answers.contains_key(&question.id) {
            continue;
        }
        loop {
            let label = resolve_text(&question.label, catalog, locale);
            if let Some(help) = question.help.as_ref() {
                println!("{} ({})", label, resolve_text(help, catalog, locale));
            } else {
                println!("{label}");
            }
            let prompt = match &question.kind {
                QuestionKind::Choice { options } => {
                    let mut idx = 1usize;
                    for option in options {
                        let option_label = resolve_text(&option.label, catalog, locale);
                        println!("  {idx}. {option_label} ({})", option.value);
                        idx += 1;
                    }
                    "Select option".to_string()
                }
                QuestionKind::Bool => "Enter true/false".to_string(),
                QuestionKind::Number => "Enter number".to_string(),
                QuestionKind::Text => "Enter text".to_string(),
            };
            let default = question
                .default
                .as_ref()
                .and_then(|value| crate::wizard_ops::cbor_value_to_json(value).ok());
            let raw = prompt_line(&prompt, default.as_ref())?;
            let value = if raw.trim().is_empty() {
                if let Some(default) = default.clone() {
                    default
                } else if question.required {
                    println!("This field is required.");
                    continue;
                } else {
                    Value::Null
                }
            } else {
                parse_answer(&question.kind, &raw)?
            };
            if value.is_null() && question.required {
                println!("This field is required.");
                continue;
            }
            answers.insert(question.id.clone(), value);
            if validate_answers_with_form(&form, &answers, true)? {
                break;
            }
        }
    }
    Ok(answers)
}

pub fn validate_required(
    spec: &ComponentQaSpec,
    catalog: &I18nCatalog,
    locale: &str,
    answers: &HashMap<String, Value>,
) -> Result<()> {
    let form = component_spec_to_form(spec, catalog, locale);
    let _ = validate_answers_with_form(&form, answers, false)?;
    Ok(())
}

fn validate_answers_with_form(
    form: &FormSpec,
    answers: &HashMap<String, Value>,
    allow_incomplete: bool,
) -> Result<bool> {
    let value = Value::Object(map_from_answers(answers));
    let result = qa_spec::validate(form, &value);
    if result.valid {
        return Ok(true);
    }
    if !allow_incomplete && !result.missing_required.is_empty() {
        return Err(FlowError::Internal {
            message: format!(
                "missing required answers: {}",
                result.missing_required.join(", ")
            ),
            location: FlowErrorLocation::new(None, None, None),
        });
    }
    if !result.errors.is_empty() {
        let lines: Vec<String> = result
            .errors
            .iter()
            .map(|err| err.message.clone())
            .collect();
        return Err(FlowError::Internal {
            message: format!("answers failed validation: {}", lines.join("; ")),
            location: FlowErrorLocation::new(None, None, None),
        });
    }
    Ok(false)
}

fn map_from_answers(answers: &HashMap<String, Value>) -> Map<String, Value> {
    let mut map = Map::new();
    for (key, value) in answers {
        if !value.is_null() {
            map.insert(key.clone(), value.clone());
        }
    }
    map
}

fn component_spec_to_form(spec: &ComponentQaSpec, catalog: &I18nCatalog, locale: &str) -> FormSpec {
    let title = resolve_text(&spec.title, catalog, locale);
    let description = spec
        .description
        .as_ref()
        .map(|text| resolve_text(text, catalog, locale));
    let mut questions = Vec::new();
    for question in &spec.questions {
        let title = resolve_text(&question.label, catalog, locale);
        let description = question
            .help
            .as_ref()
            .map(|text| resolve_text(text, catalog, locale));
        let (kind, choices) = match &question.kind {
            QuestionKind::Text => (QuestionType::String, None),
            QuestionKind::Number => (QuestionType::Number, None),
            QuestionKind::Bool => (QuestionType::Boolean, None),
            QuestionKind::Choice { options } => {
                let list = options.iter().map(|opt| opt.value.clone()).collect();
                (QuestionType::Enum, Some(list))
            }
        };
        let default_value = question
            .default
            .as_ref()
            .and_then(|value| crate::wizard_ops::cbor_value_to_json(value).ok())
            .map(|value| value.to_string());
        questions.push(QuestionSpec {
            id: question.id.clone(),
            kind,
            title,
            description,
            required: question.required,
            choices,
            default_value,
            secret: false,
            visible_if: None,
            constraint: None,
            list: None,
            computed: None,
            policy: Default::default(),
            computed_overridable: false,
        });
    }
    FormSpec {
        id: "component-wizard".to_string(),
        title,
        version: "0.6.0".to_string(),
        description,
        presentation: None,
        progress_policy: None,
        secrets_policy: None,
        store: Vec::new(),
        validations: Vec::new(),
        questions,
    }
}

fn parse_answer(kind: &QuestionKind, raw: &str) -> Result<Value> {
    let trimmed = raw.trim();
    match kind {
        QuestionKind::Text => Ok(Value::String(trimmed.to_string())),
        QuestionKind::Number => {
            let number: f64 = trimmed.parse().map_err(|err| FlowError::Internal {
                message: format!("invalid number: {err}"),
                location: FlowErrorLocation::new(None, None, None),
            })?;
            Number::from_f64(number)
                .map(Value::Number)
                .ok_or_else(|| FlowError::Internal {
                    message: "number out of range".to_string(),
                    location: FlowErrorLocation::new(None, None, None),
                })
        }
        QuestionKind::Bool => {
            let lower = trimmed.to_ascii_lowercase();
            let value = matches!(lower.as_str(), "true" | "t" | "yes" | "y" | "1");
            Ok(Value::Bool(value))
        }
        QuestionKind::Choice { options } => {
            if let Ok(idx) = trimmed.parse::<usize>()
                && idx > 0
                && idx <= options.len()
            {
                return Ok(Value::String(options[idx - 1].value.clone()));
            }
            let matched = options
                .iter()
                .find(|opt| opt.value == trimmed)
                .map(|opt| opt.value.clone())
                .ok_or_else(|| FlowError::Internal {
                    message: "invalid choice".to_string(),
                    location: FlowErrorLocation::new(None, None, None),
                })?;
            Ok(Value::String(matched))
        }
    }
}

fn prompt_line(prompt: &str, default: Option<&Value>) -> Result<String> {
    let mut line = String::new();
    if let Some(default) = default {
        print!("{prompt} [{default}]: ");
    } else {
        print!("{prompt}: ");
    }
    io::stdout().flush().map_err(|err| FlowError::Internal {
        message: format!("flush stdout: {err}"),
        location: FlowErrorLocation::new(None, None, None),
    })?;
    io::stdin()
        .read_line(&mut line)
        .map_err(|err| FlowError::Internal {
            message: format!("read input: {err}"),
            location: FlowErrorLocation::new(None, None, None),
        })?;
    Ok(line.trim_end().to_string())
}
