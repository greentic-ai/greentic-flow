mod adapter_resolvable;

pub use adapter_resolvable::AdapterResolvableRule;

use crate::{ir::FlowIR, registry::AdapterCatalog};

/// Run the built-in lint rules that do not require external data.
pub fn lint_builtin_rules(_flow: &FlowIR) -> Vec<String> {
    // Placeholder for future intrinsic flow checks.
    Vec::new()
}

/// Run all lint rules including adapter resolution backed by a catalog.
pub fn lint_with_registry(flow: &FlowIR, catalog: &AdapterCatalog) -> Vec<String> {
    let mut errors = lint_builtin_rules(flow);
    errors.extend(AdapterResolvableRule::check(flow, catalog));
    errors
}
