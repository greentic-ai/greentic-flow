use crate::error::{FlowError, FlowErrorLocation, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct AnswersPaths {
    pub json: PathBuf,
    pub cbor: PathBuf,
}

pub fn answers_paths(base_dir: &Path, flow_id: &str, node_id: &str, mode: &str) -> AnswersPaths {
    let dir = base_dir.join(flow_id).join(node_id);
    let json = dir.join(format!("{mode}.answers.json"));
    let cbor = dir.join(format!("{mode}.answers.cbor"));
    AnswersPaths { json, cbor }
}

pub fn write_answers(
    base_dir: &Path,
    flow_id: &str,
    node_id: &str,
    mode: &str,
    answers: &BTreeMap<String, Value>,
    overwrite: bool,
) -> Result<AnswersPaths> {
    let paths = answers_paths(base_dir, flow_id, node_id, mode);
    if !overwrite && (paths.json.exists() || paths.cbor.exists()) {
        return Err(FlowError::Internal {
            message: format!(
                "answers already exist for {flow_id}/{node_id}/{mode}; use --overwrite-answers"
            ),
            location: FlowErrorLocation::new(None, None, None),
        });
    }
    if let Some(parent) = paths.json.parent() {
        fs::create_dir_all(parent).map_err(|err| FlowError::Internal {
            message: format!("create answers directory: {err}"),
            location: FlowErrorLocation::new(None, None, None),
        })?;
    }
    let json_text = serde_json::to_string_pretty(answers).map_err(|err| FlowError::Internal {
        message: format!("encode answers json: {err}"),
        location: FlowErrorLocation::new(None, None, None),
    })?;
    fs::write(&paths.json, json_text).map_err(|err| FlowError::Internal {
        message: format!("write answers json: {err}"),
        location: FlowErrorLocation::new(None, None, None),
    })?;

    let cbor_bytes =
        greentic_types::cbor::canonical::to_canonical_cbor(answers).map_err(|err| {
            FlowError::Internal {
                message: format!("encode answers cbor: {err}"),
                location: FlowErrorLocation::new(None, None, None),
            }
        })?;
    fs::write(&paths.cbor, cbor_bytes).map_err(|err| FlowError::Internal {
        message: format!("write answers cbor: {err}"),
        location: FlowErrorLocation::new(None, None, None),
    })?;

    Ok(paths)
}
