use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{self, Read, Write};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionKind {
    String,
    Bool,
    Choice,
    Int,
    Float,
}

#[derive(Debug, Clone)]
pub struct Question {
    pub id: String,
    pub prompt: String,
    pub kind: QuestionKind,
    pub required: bool,
    pub default: Option<Value>,
    pub choices: Vec<Value>,
    pub show_if: Option<Value>,
    pub writes_to: Option<String>,
}

pub type Answers = HashMap<String, Value>;

#[derive(Debug, Clone)]
pub struct MissingRequired {
    pub missing: Vec<String>,
    pub template: String,
}

impl std::fmt::Display for MissingRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "missing required answers: {}. Provide via --answers/--answers-file. Example:\n{}",
            self.missing.join(", "),
            self.template
        )
    }
}

impl std::error::Error for MissingRequired {}

pub fn merge_answers(cli_answers: Option<Answers>, file_answers: Option<Answers>) -> Answers {
    let mut merged = Answers::new();
    if let Some(cli) = cli_answers {
        merged.extend(cli);
    }
    if let Some(file) = file_answers {
        merged.extend(file);
    }
    merged
}

pub fn validate_required(questions: &[Question], answers: &Answers) -> Result<()> {
    let missing = missing_required(questions, answers);
    if missing.is_empty() {
        return Ok(());
    }
    let template = serde_json::to_string_pretty(&template_for_questions(questions, answers))
        .unwrap_or_else(|_| "{}".to_string());
    Err(MissingRequired { missing, template }.into())
}

pub fn run_interactive(questions: &[Question]) -> Result<Answers> {
    run_interactive_with_seed(questions, Answers::new())
}

pub fn run_interactive_with_seed(questions: &[Question], seed: Answers) -> Result<Answers> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    run_interactive_with_io(questions, seed, stdin.lock(), stdout.lock())
}

pub fn run_interactive_with_io<R: Read, W: Write>(
    questions: &[Question],
    mut answers: Answers,
    mut reader: R,
    mut writer: W,
) -> Result<Answers> {
    let mut input = String::new();
    for question in questions {
        if !question_visible(question, &answers) {
            continue;
        }
        if answers.contains_key(&question.id) {
            continue;
        }
        let effective_default = question.default.clone();
        loop {
            input.clear();
            write_prompt(&mut writer, question, effective_default.as_ref())?;
            writer.flush().ok();
            let read_any = read_line(&mut reader, &mut input)?;
            let raw = input.trim();
            if raw.is_empty() {
                if let Some(default) = effective_default.clone() {
                    answers.insert(question.id.clone(), default);
                    break;
                }
                if !read_any {
                    return Err(anyhow!(
                        "stdin closed while waiting for answer for '{}'",
                        question.id
                    ));
                }
                if question.required {
                    continue;
                }
                break;
            }
            match parse_answer(raw, question) {
                Ok(value) => {
                    answers.insert(question.id.clone(), value);
                    break;
                }
                Err(_) => {
                    continue;
                }
            }
        }
    }
    Ok(answers)
}

