use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, anyhow};
use serde_json::Value as JsonValue;

use crate::i18n::{I18nCatalog, resolve_text};
use greentic_interfaces::canonical::node::{ComponentDescriptor, SchemaSource};
use greentic_types::cbor::canonical;
use greentic_types::schemas::component::v0_6_0::{ComponentQaSpec, QaMode, QuestionKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardAbi {
    V6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardMode {
    Default,
    Setup,
    Update,
    Remove,
}

impl WizardMode {
    pub fn as_str(self) -> &'static str {
        match self {
            WizardMode::Default => "default",
            WizardMode::Setup => "setup",
            WizardMode::Update => "update",
            WizardMode::Remove => "remove",
        }
    }

    pub fn as_qa_mode(self) -> QaMode {
        match self {
            WizardMode::Default => QaMode::Default,
            WizardMode::Setup => QaMode::Setup,
            WizardMode::Update => QaMode::Update,
            WizardMode::Remove => QaMode::Remove,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WizardOutput {
    pub abi: WizardAbi,
    pub describe_cbor: Vec<u8>,
    pub descriptor: Option<ComponentDescriptor>,
    pub qa_spec_cbor: Vec<u8>,
    pub answers_cbor: Vec<u8>,
    pub config_cbor: Vec<u8>,
}

#[cfg(not(target_arch = "wasm32"))]
pub struct WizardSpecOutput {
    pub abi: WizardAbi,
    pub describe_cbor: Vec<u8>,
    pub descriptor: Option<ComponentDescriptor>,
    pub qa_spec_cbor: Vec<u8>,
    pub answers_schema_cbor: Option<Vec<u8>>,
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(unsafe_code)]
mod host {
    use super::*;
    use greentic_interfaces::canonical::node as canonical_node;
    use wasmtime::component::{Component, Linker};
    use wasmtime::{Config, Engine, Store, StoreContextMut};

    mod runtime {
        pub use greentic_interfaces::component_v0_6::exports::greentic::component::node;
        pub use greentic_interfaces::component_v0_6::greentic::types_core::core;
        pub type RuntimeComponent = greentic_interfaces::component_v0_6::Component;
    }

    fn build_engine() -> Result<Engine> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        Engine::new(&config).map_err(|err| anyhow!("init wasm engine: {err}"))
    }

    fn add_control_imports(linker: &mut Linker<()>) -> Result<()> {
        let mut inst = linker
            .instance("greentic:component/control@0.6.0")
            .map_err(|err| anyhow!("link control import: {err}"))?;
        inst.func_wrap(
            "should-cancel",
            |_caller: StoreContextMut<'_, ()>, (): ()| -> wasmtime::Result<(bool,)> {
                Ok((false,))
            },
        )
        .map_err(|err| anyhow!("link control.should-cancel: {err}"))?;
        inst.func_wrap(
            "yield-now",
            |_caller: StoreContextMut<'_, ()>, (): ()| -> wasmtime::Result<()> { Ok(()) },
        )
        .map_err(|err| anyhow!("link control.yield-now: {err}"))?;
        Ok(())
    }

    fn schema_source_to_cbor(source: &SchemaSource, label: &str) -> Result<Vec<u8>> {
        match source {
            SchemaSource::InlineCbor(bytes) => Ok(bytes.clone()),
            SchemaSource::CborSchemaId(id) => Err(anyhow!(
                "{label} uses cbor-schema-id '{id}', but greentic-flow requires inline-cbor for wizard execution"
            )),
            SchemaSource::RefPackPath(path) => Err(anyhow!(
                "{label} uses ref-pack-path '{path}', but greentic-flow requires inline-cbor for wizard execution"
            )),
            SchemaSource::RefUri(uri) => Err(anyhow!(
                "{label} uses ref-uri '{uri}', but greentic-flow requires inline-cbor for wizard execution"
            )),
        }
    }

    fn extract_setup_contract(
        descriptor: &ComponentDescriptor,
    ) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
        let qa_ref = crate::component_setup::qa_spec_ref(descriptor)
            .ok_or_else(|| anyhow!("component descriptor missing setup.qa-spec"))?;
        let qa_spec_cbor = schema_source_to_cbor(qa_ref, "setup.qa-spec")?;
        let answers_schema_cbor = crate::component_setup::answers_schema_ref(descriptor)
            .map(|source| schema_source_to_cbor(source, "setup.answers-schema"))
            .transpose()?;
        Ok((qa_spec_cbor, answers_schema_cbor))
    }

    fn ensure_setup_apply_answers_op(descriptor: &ComponentDescriptor) -> Result<()> {
        if descriptor
            .ops
            .iter()
            .any(|op| op.name == "setup.apply_answers")
        {
            return Ok(());
        }
        Err(anyhow!(
            "component descriptor does not advertise required op 'setup.apply_answers'"
        ))
    }

    fn invoke_envelope(payload_cbor: Vec<u8>) -> runtime::node::InvocationEnvelope {
        runtime::node::InvocationEnvelope {
            ctx: runtime::core::TenantCtx {
                tenant_id: "local".to_string(),
                team_id: None,
                user_id: None,
                env_id: "local".to_string(),
                trace_id: "trace-local".to_string(),
                correlation_id: "corr-local".to_string(),
                deadline_ms: 0,
                attempt: 0,
                idempotency_key: None,
                i18n_id: "en-US".to_string(),
            },
            flow_id: "wizard-flow".to_string(),
            step_id: "wizard-step".to_string(),
            component_id: "component".to_string(),
            attempt: 0,
            payload_cbor,
            metadata_cbor: None,
        }
    }

    fn convert_schema_source(source: runtime::node::SchemaSource) -> canonical_node::SchemaSource {
        match source {
            runtime::node::SchemaSource::CborSchemaId(id) => {
                canonical_node::SchemaSource::CborSchemaId(id)
            }
            runtime::node::SchemaSource::InlineCbor(bytes) => {
                canonical_node::SchemaSource::InlineCbor(bytes)
            }
            runtime::node::SchemaSource::RefPackPath(path) => {
                canonical_node::SchemaSource::RefPackPath(path)
            }
            runtime::node::SchemaSource::RefUri(uri) => canonical_node::SchemaSource::RefUri(uri),
        }
    }

    fn convert_io_schema(schema: runtime::node::IoSchema) -> canonical_node::IoSchema {
        canonical_node::IoSchema {
            schema: convert_schema_source(schema.schema),
            content_type: schema.content_type,
            schema_version: schema.schema_version,
        }
    }

    fn convert_example(example: runtime::node::Example) -> canonical_node::Example {
        canonical_node::Example {
            title: example.title,
            input_cbor: example.input_cbor,
            output_cbor: example.output_cbor,
        }
    }

    fn convert_op(op: runtime::node::Op) -> canonical_node::Op {
        canonical_node::Op {
            name: op.name,
            summary: op.summary,
            input: convert_io_schema(op.input),
            output: convert_io_schema(op.output),
            examples: op.examples.into_iter().map(convert_example).collect(),
        }
    }

    fn convert_schema_ref(schema: runtime::node::SchemaRef) -> canonical_node::SchemaRef {
        canonical_node::SchemaRef {
            id: schema.id,
            content_type: schema.content_type,
            blake3_hash: schema.blake3_hash,
            version: schema.version,
            bytes: schema.bytes,
            uri: schema.uri,
        }
    }

    fn convert_setup_example(example: runtime::node::SetupExample) -> canonical_node::SetupExample {
        canonical_node::SetupExample {
            title: example.title,
            answers_cbor: example.answers_cbor,
        }
    }

    fn convert_setup_output(output: runtime::node::SetupOutput) -> canonical_node::SetupOutput {
        match output {
            runtime::node::SetupOutput::ConfigOnly => canonical_node::SetupOutput::ConfigOnly,
            runtime::node::SetupOutput::TemplateScaffold(scaffold) => {
                canonical_node::SetupOutput::TemplateScaffold(
                    canonical_node::SetupTemplateScaffold {
                        template_ref: scaffold.template_ref,
                        output_layout: scaffold.output_layout,
                    },
                )
            }
        }
    }

    fn convert_setup_contract(
        contract: runtime::node::SetupContract,
    ) -> canonical_node::SetupContract {
        canonical_node::SetupContract {
            qa_spec: convert_schema_source(contract.qa_spec),
            answers_schema: convert_schema_source(contract.answers_schema),
            examples: contract
                .examples
                .into_iter()
                .map(convert_setup_example)
                .collect(),
            outputs: contract
                .outputs
                .into_iter()
                .map(convert_setup_output)
                .collect(),
        }
    }

    fn convert_descriptor(descriptor: runtime::node::ComponentDescriptor) -> ComponentDescriptor {
        ComponentDescriptor {
            name: descriptor.name,
            version: descriptor.version,
            summary: descriptor.summary,
            capabilities: descriptor.capabilities,
            ops: descriptor.ops.into_iter().map(convert_op).collect(),
            schemas: descriptor
                .schemas
                .into_iter()
                .map(convert_schema_ref)
                .collect(),
            setup: descriptor.setup.map(convert_setup_contract),
        }
    }

    fn setup_apply_payload(
        mode: WizardMode,
        current_config: &[u8],
        answers: &[u8],
    ) -> Result<Vec<u8>> {
        use ciborium::value::Value as CValue;

        let current = if matches!(mode, WizardMode::Update | WizardMode::Remove) {
            CValue::Bytes(current_config.to_vec())
        } else {
            CValue::Null
        };
        let answers_value = if matches!(
            mode,
            WizardMode::Default | WizardMode::Setup | WizardMode::Update
        ) {
            CValue::Bytes(answers.to_vec())
        } else {
            CValue::Null
        };

        let value = CValue::Map(vec![
            (
                CValue::Text("mode".to_string()),
                CValue::Text(mode.as_str().to_string()),
            ),
            (CValue::Text("current_config_cbor".to_string()), current),
            (CValue::Text("answers_cbor".to_string()), answers_value),
            (CValue::Text("metadata_cbor".to_string()), CValue::Null),
        ]);

        let mut out = Vec::new();
        ciborium::ser::into_writer(&value, &mut out)
            .map_err(|err| anyhow!("encode setup.apply_answers payload: {err}"))?;
        Ok(out)
    }

    fn invoke_setup_apply(
        wasm_bytes: &[u8],
        mode: WizardMode,
        current_config: &[u8],
        answers: &[u8],
    ) -> Result<Vec<u8>> {
        let engine = build_engine()?;
        let component = Component::from_binary(&engine, wasm_bytes)
            .map_err(|err| anyhow!("load component: {err}"))?;
        let mut linker: Linker<()> = Linker::new(&engine);
        add_control_imports(&mut linker)?;
        let mut store = Store::new(&engine, ());
        let api = runtime::RuntimeComponent::instantiate(&mut store, &component, &linker)
            .map_err(|err| anyhow!("instantiate canonical component world: {err}"))?;
        let node = api.greentic_component_node();

        let payload_cbor = setup_apply_payload(mode, current_config, answers)?;
        let envelope = invoke_envelope(payload_cbor);
        let result = node
            .call_invoke(&mut store, "setup.apply_answers", &envelope)
            .map_err(|err| anyhow!("call invoke(setup.apply_answers): {err}"))?;

        let runtime::node::InvocationResult {
            ok,
            output_cbor,
            output_metadata_cbor: _,
        } = result.map_err(|err| anyhow!("invoke returned node error: {}", err.message))?;

        if !ok {
            return Err(anyhow!(
                "invoke(setup.apply_answers) returned ok=false with no node error"
            ));
        }

        Ok(output_cbor)
    }

    pub fn fetch_wizard_spec(wasm_bytes: &[u8], _mode: WizardMode) -> Result<WizardSpecOutput> {
        let engine = build_engine()?;
        let component = Component::from_binary(&engine, wasm_bytes)
            .map_err(|err| anyhow!("load component: {err}"))?;
        let mut linker: Linker<()> = Linker::new(&engine);
        add_control_imports(&mut linker)?;
        let mut store = Store::new(&engine, ());
        let api = runtime::RuntimeComponent::instantiate(&mut store, &component, &linker)
            .map_err(|err| anyhow!("instantiate canonical component world: {err}"))?;
        let node = api.greentic_component_node();

        let descriptor = node
            .call_describe(&mut store)
            .map(convert_descriptor)
            .map_err(|err| anyhow!("call describe: {err}"))?;
        let (qa_spec_cbor, answers_schema_cbor) = extract_setup_contract(&descriptor)?;
        ensure_setup_apply_answers_op(&descriptor)?;

        Ok(WizardSpecOutput {
            abi: WizardAbi::V6,
            describe_cbor: Vec::new(),
            descriptor: Some(descriptor),
            qa_spec_cbor,
            answers_schema_cbor,
        })
    }

    pub fn apply_wizard_answers(
        wasm_bytes: &[u8],
        _abi: WizardAbi,
        mode: WizardMode,
        current_config: &[u8],
        answers: &[u8],
    ) -> Result<Vec<u8>> {
        invoke_setup_apply(wasm_bytes, mode, current_config, answers)
    }

    pub fn run_wizard_ops(
        wasm_bytes: &[u8],
        mode: WizardMode,
        current_config: &[u8],
        answers: &[u8],
    ) -> Result<WizardOutput> {
        let spec = fetch_wizard_spec(wasm_bytes, mode)?;
        let config_cbor =
            apply_wizard_answers(wasm_bytes, spec.abi, mode, current_config, answers)?;
        Ok(WizardOutput {
            abi: spec.abi,
            describe_cbor: spec.describe_cbor,
            descriptor: spec.descriptor,
            qa_spec_cbor: spec.qa_spec_cbor,
            answers_cbor: answers.to_vec(),
            config_cbor,
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use host::{apply_wizard_answers, fetch_wizard_spec, run_wizard_ops};

#[cfg(target_arch = "wasm32")]
pub fn run_wizard_ops(
    _wasm_bytes: &[u8],
    _mode: WizardMode,
    _current_config: &[u8],
    _answers: &[u8],
) -> Result<WizardOutput> {
    Err(anyhow!("setup ops not supported on wasm targets"))
}

pub fn decode_component_qa_spec(qa_spec_cbor: &[u8], mode: WizardMode) -> Result<ComponentQaSpec> {
    let decoded: Result<ComponentQaSpec> =
        canonical::from_cbor(qa_spec_cbor).map_err(|err| anyhow!("decode qa-spec cbor: {err}"));
    if let Ok(spec) = decoded {
        return Ok(spec);
    }

    let legacy_json = std::str::from_utf8(qa_spec_cbor)
        .ok()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    if let Some(raw) = legacy_json {
        let adapted =
            greentic_types::adapters::component_v0_5_0_to_v0_6_0::adapt_component_qa_spec_json(
                mode.as_qa_mode(),
                raw,
            )
            .map_err(|err| anyhow!("adapt legacy qa-spec json: {err}"))?;
        let spec: ComponentQaSpec = canonical::from_cbor(adapted.as_slice())
            .map_err(|err| anyhow!("decode adapted qa-spec: {err}"))?;
        return Ok(spec);
    }

    Err(anyhow!("qa-spec payload is not valid CBOR or legacy JSON"))
}

pub fn answers_to_cbor(answers: &HashMap<String, JsonValue>) -> Result<Vec<u8>> {
    let mut map = serde_json::Map::new();
    for (k, v) in answers {
        map.insert(k.clone(), v.clone());
    }
    let json = JsonValue::Object(map);
    let bytes = canonical::to_canonical_cbor(&json)
        .map_err(|err| anyhow!("encode answers as canonical cbor: {err}"))?;
    Ok(bytes)
}

pub fn json_to_cbor(value: &JsonValue) -> Result<Vec<u8>> {
    let bytes = canonical::to_canonical_cbor(value)
        .map_err(|err| anyhow!("encode json as canonical cbor: {err}"))?;
    Ok(bytes)
}

pub fn cbor_to_json(bytes: &[u8]) -> Result<JsonValue> {
    let value: ciborium::value::Value =
        ciborium::de::from_reader(bytes).map_err(|err| anyhow!("decode cbor: {err}"))?;
    cbor_value_to_json(&value)
}

pub fn cbor_value_to_json(value: &ciborium::value::Value) -> Result<JsonValue> {
    use ciborium::value::Value as CValue;
    Ok(match value {
        CValue::Null => JsonValue::Null,
        CValue::Bool(b) => JsonValue::Bool(*b),
        CValue::Integer(i) => {
            if let Ok(v) = i64::try_from(*i) {
                JsonValue::Number(v.into())
            } else {
                let wide: i128 = (*i).into();
                JsonValue::String(wide.to_string())
            }
        }
        CValue::Float(f) => {
            let num = serde_json::Number::from_f64(*f)
                .ok_or_else(|| anyhow!("float out of range for json"))?;
            JsonValue::Number(num)
        }
        CValue::Text(s) => JsonValue::String(s.clone()),
        CValue::Bytes(b) => {
            JsonValue::Array(b.iter().map(|v| JsonValue::Number((*v).into())).collect())
        }
        CValue::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(cbor_value_to_json(item)?);
            }
            JsonValue::Array(out)
        }
        CValue::Map(entries) => {
            let mut map = serde_json::Map::new();
            for (k, v) in entries {
                let key = match k {
                    CValue::Text(s) => s.clone(),
                    other => return Err(anyhow!("non-string map key in cbor: {other:?}")),
                };
                map.insert(key, cbor_value_to_json(v)?);
            }
            JsonValue::Object(map)
        }
        CValue::Tag(_, inner) => cbor_value_to_json(inner)?,
        _ => return Err(anyhow!("unsupported cbor value")),
    })
}

pub fn qa_spec_to_questions(
    spec: &ComponentQaSpec,
    catalog: &I18nCatalog,
    locale: &str,
) -> Vec<crate::questions::Question> {
    let mut out = Vec::new();
    for question in &spec.questions {
        let prompt = resolve_text(&question.label, catalog, locale);
        let default = question
            .default
            .as_ref()
            .and_then(|value| cbor_value_to_json(value).ok());

        let (kind, choices) = match &question.kind {
            QuestionKind::Text => (crate::questions::QuestionKind::String, Vec::new()),
            QuestionKind::Number => (crate::questions::QuestionKind::Float, Vec::new()),
            QuestionKind::Bool => (crate::questions::QuestionKind::Bool, Vec::new()),
            QuestionKind::Choice { options } => {
                let mut values = Vec::new();
                for option in options {
                    values.push(JsonValue::String(option.value.clone()));
                }
                (crate::questions::QuestionKind::Choice, values)
            }
        };

        out.push(crate::questions::Question {
            id: question.id.clone(),
            prompt,
            kind,
            required: question.required,
            default,
            choices,
            show_if: None,
            writes_to: None,
        });
    }
    out
}

pub fn merge_default_answers(spec: &ComponentQaSpec, seed: &mut HashMap<String, JsonValue>) {
    for (key, value) in &spec.defaults {
        if seed.contains_key(key) {
            continue;
        }
        if let Ok(json_value) = cbor_value_to_json(value) {
            seed.insert(key.clone(), json_value);
        }
    }
}

pub fn ensure_answers_object(answers: &serde_json::Value) -> Result<()> {
    if matches!(answers, serde_json::Value::Object(_)) {
        return Ok(());
    }
    Err(anyhow!("answers must be a JSON object"))
}

pub fn empty_cbor_map() -> Vec<u8> {
    vec![0xa0]
}

pub fn describe_exports_for_meta(_abi: WizardAbi) -> Vec<String> {
    vec!["describe".to_string(), "invoke".to_string()]
}

pub fn abi_version_from_abi(_abi: WizardAbi) -> String {
    "0.6.0".to_string()
}

pub fn canonicalize_answers_map(answers: &serde_json::Map<String, JsonValue>) -> Result<Vec<u8>> {
    let mut map = BTreeMap::new();
    for (k, v) in answers {
        map.insert(k.clone(), v.clone());
    }
    let bytes =
        canonical::to_canonical_cbor(&map).map_err(|err| anyhow!("canonicalize answers: {err}"))?;
    Ok(bytes)
}
