//! Ed25519 signatures as st's node keys make them: keys as PKCS#8 documents, public keys and
//! signatures as base64url without padding. The gateway only verifies; a caller that signs
//! (st's daemon, or an st command) holds its own node key.

use base64::Engine as _;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair as _, UnparsedPublicKey};

fn encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn decode(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text.as_bytes())
        .ok()
}

/// Whether `signature` is `public`'s over `message`.
pub fn verify(public: &str, message: &[u8], signature: &str) -> bool {
    let (Some(public), Some(signature)) = (decode(public), decode(signature)) else {
        return false;
    };
    public.len() == 32
        && UnparsedPublicKey::new(&ED25519, public)
            .verify(message, &signature)
            .is_ok()
}

/// A key that signs.
pub struct Signer {
    pair: Ed25519KeyPair,
    public: String,
}

impl Signer {
    pub fn from_pkcs8(document: &[u8]) -> anyhow::Result<Self> {
        let pair = Ed25519KeyPair::from_pkcs8(document)
            .map_err(|_| anyhow::anyhow!("the key is not an Ed25519 PKCS#8 document"))?;
        let public = encode(pair.public_key().as_ref());
        Ok(Self { pair, public })
    }

    /// A new key and its PKCS#8 document.
    pub fn generate() -> anyhow::Result<(Self, Vec<u8>)> {
        let document = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
            .map_err(|_| anyhow::anyhow!("generate a key"))?;
        let bytes = document.as_ref().to_vec();
        Ok((Self::from_pkcs8(&bytes)?, bytes))
    }

    pub fn public(&self) -> &str {
        &self.public
    }

    pub fn sign(&self, message: &[u8]) -> String {
        encode(self.pair.sign(message).as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signature_verifies_only_with_its_key_and_message() {
        let (key, _) = Signer::generate().unwrap();
        let (other, _) = Signer::generate().unwrap();
        let signature = key.sign(b"statement");
        assert!(verify(key.public(), b"statement", &signature));
        assert!(!verify(other.public(), b"statement", &signature));
        assert!(!verify(key.public(), b"another", &signature));
        assert!(!verify("not base64!", b"statement", &signature));
    }
}
