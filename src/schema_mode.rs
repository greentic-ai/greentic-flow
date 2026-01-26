use anyhow::{Result, anyhow};
use std::env;

/// CLI-wide mode governing how input schemas are enforced.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SchemaMode {
    Strict,
    Permissive,
}

impl SchemaMode {
    /// Resolve the schema mode from the CLI flag and `GREENTIC_FLOW_STRICT`.
    ///
    /// - `--permissive` trumps the environment variable.
    /// - `GREENTIC_FLOW_STRICT=0` → permissive, `1` → strict.
    /// - Missing settings default to strict.
    pub fn resolve(cli_permissive: bool) -> Result<Self> {
        if cli_permissive {
            return Ok(SchemaMode::Permissive);
        }
        match env::var("GREENTIC_FLOW_STRICT") {
            Ok(val) => match val.as_str() {
                "0" => Ok(SchemaMode::Permissive),
                "1" => Ok(SchemaMode::Strict),
                other => Err(anyhow!(
                    "GREENTIC_FLOW_STRICT must be '0' or '1', got '{other}'"
                )),
            },
            Err(env::VarError::NotPresent) => Ok(SchemaMode::Strict),
            Err(err) => Err(anyhow!("failed to read GREENTIC_FLOW_STRICT: {err}")),
        }
    }

    pub fn is_permissive(&self) -> bool {
        matches!(self, SchemaMode::Permissive)
    }
}
