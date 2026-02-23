use greentic_flow::wizard::{
    ApplyOptions, MODE_NEW, MODE_SCAFFOLD, ProviderContext, execute_plan, wizard_provider,
};
use insta::assert_snapshot;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use tempfile::tempdir;

fn base_answers(path: &str, scaffold: bool, variant: &str) -> HashMap<String, Value> {
    let mut answers = HashMap::new();
    answers.insert("flow.name".to_string(), json!("main"));
    answers.insert("flow.path".to_string(), json!(path));
    answers.insert("flow.kind".to_string(), json!("messaging"));
    answers.insert("flow.entrypoint".to_string(), json!("start"));
    answers.insert("flow.nodes.scaffold".to_string(), json!(scaffold));
    answers.insert("flow.nodes.variant".to_string(), json!(variant));
    answers
}

#[test]
fn snapshot_answers_to_plan_json() {
    let provider = wizard_provider();
    let answers = base_answers("flows/main.ygtc", true, "start-log-end");
    let plan = provider
        .apply(
            MODE_SCAFFOLD,
            &ProviderContext {
                root_dir: PathBuf::new(),
            },
            &answers,
            &ApplyOptions { validate: true },
        )
        .expect("plan");

    let rendered = serde_json::to_string_pretty(&plan).expect("serialize plan");
    assert_snapshot!(rendered, @r#"
    {
      "mode": "scaffold",
      "validate": true,
      "steps": [
        {
          "ensure-dir": {
            "path": "flows"
          }
        },
        {
          "write-file": {
            "path": "flows/main.ygtc",
            "content": "id: main\ntype: messaging\nparameters: {}\ntags: []\nschema_version: 2\nentrypoints:\n  default: start\nnodes:\n  start:\n    routing:\n    - to: log\n    template: '{\"stage\":\"start\"}'\n  log:\n    routing:\n    - to: end\n    template: '{\"stage\":\"log\",\"message\":\"payload\"}'\n  end:\n    routing:\n    - out: true\n    template: '{\"stage\":\"end\"}'\n"
          }
        },
        {
          "validate-flow": {
            "path": "flows/main.ygtc"
          }
        }
      ]
    }
    "#);
}

#[test]
fn execute_plan_creates_valid_flow() {
    let provider = wizard_provider();
    let temp = tempdir().expect("tempdir");
    let answers = base_answers("generated/flow.ygtc", true, "start-end");
    let plan = provider
        .apply(
            MODE_NEW,
            &ProviderContext {
                root_dir: temp.path().to_path_buf(),
            },
            &answers,
            &ApplyOptions { validate: true },
        )
        .expect("plan");

    execute_plan(&plan).expect("execute plan");

    let flow_path = temp.path().join("generated/flow.ygtc");
    let doc = greentic_flow::loader::load_ygtc_from_path(&flow_path).expect("load flow");
    assert_eq!(doc.id, "main");
    assert_eq!(doc.flow_type, "messaging");
    assert!(doc.nodes.contains_key("start"));
    assert!(doc.nodes.contains_key("end"));
}
