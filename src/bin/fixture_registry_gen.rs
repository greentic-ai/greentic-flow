use greentic_types::cbor::canonical;
use greentic_types::i18n_text::I18nText;
use greentic_types::schemas::common::schema_ir::{AdditionalProperties, SchemaIr};
use greentic_types::schemas::component::v0_6_0::{
    ComponentDescribe, ComponentInfo, ComponentOperation, ComponentQaSpec, ComponentRunInput,
    ComponentRunOutput, QaMode, schema_hash,
};
use serde_json::json;
use std::fs;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("registry");
    let component_dir = root.join("components").join("acme_widget_1");
    fs::create_dir_all(&component_dir)?;

    let index = json!({
        "components": {
            "oci://acme/widget:1": {
                "path": "components/acme_widget_1",
                "abi_version": "0.6.0"
            }
        }
    });
    fs::write(root.join("index.json"), index.to_string())?;

    let config_schema = SchemaIr::Object {
        properties: std::collections::BTreeMap::new(),
        required: Vec::new(),
        additional: AdditionalProperties::Allow,
    };
    let op_schema = SchemaIr::Object {
        properties: std::collections::BTreeMap::new(),
        required: Vec::new(),
        additional: AdditionalProperties::Allow,
    };
    let op_schema_hash = schema_hash(&op_schema, &op_schema, &config_schema).unwrap();
    let describe = ComponentDescribe {
        info: ComponentInfo {
            id: "acme.widget".to_string(),
            version: "0.1.0".to_string(),
            role: "tool".to_string(),
            display_name: None,
        },
        provided_capabilities: Vec::new(),
        required_capabilities: Vec::new(),
        metadata: std::collections::BTreeMap::new(),
        operations: vec![ComponentOperation {
            id: "run".to_string(),
            display_name: None,
            input: ComponentRunInput {
                schema: op_schema.clone(),
            },
            output: ComponentRunOutput { schema: op_schema },
            defaults: std::collections::BTreeMap::new(),
            redactions: Vec::new(),
            constraints: std::collections::BTreeMap::new(),
            schema_hash: op_schema_hash,
        }],
        config_schema,
    };
    let describe_cbor = canonical::to_canonical_cbor_allow_floats(&describe)?;
    fs::write(component_dir.join("describe.cbor"), describe_cbor)?;

    for (mode, config) in [
        (QaMode::Default, json!({"foo":"bar"})),
        (QaMode::Upgrade, json!({"foo":"updated"})),
        (QaMode::Remove, json!({"foo":"removed"})),
    ] {
        let spec = ComponentQaSpec {
            mode: mode.clone(),
            title: I18nText::new("title", Some("Fixture Wizard".to_string())),
            description: None,
            questions: Vec::new(),
            defaults: std::collections::BTreeMap::new(),
        };
        let qa_spec_cbor = canonical::to_canonical_cbor(&spec)?;
        let mode_str = match mode {
            QaMode::Default => "default",
            QaMode::Setup => "setup",
            QaMode::Upgrade => "upgrade",
            QaMode::Remove => "remove",
        };
        fs::write(
            component_dir.join(format!("qa_{mode_str}.cbor")),
            qa_spec_cbor,
        )?;
        let apply_cbor = canonical::to_canonical_cbor(&config)?;
        fs::write(
            component_dir.join(format!("apply_{mode_str}_config.cbor")),
            apply_cbor,
        )?;
    }

    println!("Generated fixtures under {}", root.display());
    Ok(())
}
