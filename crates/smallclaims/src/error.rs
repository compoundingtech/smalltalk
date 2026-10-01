//! The error every graph operation returns: a stable code and a message.

use std::fmt;

use serde_json::Value;

#[derive(Debug)]
pub struct Error {
    pub code: &'static str,
    pub message: String,
    pub details: serde_json::Map<String, Value>,
}

impl Error {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: serde_json::Map::new(),
        }
    }

    pub fn with_detail(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.details.insert(name.into(), value.into());
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

/// An unexpected failure, such as a storage error, as an `internal` error.
pub fn internal(error: impl fmt::Display) -> Error {
    Error::new("internal", error.to_string())
}
