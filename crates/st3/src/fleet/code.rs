//! Join codes: `stj1-` and lowercase base32 of a compact binary record plus a 4-byte checksum.
//!
//! The only secret in a code is the invite token. It is not the fleet secret and cannot derive
//! it: the join handshake seals the secret with a key that also needs the joiner's ephemeral
//! private key.

use anyhow::Result;
use base64::Engine as _;
use data_encoding::BASE32_NOPAD;
use sha2::{Digest as _, Sha256};

pub const PREFIX: &str = "stj1-";
const VERSION: u8 = 1;
const FLAG_MIGRATE: u8 = 1;

/// Where the sponsor listens.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CodeEndpoint {
    Tailscale(String),
    Fabric { node: String, protocol: String },
    Loopback(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JoinCode {
    pub fleet_id: uuid::Uuid,
    pub invite: [u8; 16],
    pub token: [u8; 16],
    /// The first 16 bytes of the SHA-256 of the sponsor's member public key.
    pub fingerprint: [u8; 16],
    /// Unix seconds; for messages only. The sponsor's clock decides expiry.
    pub expires_at: u64,
    pub name: Option<String>,
    /// A migration code admits an existing config-peer node and carries no secret.
    pub migrate: bool,
    pub endpoints: Vec<CodeEndpoint>,
}

/// The fingerprint of a base64url member public key.
pub fn fingerprint(member_key: &str) -> [u8; 16] {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(member_key)
        .unwrap_or_else(|_| member_key.as_bytes().to_vec());
    let digest = Sha256::digest(bytes);
    digest[..16]
        .try_into()
        .expect("a SHA-256 digest has 32 bytes")
}

fn put_text(record: &mut Vec<u8>, text: &str) -> Result<()> {
    let length =
        u16::try_from(text.len()).map_err(|_| anyhow::anyhow!("a join code field is too long"))?;
    record.extend(length.to_be_bytes());
    record.extend(text.as_bytes());
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        anyhow::ensure!(self.bytes.len() >= length, "this join code is damaged");
        let (taken, rest) = self.bytes.split_at(length);
        self.bytes = rest;
        Ok(taken)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("take returns N bytes"))
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn text(&mut self) -> Result<String> {
        let length = u16::from_be_bytes(self.array()?) as usize;
        String::from_utf8(self.take(length)?.to_vec())
            .map_err(|_| anyhow::anyhow!("this join code is damaged"))
    }
}

impl JoinCode {
    pub fn encode(&self) -> Result<String> {
        let mut record = vec![VERSION, if self.migrate { FLAG_MIGRATE } else { 0 }];
        record.extend(self.fleet_id.as_bytes());
        record.extend(self.invite);
        record.extend(self.token);
        record.extend(self.fingerprint);
        record.extend(self.expires_at.to_be_bytes());
        match &self.name {
            Some(name) => {
                let length = u8::try_from(name.len())
                    .map_err(|_| anyhow::anyhow!("the pinned name is too long"))?;
                record.push(length);
                record.extend(name.as_bytes());
            }
            None => record.push(0),
        }
        let count = u8::try_from(self.endpoints.len())
            .map_err(|_| anyhow::anyhow!("too many endpoints for a join code"))?;
        record.push(count);
        for endpoint in &self.endpoints {
            match endpoint {
                CodeEndpoint::Tailscale(address) => {
                    record.push(1);
                    put_text(&mut record, address)?;
                }
                CodeEndpoint::Fabric { node, protocol } => {
                    record.push(2);
                    put_text(&mut record, node)?;
                    put_text(&mut record, protocol)?;
                }
                CodeEndpoint::Loopback(address) => {
                    record.push(3);
                    put_text(&mut record, address)?;
                }
            }
        }
        let checksum = Sha256::digest(&record);
        record.extend(&checksum[..4]);
        Ok(format!(
            "{PREFIX}{}",
            BASE32_NOPAD.encode(&record).to_ascii_lowercase()
        ))
    }

    pub fn decode(text: &str) -> Result<Self> {
        let compact = text
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        let body = compact
            .get(..PREFIX.len())
            .filter(|prefix| prefix.eq_ignore_ascii_case(PREFIX))
            .map(|_| &compact[PREFIX.len()..])
            .ok_or_else(|| anyhow::anyhow!("this is not an st fleet join code"))?;
        let bytes = BASE32_NOPAD
            .decode(body.to_ascii_uppercase().as_bytes())
            .map_err(|_| anyhow::anyhow!("this join code is damaged"))?;
        anyhow::ensure!(bytes.len() > 4, "this join code is damaged");
        let (record, checksum) = bytes.split_at(bytes.len() - 4);
        anyhow::ensure!(
            Sha256::digest(record)[..4] == *checksum,
            "this join code is damaged: check that it was copied whole"
        );
        let mut reader = Reader { bytes: record };
        let version = reader.byte()?;
        anyhow::ensure!(
            version == VERSION,
            "this join code needs a newer st (code version {version})"
        );
        let flags = reader.byte()?;
        let fleet_id = uuid::Uuid::from_bytes(reader.array()?);
        let invite = reader.array()?;
        let token = reader.array()?;
        let fingerprint = reader.array()?;
        let expires_at = u64::from_be_bytes(reader.array()?);
        let name_length = reader.byte()? as usize;
        let name = match name_length {
            0 => None,
            length => Some(
                String::from_utf8(reader.take(length)?.to_vec())
                    .map_err(|_| anyhow::anyhow!("this join code is damaged"))?,
            ),
        };
        let mut endpoints = Vec::new();
        for _ in 0..reader.byte()? {
            endpoints.push(match reader.byte()? {
                1 => CodeEndpoint::Tailscale(reader.text()?),
                2 => CodeEndpoint::Fabric {
                    node: reader.text()?,
                    protocol: reader.text()?,
                },
                3 => CodeEndpoint::Loopback(reader.text()?),
                other => anyhow::bail!("this join code has an unknown endpoint kind {other}"),
            });
        }
        anyhow::ensure!(reader.bytes.is_empty(), "this join code is damaged");
        Ok(Self {
            fleet_id,
            invite,
            token,
            fingerprint,
            expires_at,
            name,
            migrate: flags & FLAG_MIGRATE != 0,
            endpoints,
        })
    }

