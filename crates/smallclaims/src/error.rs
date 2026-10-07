//! The error every graph operation returns: a stable code and a message.

use std::{any::Any, fmt};

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

    /// SQLite connection contention is not a malformed claim or projection failure.
    pub fn is_sqlite_contention(&self) -> bool {
        matches!(self.code, "database-busy" | "database-locked")
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

/// Keep SQLite BUSY/LOCKED typed; other unexpected failures remain `internal`.
/// Never infer contention from text: schema errors and user messages can say "locked" too.
pub fn internal(error: impl fmt::Display + 'static) -> Error {
    let source = &error as &dyn Any;
    let sqlite = source.downcast_ref::<rusqlite::Error>().or_else(|| {
        source
            .downcast_ref::<anyhow::Error>()?
            .downcast_ref::<rusqlite::Error>()
    });
    if let Some(rusqlite::Error::SqliteFailure(code, _)) = sqlite {
        let kind = match code.code {
            rusqlite::ErrorCode::DatabaseBusy => Some("database-busy"),
            rusqlite::ErrorCode::DatabaseLocked => Some("database-locked"),
            _ => None,
        };
        if let Some(kind) = kind {
            return Error::new(kind, error.to_string())
                .with_detail("sqlite_extended_code", code.extended_code);
        }
    }
    // An anyhow context must not erase a storage error already converted by a lower seam.
    let stored = source.downcast_ref::<Error>().or_else(|| {
        source
            .downcast_ref::<anyhow::Error>()?
            .downcast_ref::<Error>()
    });
    if let Some(stored) = stored.filter(|stored| stored.is_sqlite_contention()) {
        return Error {
            code: stored.code,
            message: error.to_string(),
            details: stored.details.clone(),
        };
    }
    Error::new("internal", error.to_string())
}

/// The store error inside `error` when there is one, such as a rule's `rule-denied`, else an
/// internal error with its message.
pub fn typed(error: anyhow::Error) -> Error {
    match error.downcast::<Error>() {
        Ok(error) => error,
        Err(error) => {
            let message = format!("{error:#}");
            let mut converted = internal(error);
            converted.message = message;
            converted
        }
    }
}

#[cfg(test)]
mod contention_tests {
    use super::*;
    #[test]
    fn sqlite_primary_extended_and_wrapped_codes_survive_without_matching_text() {
        for (code, expected) in [
            (rusqlite::ffi::SQLITE_BUSY, "database-busy"),
            (rusqlite::ffi::SQLITE_BUSY_SNAPSHOT, "database-busy"),
            (rusqlite::ffi::SQLITE_LOCKED, "database-locked"),
        ] {
            let sqlite = || {
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(code),
                    Some("not a lock phrase".into()),
                )
            };
            assert_eq!(internal(sqlite()).code, expected);
            assert_eq!(
                internal(anyhow::Error::new(sqlite()).context("projection")).code,
                expected
            );
            assert_eq!(
                typed(anyhow::Error::new(sqlite()).context("projection")).code,
                expected
            );
            assert_eq!(
                internal(anyhow::Error::new(internal(sqlite())).context("another seam")).code,
                expected
            );
        }
        assert_eq!(internal("database is locked").code, "internal");
        assert_eq!(
            internal(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                Some("database is locked".into())
            ))
            .code,
            "internal"
        );
        assert_eq!(
            typed(anyhow::Error::new(Error::new("rule-denied", "authority"))).code,
            "rule-denied"
        );
    }
}
