//! Canonical identity of the private notes subject for one host-qualified seat.
//! Filesystem realization and ordinary subject admission belong to the owning node.
use super::{ValidationError, error};
use std::fmt;

pub const SCHEME: &str = "dev.schickling.agent-private-notes";
const PREFIX: &str = "dev.schickling.agent-private-notes://";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrivateNotesUri {
    raw: String,
    host: String,
    identity: String,
}

impl PrivateNotesUri {
    pub fn for_subject(host: &str, identity: &str) -> Result<Self, ValidationError> {
        validate_component(host)?;
        validate_component(identity)?;
        let mut raw = String::from(PREFIX);
        encode_into(host, &mut raw);
        raw.push('/');
        encode_into(identity, &mut raw);
        Ok(Self { raw, host: host.into(), identity: identity.into() })
    }

    pub fn parse(value: &str) -> Result<Self, ValidationError> {
        let rest = value.strip_prefix(PREFIX).ok_or_else(shape)?;
        let (host, identity) = rest.split_once('/').ok_or_else(shape)?;
        let host = decode_component(host)?;
        let identity = decode_component(identity)?;
        let parsed = Self::for_subject(&host, &identity)?;
        if parsed.raw != value {
            return Err(error("private-notes-uri-component", "notes URI components must use canonical percent encoding"));
        }
        Ok(parsed)
    }

    pub fn for_expected_subject(value: &str, host: &str, identity: &str) -> Result<Self, ValidationError> {
        let parsed = Self::parse(value)?;
        if parsed.host != host || parsed.identity != identity {
            return Err(error("private-notes-subject-mismatch", "notes URI does not identify the exact subject"));
        }
        Ok(parsed)
    }

    pub fn as_str(&self) -> &str { &self.raw }
    pub fn host(&self) -> &str { &self.host }
    pub fn identity(&self) -> &str { &self.identity }
}

impl fmt::Display for PrivateNotesUri {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.raw)
    }
}

pub fn claims(value: &str) -> bool {
    value.split_once(':').is_some_and(|(scheme, _)| scheme == SCHEME)
}

fn shape() -> ValidationError {
    error("private-notes-uri-shape", "notes URI must identify one host and identity")
}

fn validate_component(value: &str) -> Result<(), ValidationError> {
    if value.is_empty() || value.contains(['/', '\0']) || matches!(value, "." | "..") {
        return Err(shape());
    }
    Ok(())
}

fn encode_into(value: &str, encoded: &mut String) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 15)]));
        }
    }
}

fn decode_component(value: &str) -> Result<String, ValidationError> {
    let invalid = || error("private-notes-uri-component", "notes URI has an invalid percent-encoded component");
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            decoded.push(byte);
            index += 1;
        } else if byte == b'%' && index + 2 < bytes.len() {
            let high = hex(bytes[index + 1]).ok_or_else(invalid)?;
            let low = hex(bytes[index + 2]).ok_or_else(invalid)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            return Err(invalid());
        }
    }
    String::from_utf8(decoded).map_err(|_| invalid())
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_identity_is_exact_and_does_not_allow_path_selection() {
        let uri = PrivateNotesUri::for_subject("host a", "org.worker a").unwrap();
        assert_eq!(uri.as_str(), "dev.schickling.agent-private-notes://host%20a/org.worker%20a");
        assert_eq!(PrivateNotesUri::parse(uri.as_str()).unwrap(), uri);
        assert!(PrivateNotesUri::for_expected_subject(uri.as_str(), "host a", "org.worker b").is_err());
        for suffix in ["host/", "host/a/b", "host/%2F", "host/%00", "host/%61", "host/%FF", "host/..", "host/a?query", "host/a#fragment"] {
            assert!(PrivateNotesUri::parse(&format!("{PREFIX}{suffix}")).is_err(), "{suffix}");
        }
    }
}
