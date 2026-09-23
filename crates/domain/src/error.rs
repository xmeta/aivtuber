use std::{error::Error, fmt};

/// Validation failure for a provider-neutral domain value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainValidationError {
    field: &'static str,
    message: String,
}

impl DomainValidationError {
    pub(crate) fn new(field: &'static str, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }

    /// Domain field that failed validation.
    pub fn field(&self) -> &'static str {
        self.field
    }

    /// Human-readable validation message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for DomainValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

impl Error for DomainValidationError {}