pub fn extract_questions_from_flow(flow: &Value) -> Result<Vec<Question>> {
    let Some(nodes) = flow.get("nodes").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let mut questions = Vec::new();
    for node in nodes.values() {
        let Some(qnode) = node.get("questions") else {
            continue;
        };
        let fields = qnode
            .get("fields")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("questions node missing fields array"))?;
        for field in fields {
            let id = field
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("questions field missing id"))?;
            let prompt = field
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_string();
            let default = field.get("default").cloned();
            let required = field
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(default.is_none());
            let kind = match field.get("type").and_then(Value::as_str) {
                Some("bool") | Some("boolean") => QuestionKind::Bool,
                Some("int") | Some("integer") => QuestionKind::Int,
                Some("float") | Some("number") => QuestionKind::Float,
                Some("choice") | Some("enum") => QuestionKind::Choice,
                _ => QuestionKind::String,
            };
            let choices = field
                .get("options")
                .and_then(Value::as_array)
                .map(|opts| opts.to_vec())
                .unwrap_or_default();
            let show_if = field.get("show_if").cloned();
            questions.push(Question {
                id: id.to_string(),
                prompt,
                kind,
                required,
                default,
                choices,
                show_if,
                writes_to: field
                    .get("writes_to")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string()),
            });
        }
    }
    Ok(questions)
}
fn write_prompt<W: Write>(
    writer: &mut W,
    question: &Question,
    default_override: Option<&Value>,
) -> Result<()> {
    write!(writer, "Question ({}): {}", question.id, question.prompt).context("write prompt")?;
    if let Some(default) = default_override.or(question.default.as_ref()) {
        write!(writer, " [default: {}]", display_value(default)).ok();
    }
    writeln!(writer).ok();
    if question.kind == QuestionKind::Choice && !question.choices.is_empty() {
        for (idx, choice) in question.choices.iter().enumerate() {
            writeln!(writer, "  {}) {}", idx + 1, display_value(choice)).ok();
        }
    }
    Ok(())
}

fn read_line<R: Read>(reader: &mut R, buf: &mut String) -> Result<bool> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    let mut read_any = false;
    while reader.read(&mut byte).context("read input")? == 1 {
        read_any = true;
        if byte[0] == b'\n' {
            break;
        }
        bytes.push(byte[0]);
    }
    *buf = String::from_utf8(bytes).context("parse input as UTF-8")?;
    Ok(read_any)
}

fn parse_answer(raw: &str, question: &Question) -> Result<Value> {
    match question.kind {
        QuestionKind::String => Ok(Value::String(raw.to_string())),
        QuestionKind::Bool => parse_bool(raw).map(Value::Bool),
        QuestionKind::Int => {
            let parsed = raw.parse::<i64>().map_err(|_| anyhow!("invalid integer"))?;
            Ok(Value::Number(parsed.into()))
        }
        QuestionKind::Float => {
            let parsed = raw.parse::<f64>().map_err(|_| anyhow!("invalid number"))?;
            let number =
                serde_json::Number::from_f64(parsed).ok_or_else(|| anyhow!("invalid number"))?;
            Ok(Value::Number(number))
        }
        QuestionKind::Choice => parse_choice(raw, question),
    }
}

fn parse_bool(raw: &str) -> Result<bool> {
    let lowered = raw.trim().to_lowercase();
    let compact: String = lowered.chars().filter(|c| !c.is_whitespace()).collect();
    match compact.as_str() {
        "yes=true" => Ok(true),
        "no=false" => Ok(false),
        "y" | "yes" | "true" | "1" => Ok(true),
        "n" | "no" | "false" | "0" => Ok(false),
        _ => Err(anyhow!("invalid boolean")),
    }
}

fn parse_choice(raw: &str, question: &Question) -> Result<Value> {
    if let Ok(idx) = raw.parse::<usize>()
        && idx >= 1
        && idx <= question.choices.len()
    {
        return Ok(question.choices[idx - 1].clone());
    }
    for choice in &question.choices {
        if display_value(choice) == raw {
            return Ok(choice.clone());
        }
    }
    Err(anyhow!("invalid choice"))
}

fn display_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub fn apply_writes_to(
    mut base: Value,
    questions: &[Question],
    answers: &Answers,
) -> Result<Value> {
    for question in questions {
        let Some(path) = question.writes_to.as_deref() else {
            continue;
        };
        let Some(answer) = answers.get(&question.id) else {
            continue;
        };
        let tokens = parse_path_tokens(path)?;
        set_value_at_path(&mut base, &tokens, answer.clone());
    }
    Ok(base)
}

