use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, anyhow};
use serde_json::Value as JsonValue;

use crate::i18n::{I18nCatalog, resolve_text};
use greentic_types::cbor::canonical;
use greentic_types::schemas::component::v0_6_0::{ComponentQaSpec, QaMode, QuestionKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardAbi {
    V6,
    Legacy,
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

    pub fn as_legacy_str(self) -> &'static str {
        match self {
            WizardMode::Update => "upgrade",
            _ => self.as_str(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WizardOutput {
    pub abi: WizardAbi,
    pub describe_cbor: Vec<u8>,
    pub qa_spec_cbor: Vec<u8>,
    pub answers_cbor: Vec<u8>,
    pub config_cbor: Vec<u8>,
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(unsafe_code)]
mod host {
    use super::*;
    use wasmtime::component::{Component, Linker};
    use wasmtime::{Config, Engine, Store};

    wasmtime::component::bindgen!({
        path: "wit/component-wizard-v0_6.wit",
        world: "component-wizard"
    });

    wasmtime::component::bindgen!({
        path: "wit/component-wizard-legacy.wit",
        world: "component-wizard-legacy"
    });

    pub struct WizardSpecOutput {
        pub abi: WizardAbi,
        pub describe_cbor: Vec<u8>,
        pub qa_spec_cbor: Vec<u8>,
    }

    pub fn fetch_wizard_spec(wasm_bytes: &[u8], mode: WizardMode) -> Result<WizardSpecOutput> {
        if let Ok(spec) = fetch_v6_spec(wasm_bytes, mode) {
            return Ok(spec);
        }
        fetch_legacy_spec(wasm_bytes, mode)
    }

    pub fn apply_wizard_answers(
        wasm_bytes: &[u8],
        abi: WizardAbi,
        mode: WizardMode,
        current_config: &[u8],
        answers: &[u8],
    ) -> Result<Vec<u8>> {
        match abi {
            WizardAbi::V6 => apply_v6(wasm_bytes, mode, current_config, answers),
            WizardAbi::Legacy => apply_legacy(wasm_bytes, mode, answers),
        }
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
            qa_spec_cbor: spec.qa_spec_cbor,
            answers_cbor: answers.to_vec(),
            config_cbor,
        })
    }

    fn build_engine() -> Result<Engine> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        Engine::new(&config).map_err(|err| anyhow!("init wasm engine: {err}"))
    }

    fn fetch_v6_spec(wasm_bytes: &[u8], mode: WizardMode) -> Result<WizardSpecOutput> {
        let engine = build_engine()?;
        let component = Component::from_binary(&engine, wasm_bytes)
            .map_err(|err| anyhow!("load component: {err}"))?;
        let linker: Linker<()> = Linker::new(&engine);
        let mut store = Store::new(&engine, ());
        let api = ComponentWizard::instantiate(&mut store, &component, &linker)
            .map_err(|err| anyhow!("instantiate component wizard (v0.6): {err}"))?;

        let describe_cbor = api
            .call_describe(&mut store)
            .map_err(|err| anyhow!("call describe: {err}"))?;
        let qa_spec_cbor = api
            .call_qa_spec(&mut store, mode_to_wit(mode))
            .map_err(|err| anyhow!("call qa-spec: {err}"))?;

        Ok(WizardSpecOutput {
            abi: WizardAbi::V6,
            describe_cbor,
            qa_spec_cbor,
        })
    }

    fn fetch_legacy_spec(wasm_bytes: &[u8], mode: WizardMode) -> Result<WizardSpecOutput> {
        let engine = build_engine()?;
        let component = Component::from_binary(&engine, wasm_bytes)
            .map_err(|err| anyhow!("load component: {err}"))?;
        let linker: Linker<()> = Linker::new(&engine);
        let mut store = Store::new(&engine, ());
        let api = ComponentWizardLegacy::instantiate(&mut store, &component, &linker)
            .map_err(|err| anyhow!("instantiate component wizard (legacy): {err}"))?;

        let describe_cbor = api
            .call_describe(&mut store)
            .map_err(|err| anyhow!("call describe: {err}"))?;
        let qa_spec_cbor = api
            .call_qa_spec(&mut store, mode.as_legacy_str())
            .map_err(|err| anyhow!("call qa-spec: {err}"))?;

        Ok(WizardSpecOutput {
            abi: WizardAbi::Legacy,
            describe_cbor,
            qa_spec_cbor,
        })
    }

    fn apply_v6(
        wasm_bytes: &[u8],
        mode: WizardMode,
        current_config: &[u8],
        answers: &[u8],
    ) -> Result<Vec<u8>> {
        let engine = build_engine()?;
        let component = Component::from_binary(&engine, wasm_bytes)
            .map_err(|err| anyhow!("load component: {err}"))?;
        let linker: Linker<()> = Linker::new(&engine);
        let mut store = Store::new(&engine, ());
        let api = ComponentWizard::instantiate(&mut store, &component, &linker)
            .map_err(|err| anyhow!("instantiate component wizard (v0.6): {err}"))?;

        api.call_apply_answers(&mut store, mode_to_wit(mode), current_config, answers)
            .map_err(|err| anyhow!("call apply-answers: {err}"))
    }

    fn apply_legacy(wasm_bytes: &[u8], mode: WizardMode, answers: &[u8]) -> Result<Vec<u8>> {
        let engine = build_engine()?;
        let component = Component::from_binary(&engine, wasm_bytes)
            .map_err(|err| anyhow!("load component: {err}"))?;
        let linker: Linker<()> = Linker::new(&engine);
        let mut store = Store::new(&engine, ());
        let api = ComponentWizardLegacy::instantiate(&mut store, &component, &linker)
            .map_err(|err| anyhow!("instantiate component wizard (legacy): {err}"))?;

        api.call_apply_answers(&mut store, mode.as_legacy_str(), answers)
            .map_err(|err| anyhow!("call apply-answers: {err}"))
    }

    fn mode_to_wit(mode: WizardMode) -> QaMode {
        match mode {
            WizardMode::Default => QaMode::Default,
            WizardMode::Setup => QaMode::Setup,
            WizardMode::Update => QaMode::Update,
            WizardMode::Remove => QaMode::Remove,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use host::{WizardSpecOutput, apply_wizard_answers, fetch_wizard_spec, run_wizard_ops};

#[cfg(target_arch = "wasm32")]
pub fn run_wizard_ops(
    _wasm_bytes: &[u8],
    _mode: WizardMode,
    _current_config: &[u8],
    _answers: &[u8],
) -> Result<WizardOutput> {
    Err(anyhow!("wizard ops not supported on wasm targets"))
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
                    let _label = resolve_text(&option.label, catalog, locale);
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
    // canonical CBOR map with 0 entries
    vec![0xa0]
}

pub fn describe_exports_for_meta(abi: WizardAbi) -> Vec<String> {
    match abi {
        WizardAbi::V6 => vec![
            "describe".to_string(),
            "qa-spec".to_string(),
            "apply-answers".to_string(),
        ],
        WizardAbi::Legacy => vec![
            "describe".to_string(),
            "qa-spec".to_string(),
            "apply-answers".to_string(),
            "legacy".to_string(),
        ],
    }
}

pub fn abi_version_from_abi(abi: WizardAbi) -> String {
    match abi {
        WizardAbi::V6 => "0.6.0".to_string(),
        WizardAbi::Legacy => "0.5.0".to_string(),
    }
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
