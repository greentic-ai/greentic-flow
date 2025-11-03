use thiserror::Error;

#[derive(Debug, Error)]
pub enum FlowError {
    #[error("YAML parse error at {0}: {1}")]
    Yaml(String, String),
    #[error("Schema validation failed:\n{0}")]
    Schema(String),
    #[error(
        "Node '{0}' must contain exactly one component key like 'qa.process' plus optional 'routing'"
    )]
    NodeComponentShape(String),
    #[error("Invalid component key '{0}' in node '{1}' (must match ^[A-Za-z][\\w.-]*\\.[\\w.-]+$)")]
    BadComponentKey(String, String),
    #[error("Missing node '{0}' referenced in routing from '{1}'")]
    MissingNode(String, String),
    #[error("Internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, FlowError>;