pub fn extract_answers_from_payload(questions: &[Question], payload: &Value) -> Answers {
    let mut answers = Answers::new();
    for question in questions {
        let Some(path) = question.writes_to.as_deref() else {
            continue;
        };
        if let Ok(tokens) = parse_path_tokens(path)
            && let Some(value) = get_value_at_path(payload, &tokens)
        {
            answers.insert(question.id.clone(), value);
        }
    }
    answers
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PathToken {
    Key(String),
    Index(usize),
}

fn parse_path_tokens(path: &str) -> Result<Vec<PathToken>> {
    let mut tokens = Vec::new();
    let mut buf = String::new();
    let mut chars = path.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '.' => {
                if !buf.is_empty() {
                    tokens.push(PathToken::Key(std::mem::take(&mut buf)));
                }
            }
            '[' => {
                if !buf.is_empty() {
                    tokens.push(PathToken::Key(std::mem::take(&mut buf)));
                }
                let mut idx_buf = String::new();
                for c in chars.by_ref() {
                    if c == ']' {
                        break;
                    }
                    idx_buf.push(c);
                }
                let idx = idx_buf
                    .parse::<usize>()
                    .map_err(|_| anyhow!("invalid index in writes_to path"))?;
                tokens.push(PathToken::Index(idx));
            }
            _ => buf.push(ch),
        }
    }
    if !buf.is_empty() {
        tokens.push(PathToken::Key(buf));
    }
    if tokens.is_empty() {
        Err(anyhow!("writes_to path is empty"))
    } else {
        Ok(tokens)
    }
}

fn ensure_array_len(arr: &mut Vec<Value>, index: usize) {
    if arr.len() <= index {
        arr.resize(index + 1, Value::Null);
    }
}

fn set_value_at_path(target: &mut Value, tokens: &[PathToken], value: Value) {
    let mut current = target;
    for (i, token) in tokens.iter().enumerate() {
        let last = i == tokens.len() - 1;
        match token {
            PathToken::Key(key) => {
                if !current.is_object() {
                    *current = Value::Object(serde_json::Map::new());
                }
                let obj = current.as_object_mut().unwrap();
                if last {
                    obj.insert(key.clone(), value);
                    return;
                }
                current = obj.entry(key.clone()).or_insert(Value::Null);
            }
            PathToken::Index(index) => {
                if !current.is_array() {
                    *current = Value::Array(Vec::new());
                }
                let arr = current.as_array_mut().unwrap();
                ensure_array_len(arr, *index);
                if last {
                    arr[*index] = value;
                    return;
                }
                current = &mut arr[*index];
            }
        }
    }
}

fn get_value_at_path(target: &Value, tokens: &[PathToken]) -> Option<Value> {
    let mut current = target;
    for token in tokens {
        match token {
            PathToken::Key(key) => {
                current = current.as_object()?.get(key)?;
            }
            PathToken::Index(index) => {
                current = current.as_array()?.get(*index)?;
            }
        }
    }
    Some(current.clone())
}

fn missing_required(questions: &[Question], answers: &Answers) -> Vec<String> {
    questions
        .iter()
        .filter(|q| q.required && question_visible(q, answers) && !answers.contains_key(&q.id))
        .map(|q| q.id.clone())
        .collect::<Vec<_>>()
}

fn template_for_questions(questions: &[Question], answers: &Answers) -> Value {
    let mut obj = serde_json::Map::new();
    for question in questions {
        if !question_visible(question, answers) {
            continue;
        }
        let value = if let Some(default) = question.default.clone() {
            default
        } else {
            match question.kind {
                QuestionKind::Bool => Value::Bool(false),
                QuestionKind::Int => Value::Number(0.into()),
                QuestionKind::Float => Value::Number(
                    serde_json::Number::from_f64(0.0)
                        .unwrap_or_else(|| serde_json::Number::from(0)),
                ),
                QuestionKind::Choice => question
                    .choices
                    .first()
                    .cloned()
                    .unwrap_or_else(|| Value::String(String::new())),
                QuestionKind::String => Value::String(String::new()),
            }
        };
        obj.insert(question.id.clone(), value);
    }
    Value::Object(obj)
}

