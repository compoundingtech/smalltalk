//! Decoded envelope bytes in memory and SQLite; base64 only at the wire boundary.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rusqlite::types::{FromSql, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::{
    Deserializer, Serialize, Serializer,
    de::{self, Visitor},
};

/// Invalid base64 remains inspectable and repairable through normal admission. Old TEXT
/// rows and new BLOB rows can coexist while the background conversion advances.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvelopePayload(Repr);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Repr {
    Bytes(Vec<u8>),
    InvalidBase64(String),
}

impl EnvelopePayload {
    #[inline]
    pub fn bytes(&self) -> Result<&[u8], base64::DecodeError> {
        match &self.0 {
            Repr::Bytes(bytes) => Ok(bytes),
            Repr::InvalidBase64(text) => Err(invalid_base64_error(text)),
        }
    }

    pub fn base64(&self) -> String {
        match &self.0 {
            Repr::Bytes(bytes) => STANDARD.encode(bytes),
            Repr::InvalidBase64(text) => text.clone(),
        }
    }
}

impl From<Vec<u8>> for EnvelopePayload {
    #[inline]
    fn from(bytes: Vec<u8>) -> Self {
        Self(Repr::Bytes(bytes))
    }
}

impl From<String> for EnvelopePayload {
    fn from(text: String) -> Self {
        match STANDARD.decode(&text) {
            Ok(bytes) => Self(Repr::Bytes(bytes)),
            Err(_) => Self(Repr::InvalidBase64(text)),
        }
    }
}

impl From<&str> for EnvelopePayload {
    fn from(text: &str) -> Self {
        match STANDARD.decode(text) {
            Ok(bytes) => Self(Repr::Bytes(bytes)),
            Err(_) => Self(Repr::InvalidBase64(text.to_owned())),
        }
    }
}

impl Serialize for EnvelopePayload {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.0 {
            Repr::Bytes(bytes) => {
                serializer.collect_str(&base64::display::Base64Display::new(bytes, &STANDARD))
            }
            Repr::InvalidBase64(text) => serializer.serialize_str(text),
        }
    }
}

impl<'de> serde::Deserialize<'de> for EnvelopePayload {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PayloadVisitor;
        impl Visitor<'_> for PayloadVisitor {
            type Value = EnvelopePayload;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a base64 envelope payload")
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<Self::Value, E> {
                Ok(text.into())
            }
            fn visit_string<E: de::Error>(self, text: String) -> Result<Self::Value, E> {
                Ok(text.into())
            }
        }
        deserializer.deserialize_str(PayloadVisitor)
    }
}

impl ToSql for EnvelopePayload {
    #[inline]
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(match &self.0 {
            Repr::Bytes(bytes) => ValueRef::Blob(bytes),
            Repr::InvalidBase64(text) => ValueRef::Text(text.as_bytes()),
        }))
    }
}

impl FromSql for EnvelopePayload {
    #[inline]
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value {
            ValueRef::Blob(bytes) => Ok(bytes.to_vec().into()),
            ValueRef::Text(_) => Ok(value.as_str()?.into()),
            _ => Err(rusqlite::types::FromSqlError::InvalidType),
        }
    }
}

#[cold]
fn invalid_base64_error(text: &str) -> base64::DecodeError {
    STANDARD.decode(text).unwrap_err()
}
