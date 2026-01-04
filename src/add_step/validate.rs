use serde_yaml_bw::to_string;

use crate::{
    component_catalog::ComponentCatalog,
    error::{FlowError, FlowErrorLocation, Result},
    flow_ir::FlowIr,
    loader::load_ygtc_from_str,
};

use super::validate_flow;

pub fn validate_schema_and_flow(flow: &FlowIr, catalog: &dyn ComponentCatalog) -> Result<()> {
    let doc = flow.to_doc()?;
    let yaml = to_string(&doc).map_err(|e| FlowError::Internal {
        message: format!("serialize flow for validation: {e}"),
        location: FlowErrorLocation::at_path("add_step.validate".to_string()),
    })?;
    let _ = load_ygtc_from_str(&yaml)?;
    let diags = validate_flow(flow, catalog);
    if diags.is_empty() {
        Ok(())
    } else {
        super::diagnostics_to_error(diags)
    }
}
