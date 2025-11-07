use std::fmt;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowErrorLocation {
    pub path: Option<String>,
    pub line: Option<usize>,
    pub column: Option<usize>,
}

impl FlowErrorLocation {
    pub fn new<P: Into<Option<String>>>(
        path: P,
        line: Option<usize>,
        column: Option<usize>,
    ) -> Self {
        FlowErrorLocation {
            path: path.into(),
            line,
            column,
        }
    }

    pub fn at_path(path: impl Into<String>) -> Self {
        FlowErrorLocation::new(Some(path.into()), None, None)
    }

    pub fn at_path_with_position(
        path: impl Into<String>,
        line: Option<usize>,
        column: Option<usize>,
    ) -> Self {
        FlowErrorLocation::new(Some(path.into()), line, column)
    }

    pub fn describe(&self) -> Option<String> {
        if self.path.is_none() && self.line.is_none() && self.column.is_none() {
            return None;
        }
        let mut parts = String::new();
        if let Some(path) = &self.path {
            parts.push_str(path);
        }
        match (self.line, self.column) {
            (Some(line), Some(column)) => {
                if !parts.is_empty() {
                    parts.push(':');
                }
                parts.push_str(&format!("{line}:{column}"));
            }
            (Some(line), None) => {
                if !parts.is_empty() {
                    parts.push(':');
                }
                parts.push_str(&line.to_string());
            }
            (None, Some(column)) => {
                if !parts.is_empty() {
                    parts.push(':');
                }
                parts.push_str(&column.to_string());
            }
            _ => {}
        }
        Some(parts)
    }
}

impl fmt::Display for FlowErrorLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_none() && self.line.is_none() && self.column.is_none() {
            return Ok(());
        }
        write!(f, " at ")?;
        if let Some(path) = &self.path {
            write!(f, "{path}")?;
            if self.line.is_some() || self.column.is_some() {
                write!(f, ":")?;
            }
        }
        match (self.line, self.column) {
            (Some(line), Some(column)) => write!(f, "{line}:{column}")?,
            (Some(line), None) => write!(f, "{line}")?,
            (None, Some(column)) => write!(f, "{column}")?,
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaErrorDetail {
    pub message: String,
    pub location: FlowErrorLocation,
}

#[derive(Debug, Error)]
pub enum FlowError {
    #[error("YAML parse error{location}: {message}")]
    Yaml {
        message: String,
        location: FlowErrorLocation,
    },
    #[error("Schema validation failed{location}:\n{message}")]
    Schema {
        message: String,
        details: Vec<SchemaErrorDetail>,
        location: FlowErrorLocation,
    },
    #[error(
        "Node '{node_id}' must contain exactly one component key like 'qa.process' plus optional 'routing'{location}"
    )]
    NodeComponentShape {
        node_id: String,
        location: FlowErrorLocation,
    },
    #[error(
        "Invalid component key '{component}' in node '{node_id}' (must match ^[A-Za-z][\\w.-]*\\.[\\w.-]+$){location}"
    )]
    BadComponentKey {
        component: String,
        node_id: String,
        location: FlowErrorLocation,
    },
    #[error("Missing node '{target}' referenced in routing from '{node_id}'{location}")]
    MissingNode {
        target: String,
        node_id: String,
        location: FlowErrorLocation,
    },
    #[error("Internal error{location}: {message}")]
    Internal {
        message: String,
        location: FlowErrorLocation,
    },
}

pub type Result<T> = std::result::Result<T, FlowError>;
