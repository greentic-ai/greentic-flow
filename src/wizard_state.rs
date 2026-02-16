use crate::error::{FlowError, FlowErrorLocation, Result};
use greentic_types::cbor::canonical;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WizardState {
    pub flow_id: String,
    pub locale: String,
    pub steps: Vec<WizardStepState>,
    pub last_updated: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WizardStepState {
    pub node_id: String,
    pub mode: String,
    pub updated_at: u64,
}

pub fn wizard_state_path(flow_path: &Path, flow_id: &str) -> PathBuf {
    let base = flow_path.parent().unwrap_or_else(|| Path::new("."));
    base.join(".greentic/cache/flow_wizard")
        .join(format!("{flow_id}.cbor"))
}

pub fn load_wizard_state(flow_path: &Path, flow_id: &str) -> Result<Option<WizardState>> {
    let path = wizard_state_path(flow_path, flow_id);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).map_err(|err| FlowError::Internal {
        message: format!("read wizard state: {err}"),
        location: FlowErrorLocation::new(None, None, None),
    })?;
    let mut state: WizardState =
        canonical::from_cbor(&bytes).map_err(|err| FlowError::Internal {
            message: format!("decode wizard state: {err}"),
            location: FlowErrorLocation::new(None, None, None),
        })?;
    // Compatibility: keep legacy persisted "upgrade" mode readable in 0.6.x.
    for step in &mut state.steps {
        if step.mode == "upgrade" {
            step.mode = "update".to_string();
        }
    }
    Ok(Some(state))
}

pub fn update_wizard_state(
    flow_path: &Path,
    flow_id: &str,
    node_id: &str,
    mode: &str,
    locale: &str,
) -> Result<()> {
    let mode = if mode == "upgrade" { "update" } else { mode };
    let mut state = load_wizard_state(flow_path, flow_id)?.unwrap_or_else(|| WizardState {
        flow_id: flow_id.to_string(),
        locale: locale.to_string(),
        steps: Vec::new(),
        last_updated: 0,
    });
    let now = now_epoch_secs();
    state.locale = locale.to_string();
    state.last_updated = now;
    if let Some(step) = state.steps.iter_mut().find(|s| s.node_id == node_id) {
        step.mode = mode.to_string();
        step.updated_at = now;
    } else {
        state.steps.push(WizardStepState {
            node_id: node_id.to_string(),
            mode: mode.to_string(),
            updated_at: now,
        });
    }
    write_wizard_state(flow_path, &state)
}

pub fn remove_wizard_step(flow_path: &Path, flow_id: &str, node_id: &str) -> Result<()> {
    let Some(mut state) = load_wizard_state(flow_path, flow_id)? else {
        return Ok(());
    };
    let now = now_epoch_secs();
    state.last_updated = now;
    state.steps.retain(|step| step.node_id != node_id);
    write_wizard_state(flow_path, &state)
}

fn write_wizard_state(flow_path: &Path, state: &WizardState) -> Result<()> {
    let path = wizard_state_path(flow_path, &state.flow_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| FlowError::Internal {
            message: format!("create wizard state directory: {err}"),
            location: FlowErrorLocation::new(None, None, None),
        })?;
    }
    let bytes = canonical::to_canonical_cbor(state).map_err(|err| FlowError::Internal {
        message: format!("encode wizard state: {err}"),
        location: FlowErrorLocation::new(None, None, None),
    })?;
    fs::write(&path, bytes).map_err(|err| FlowError::Internal {
        message: format!("write wizard state: {err}"),
        location: FlowErrorLocation::new(None, None, None),
    })?;
    Ok(())
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