fn question_visible(question: &Question, answers: &Answers) -> bool {
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
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn interactive_accepts_default_on_empty() {
        let question = Question {
            id: "name".to_string(),
            prompt: "Name?".to_string(),
            kind: QuestionKind::String,
            required: true,
            default: Some(Value::String("Ada".to_string())),
            choices: Vec::new(),
            show_if: None,
            writes_to: None,
        };
        let input = Cursor::new("\n");
        let output = Vec::new();
        let answers = run_interactive_with_io(&[question], Answers::new(), input, output).unwrap();
        assert_eq!(answers.get("name"), Some(&Value::String("Ada".to_string())));
    }

    #[test]
    fn choice_accepts_index_or_value() {
        let question = Question {
            id: "color".to_string(),
            prompt: "Color?".to_string(),
            kind: QuestionKind::Choice,
            required: true,
            default: None,
            choices: vec![
                Value::String("red".to_string()),
                Value::String("blue".to_string()),
            ],
            show_if: None,
            writes_to: None,
        };
        let input = Cursor::new("2\n");
        let output = Vec::new();
        let answers = run_interactive_with_io(
            std::slice::from_ref(&question),
            Answers::new(),
            input,
            output,
        )
        .unwrap();
        assert_eq!(
            answers.get("color"),
            Some(&Value::String("blue".to_string()))
        );

        let input = Cursor::new("red\n");
        let output = Vec::new();
        let answers = run_interactive_with_io(&[question], Answers::new(), input, output).unwrap();
        assert_eq!(
            answers.get("color"),
            Some(&Value::String("red".to_string()))
        );
    }

    #[test]
    fn missing_required_reports_all_fields() {
        let questions = vec![
            Question {
                id: "a".to_string(),
                prompt: "A?".to_string(),
                kind: QuestionKind::String,
                required: true,
                default: None,
                choices: Vec::new(),
                show_if: None,
                writes_to: None,
            },
            Question {
                id: "b".to_string(),
                prompt: "B?".to_string(),
                kind: QuestionKind::String,
                required: true,
                default: None,
                choices: Vec::new(),
                show_if: None,
                writes_to: None,
            },
        ];
        let err = validate_required(&questions, &Answers::new()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("a"));
        assert!(msg.contains("b"));
        assert!(msg.contains("--answers"));
        assert!(msg.contains('{'));
    }

    #[test]
    fn interactive_parses_int_and_bool() {
        let questions = vec![
            Question {
                id: "count".to_string(),
                prompt: "Count?".to_string(),
                kind: QuestionKind::Int,
                required: true,
                default: None,
                choices: Vec::new(),
                show_if: None,
                writes_to: None,
            },
            Question {
                id: "flag".to_string(),
                prompt: "Flag?".to_string(),
                kind: QuestionKind::Bool,
                required: true,
                default: None,
                choices: Vec::new(),
                show_if: None,
                writes_to: None,
            },
        ];
        let input = Cursor::new("42\ny\n");
        let output = Vec::new();
        let answers = run_interactive_with_io(&questions, Answers::new(), input, output).unwrap();
        assert_eq!(answers.get("count"), Some(&Value::Number(42.into())));
        assert_eq!(answers.get("flag"), Some(&Value::Bool(true)));
    }

    #[test]
    fn interactive_accepts_yes_no_equals_true_false() {
        let questions = vec![
            Question {
                id: "enabled".to_string(),
                prompt: "Enabled?".to_string(),
                kind: QuestionKind::Bool,
                required: true,
                default: None,
                choices: Vec::new(),
                show_if: None,
                writes_to: None,
            },
            Question {
                id: "disabled".to_string(),
                prompt: "Disabled?".to_string(),
                kind: QuestionKind::Bool,
                required: true,
                default: None,
                choices: Vec::new(),
                show_if: None,
                writes_to: None,
            },
        ];
        let input = Cursor::new("YeS = TrUe\nNo = False\n");
        let output = Vec::new();
        let answers = run_interactive_with_io(&questions, Answers::new(), input, output).unwrap();
        assert_eq!(answers.get("enabled"), Some(&Value::Bool(true)));
        assert_eq!(answers.get("disabled"), Some(&Value::Bool(false)));
    }

    #[test]
    fn interactive_respects_show_if_equals() {
        let questions = vec![
            Question {
                id: "mode".to_string(),
                prompt: "Mode?".to_string(),
                kind: QuestionKind::String,
                required: true,
                default: Some(Value::String("asset".to_string())),
                choices: Vec::new(),
                show_if: None,
                writes_to: None,
            },
            Question {
                id: "asset_path".to_string(),
                prompt: "Asset?".to_string(),
                kind: QuestionKind::String,
                required: true,
                default: None,
                choices: Vec::new(),
                show_if: Some(json!({ "id": "mode", "equals": "asset" })),
                writes_to: None,
            },
        ];
        let input = Cursor::new("\npath.json\n");
        let output = Vec::new();
        let answers = run_interactive_with_io(&questions, Answers::new(), input, output).unwrap();
        assert_eq!(
            answers.get("mode"),
            Some(&Value::String("asset".to_string()))
        );
        assert_eq!(
            answers.get("asset_path"),
            Some(&Value::String("path.json".to_string()))
        );

        let input = Cursor::new("inline\n");
        let output = Vec::new();
        let answers = run_interactive_with_io(&questions, Answers::new(), input, output).unwrap();
        assert_eq!(
            answers.get("mode"),
            Some(&Value::String("inline".to_string()))
        );
        assert!(!answers.contains_key("asset_path"));
    }

    #[test]
    fn validate_required_skips_hidden_questions() {
        let questions = vec![Question {
            id: "hidden".to_string(),
            prompt: "Hidden?".to_string(),
            kind: QuestionKind::String,
            required: true,
            default: None,
            choices: Vec::new(),
            show_if: Some(Value::Bool(false)),
            writes_to: None,
        }];
        validate_required(&questions, &Answers::new()).unwrap();
    }

    #[test]
    fn writes_to_creates_nested_objects() {
        let questions = vec![Question {
            id: "asset_path".to_string(),
            prompt: "Asset?".to_string(),
            kind: QuestionKind::String,
            required: true,
            default: None,
            choices: Vec::new(),
            show_if: None,
            writes_to: Some("card_spec.asset_path".to_string()),
        }];
        let mut answers = Answers::new();
        answers.insert(
            "asset_path".to_string(),
            Value::String("path.json".to_string()),
        );
        let output = apply_writes_to(Value::Object(Default::default()), &questions, &answers)
            .expect("apply");
        let card_spec = output.get("card_spec").and_then(Value::as_object).unwrap();
        assert_eq!(
            card_spec.get("asset_path").and_then(Value::as_str),
            Some("path.json")
        );
    }

    #[test]
    fn writes_to_supports_array_indexes() {
        let questions = vec![Question {
            id: "action_id".to_string(),
            prompt: "Action?".to_string(),
            kind: QuestionKind::String,
            required: true,
            default: None,
            choices: Vec::new(),
            show_if: None,
            writes_to: Some("actions[0].id".to_string()),
        }];
        let mut answers = Answers::new();
        answers.insert(
            "action_id".to_string(),
            Value::String("action-1".to_string()),
        );
        let output = apply_writes_to(Value::Object(Default::default()), &questions, &answers)
            .expect("apply");
        let actions = output.get("actions").and_then(Value::as_array).unwrap();
        let first = actions[0].as_object().unwrap();
        assert_eq!(first.get("id").and_then(Value::as_str), Some("action-1"));
    }
}
