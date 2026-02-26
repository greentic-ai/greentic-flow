use anyhow::{Context, Result, anyhow};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::model::{FlowDoc, NodeDoc};

pub const MODE_SCAFFOLD: &str = "scaffold";
pub const MODE_NEW: &str = "new";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WizardPlanStep {
    EnsureDir { path: PathBuf },
    WriteFile { path: PathBuf, content: String },
    ValidateFlow { path: PathBuf },
    RunCommand { command: String, args: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WizardPlan {
    pub mode: String,
    pub validate: bool,
    pub steps: Vec<WizardPlanStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FlowQuestionKind {
    String,
    Bool,
    Choice,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlowQuestionSpec {
    pub id: String,
    pub prompt: String,
    pub kind: FlowQuestionKind,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QaSpec {
    pub mode: String,
    pub questions: Vec<FlowQuestionSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApplyOptions {
    pub validate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderContext {
    pub root_dir: PathBuf,
}

impl Default for ProviderContext {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("."),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct FlowScaffoldWizardProvider;

pub fn wizard_provider() -> FlowScaffoldWizardProvider {
    FlowScaffoldWizardProvider
}

impl FlowScaffoldWizardProvider {
    pub fn id(&self) -> &'static str {
        "greentic-flow.scaffold"
    }

    pub fn spec(&self, mode: &str, _ctx: &ProviderContext) -> Result<QaSpec> {
        validate_mode(mode)?;
        Ok(QaSpec {
            mode: mode.to_string(),
            questions: vec![
                FlowQuestionSpec {
                    id: "flow.name".to_string(),
                    prompt: "Flow id (used in the file content)".to_string(),
                    kind: FlowQuestionKind::String,
                    required: true,
                    default: None,
                    options: Vec::new(),
                },
                FlowQuestionSpec {
                    id: "flow.title".to_string(),
                    prompt: "Optional flow title".to_string(),
                    kind: FlowQuestionKind::String,
                    required: false,
                    default: None,
                    options: Vec::new(),
                },
                FlowQuestionSpec {
                    id: "flow.description".to_string(),
                    prompt: "Optional flow description".to_string(),
                    kind: FlowQuestionKind::String,
                    required: false,
                    default: None,
                    options: Vec::new(),
                },
                FlowQuestionSpec {
                    id: "flow.path".to_string(),
                    prompt: "Flow file path (for example flows/main.ygtc)".to_string(),
                    kind: FlowQuestionKind::String,
                    required: true,
                    default: Some(Value::String("flows/main.ygtc".to_string())),
                    options: Vec::new(),
                },
                FlowQuestionSpec {
                    id: "flow.kind".to_string(),
                    prompt: "Flow kind".to_string(),
                    kind: FlowQuestionKind::Choice,
                    required: true,
                    default: Some(Value::String("messaging".to_string())),
                    options: vec![
                        Value::String("messaging".to_string()),
                        Value::String("events".to_string()),
                        Value::String("component-config".to_string()),
                        Value::String("job".to_string()),
                        Value::String("http".to_string()),
                    ],
                },
                FlowQuestionSpec {
                    id: "flow.entrypoint".to_string(),
                    prompt: "Default entrypoint node id".to_string(),
                    kind: FlowQuestionKind::String,
                    required: true,
                    default: Some(Value::String("start".to_string())),
                    options: Vec::new(),
                },
                FlowQuestionSpec {
                    id: "flow.nodes.scaffold".to_string(),
                    prompt: "Scaffold starter nodes".to_string(),
                    kind: FlowQuestionKind::Bool,
                    required: true,
                    default: Some(Value::Bool(false)),
                    options: Vec::new(),
                },
                FlowQuestionSpec {
                    id: "flow.nodes.variant".to_string(),
                    prompt: "Starter graph variant".to_string(),
                    kind: FlowQuestionKind::Choice,
                    required: true,
                    default: Some(Value::String("start-end".to_string())),
                    options: vec![
                        Value::String("start-end".to_string()),
                        Value::String("start-log-end".to_string()),
                    ],
                },
            ],
        })
    }

    pub fn apply(
        &self,
        mode: &str,
        ctx: &ProviderContext,
        answers: &HashMap<String, Value>,
        options: &ApplyOptions,
    ) -> Result<WizardPlan> {
        validate_mode(mode)?;
        let flow_name = required_str(answers, "flow.name")?;
        let flow_title = optional_str(answers, "flow.title");
        let flow_description = optional_str(answers, "flow.description");
        let flow_kind = required_str(answers, "flow.kind")?;
        let flow_path = required_str(answers, "flow.path")?;
        let entrypoint = answers
            .get("flow.entrypoint")
            .and_then(Value::as_str)
            .unwrap_or("start");
        let scaffold_nodes = answers
            .get("flow.nodes.scaffold")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let variant = answers
            .get("flow.nodes.variant")
            .and_then(Value::as_str)
            .unwrap_or("start-end");

        let flow_rel = PathBuf::from(flow_path);
        let flow_file = ctx.root_dir.join(&flow_rel);
        let mut doc = FlowDoc {
            id: flow_name.to_string(),
            title: flow_title.map(ToOwned::to_owned),
            description: flow_description.map(ToOwned::to_owned),
            flow_type: flow_kind.to_string(),
            start: None,
            parameters: Value::Object(Default::default()),
            tags: Vec::new(),
            schema_version: Some(2),
            entrypoints: IndexMap::new(),
            meta: None,
            nodes: IndexMap::new(),
        };

        if scaffold_nodes {
            doc.entrypoints
                .insert("default".to_string(), Value::String(entrypoint.to_string()));
            for (id, node) in starter_nodes(variant, entrypoint)? {
                doc.nodes.insert(id, node);
            }
        }

        let mut yaml = serde_yaml_bw::to_string(&doc).context("serialize scaffold flow")?;
        if !yaml.ends_with('\n') {
            yaml.push('\n');
        }

        let mut steps = Vec::new();
        if let Some(parent) = flow_file.parent()
            && !parent.as_os_str().is_empty()
        {
            steps.push(WizardPlanStep::EnsureDir {
                path: parent.to_path_buf(),
            });
        }
        steps.push(WizardPlanStep::WriteFile {
            path: flow_file.clone(),
            content: yaml,
        });
        if options.validate {
            steps.push(WizardPlanStep::ValidateFlow { path: flow_file });
        }

        Ok(WizardPlan {
            mode: mode.to_string(),
            validate: options.validate,
            steps,
        })
    }
}

pub fn execute_plan(plan: &WizardPlan) -> Result<()> {
    for step in &plan.steps {
        match step {
            WizardPlanStep::EnsureDir { path } => {
                fs::create_dir_all(path)
                    .with_context(|| format!("create scaffold directory {}", path.display()))?;
            }
            WizardPlanStep::WriteFile { path, content } => {
                if let Some(parent) = path.parent()
                    && !parent.as_os_str().is_empty()
                {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("create parent directory {}", parent.display()))?;
                }
                fs::write(path, content)
                    .with_context(|| format!("write scaffold flow {}", path.display()))?;
            }
            WizardPlanStep::ValidateFlow { path } => {
                validate_flow_file(path)?;
            }
            WizardPlanStep::RunCommand { command, .. } => {
                return Err(anyhow!(
                    "run-command execution is not implemented in-process (command: {command})"
                ));
            }
        }
    }
    Ok(())
}

fn validate_flow_file(path: &Path) -> Result<()> {
    let doc = crate::loader::load_ygtc_from_path(path)
        .map_err(|err| anyhow!("load scaffolded flow {}: {err}", path.display()))?;
    let compiled = crate::compile_flow(doc)
        .map_err(|err| anyhow!("compile scaffolded flow {}: {err}", path.display()))?;
    let lint_errors = crate::lint::lint_builtin_rules(&compiled);
    if lint_errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "scaffolded flow {} failed builtin lint: {}",
            path.display(),
            lint_errors.join("; ")
        ))
    }
}

fn starter_nodes(variant: &str, entrypoint: &str) -> Result<Vec<(String, NodeDoc)>> {
    if entrypoint.trim().is_empty() {
        return Err(anyhow!(
            "flow.entrypoint cannot be empty when scaffolding nodes"
        ));
    }

    let end_id = "end".to_string();
    let mut nodes = Vec::new();

    match variant {
        "start-end" => {
            nodes.push((
                entrypoint.to_string(),
                template_node("{\"stage\":\"start\"}", vec![route_to(&end_id)]),
            ));
            nodes.push((
                end_id,
                template_node("{\"stage\":\"end\"}", vec![route_out()]),
            ));
        }
        "start-log-end" => {
            let log_id = "log".to_string();
            nodes.push((
                entrypoint.to_string(),
                template_node("{\"stage\":\"start\"}", vec![route_to(&log_id)]),
            ));
            nodes.push((
                log_id,
                template_node(
                    "{\"stage\":\"log\",\"message\":\"payload\"}",
                    vec![route_to("end")],
                ),
            ));
            nodes.push((
                end_id,
                template_node("{\"stage\":\"end\"}", vec![route_out()]),
            ));
        }
        other => {
            return Err(anyhow!(
                "unsupported flow.nodes.variant '{other}'; expected start-end or start-log-end"
            ));
        }
    }

    Ok(nodes)
}

fn template_node(template: &str, routing: Vec<Value>) -> NodeDoc {
    let mut raw = IndexMap::new();
    raw.insert("template".to_string(), Value::String(template.to_string()));
    NodeDoc {
        routing: Value::Array(routing),
        telemetry: None,
        operation: Some("template".to_string()),
        payload: Value::String(template.to_string()),
        raw,
    }
}

fn route_to(to: &str) -> Value {
    serde_json::json!({ "to": to })
}

fn route_out() -> Value {
    serde_json::json!({ "out": true })
}

fn validate_mode(mode: &str) -> Result<()> {
    if matches!(mode, MODE_SCAFFOLD | MODE_NEW) {
        Ok(())
    } else {
        Err(anyhow!(
            "unsupported wizard mode '{mode}'; expected '{MODE_SCAFFOLD}' or '{MODE_NEW}'"
        ))
    }
}

fn required_str<'a>(answers: &'a HashMap<String, Value>, key: &str) -> Result<&'a str> {
    answers
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow!("missing required answer '{key}'"))
}

fn optional_str<'a>(answers: &'a HashMap<String, Value>, key: &str) -> Option<&'a str> {
    answers
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_contains_stable_question_ids() {
        let provider = wizard_provider();
        let spec = provider
            .spec(MODE_SCAFFOLD, &ProviderContext::default())
            .unwrap();
        let ids: Vec<&str> = spec.questions.iter().map(|q| q.id.as_str()).collect();
        assert!(ids.contains(&"flow.name"));
        assert!(ids.contains(&"flow.path"));
        assert!(ids.contains(&"flow.entrypoint"));
        assert!(ids.contains(&"flow.kind"));
        assert!(ids.iter().any(|id| id.starts_with("flow.nodes.")));
    }
}