    pub fn invite_id(&self) -> String {
        hex::encode(self.invite)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> JoinCode {
        JoinCode {
            fleet_id: uuid::Uuid::parse_str("5b0c1d8e-6a44-4f0e-9d51-2f7f3c9a0b12").unwrap(),
            invite: [1; 16],
            token: [2; 16],
            fingerprint: fingerprint("AAAA"),
            expires_at: 1_790_000_000,
            name: Some("laptop".into()),
            migrate: false,
            endpoints: vec![
                CodeEndpoint::Tailscale("100.101.102.103:31313".into()),
                CodeEndpoint::Fabric {
                    node: "a".repeat(64),
                    protocol: "st3/fleet/5b0c1d8e-6a44-4f0e-9d51-2f7f3c9a0b12".into(),
                },
                CodeEndpoint::Loopback("127.0.0.1:31313".into()),
            ],
        }
    }

    #[test]
    fn a_join_code_round_trips() {
        let code = sample();
        let text = code.encode().unwrap();
        assert!(text.starts_with(PREFIX));
        assert_eq!(text, text.to_ascii_lowercase());
        assert_eq!(JoinCode::decode(&text).unwrap(), code);
        // Pasting tolerates whitespace and case.
        let pasted = format!(
            "  {}\n",
            text.to_ascii_uppercase().replacen("STJ1-", "stj1-", 1)
        );
        assert_eq!(JoinCode::decode(&pasted).unwrap(), code);
        let migrate = JoinCode {
            migrate: true,
            name: None,
            endpoints: Vec::new(),
            ..code
        };
        assert_eq!(
            JoinCode::decode(&migrate.encode().unwrap()).unwrap(),
            migrate
        );
    }

    #[test]
    fn a_damaged_code_fails_its_checksum() {
        let text = sample().encode().unwrap();
        let mut damaged = text.clone().into_bytes();
        let position = PREFIX.len() + 10;
        damaged[position] = if damaged[position] == b'a' {
            b'b'
        } else {
            b'a'
        };
        let error = JoinCode::decode(&String::from_utf8(damaged).unwrap()).unwrap_err();
        assert!(error.to_string().contains("damaged"), "{error}");
        let truncated = &text[..text.len() - 6];
        assert!(JoinCode::decode(truncated).is_err());
        assert!(JoinCode::decode("hello").is_err());
    }

    #[test]
    fn an_unknown_code_version_is_refused() {
        let text = sample().encode().unwrap();
        let mut bytes = BASE32_NOPAD
            .decode(text[PREFIX.len()..].to_ascii_uppercase().as_bytes())
            .unwrap();
        bytes.truncate(bytes.len() - 4);
        bytes[0] = 9;
        let checksum = Sha256::digest(&bytes);
        bytes.extend(&checksum[..4]);
        let forged = format!(
            "{PREFIX}{}",
            BASE32_NOPAD.encode(&bytes).to_ascii_lowercase()
        );
        assert!(
            JoinCode::decode(&forged)
                .unwrap_err()
                .to_string()
                .contains("newer st")
        );
    }

    #[test]
    fn a_join_code_never_contains_the_fleet_secret() {
        // A code has no field for the secret. Check that no encoding of a random secret shows
        // up in codes built beside it, including one whose token is the secret's first half.
        for _ in 0..32 {
            let mut secret = [0_u8; 32];
            getrandom::fill(&mut secret).unwrap();
            let code = JoinCode {
                token: secret[..16].try_into().unwrap(),
                invite: secret[16..].try_into().unwrap(),
                ..sample()
            };
            let text = code.encode().unwrap();
            let raw = BASE32_NOPAD
                .decode(text[PREFIX.len()..].to_ascii_uppercase().as_bytes())
                .unwrap();
            for form in [
                hex::encode(secret),
                BASE32_NOPAD.encode(&secret).to_ascii_lowercase(),
                base64::engine::general_purpose::STANDARD.encode(secret),
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret),
            ] {
                assert!(!text.contains(&form));
            }
            // Even with both halves present, they are separated, never the secret's 32 bytes.
            assert!(!raw.windows(32).any(|window| window == secret));
        }
    }
}
