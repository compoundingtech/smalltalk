//! Member keys. Each fleet member generates one Ed25519 key pair. The private half stays in
//! `STATE/fleet/node.key`; the public half, base64url without padding, appears in membership
//! claims and signs the member's connections and envelopes.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;

use anyhow::{Context as _, Result};
use base64::Engine as _;
use ring::rand::SystemRandom;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair as _, UnparsedPublicKey};

const ENVELOPE_SIGNATURE_DOMAIN: &str = "st3-envelope-v1";

fn encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn decode(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text.as_bytes())
        .ok()
}

/// One member's Ed25519 key pair.
pub struct MemberKey {
    pair: Ed25519KeyPair,
    public: String,
}

impl std::fmt::Debug for MemberKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MemberKey")
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

impl MemberKey {
    /// Generate a new key pair and return it with its PKCS#8 document.
    pub fn generate() -> Result<(Self, Vec<u8>)> {
        let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .map_err(|_| anyhow::anyhow!("generate a member key"))?;
        let bytes = document.as_ref().to_vec();
        Ok((Self::from_pkcs8(&bytes)?, bytes))
    }

    pub fn from_pkcs8(bytes: &[u8]) -> Result<Self> {
        let pair = Ed25519KeyPair::from_pkcs8(bytes)
            .map_err(|_| anyhow::anyhow!("the member key is not an Ed25519 PKCS#8 document"))?;
        let public = encode(pair.public_key().as_ref());
        Ok(Self { pair, public })
    }

    /// Read a key file. The file must deny group and other access.
    pub fn load(path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path)
            .with_context(|| format!("inspect the member key {}", path.display()))?;
        anyhow::ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "the member key must not grant group or other permissions"
        );
        let bytes =
            fs::read(path).with_context(|| format!("read the member key {}", path.display()))?;
        Self::from_pkcs8(&bytes)
    }

    /// Read the key at `path`, or create it there. A new key is written to a temporary `0600`
    /// file, synced, and renamed into place, so a crash never leaves a partial key.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            return Self::load(path);
        }
        let parent = path
            .parent()
            .context("the member key path has no parent directory")?;
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        let (key, document) = Self::generate()?;
        let temporary = parent.join(format!(
            ".{}.{}.tmp",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("node.key"),
            std::process::id()
        ));
        {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .with_context(|| format!("create {}", temporary.display()))?;
            file.write_all(&document)?;
            file.sync_all()?;
        }
        fs::rename(&temporary, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(key)
    }

    /// The public key, base64url without padding.
    pub fn public(&self) -> &str {
        &self.public
    }

    /// Sign `message`; the signature is base64url without padding.
    pub fn sign(&self, message: &[u8]) -> String {
        encode(self.pair.sign(message).as_ref())
    }
}

/// Check an Ed25519 signature made by `public` over `message`.
pub fn verify_signature(public: &str, message: &[u8], signature: &str) -> bool {
    let (Some(public), Some(signature)) = (decode(public), decode(signature)) else {
        return false;
    };
    public.len() == 32
        && UnparsedPublicKey::new(&ED25519, public)
            .verify(message, &signature)
            .is_ok()
}

/// The bytes a writer signs for one envelope. The envelope hash already covers the writer,
/// sequence, previous hash, accept time, and payload; the fleet ID keeps a signature from
/// counting in another fleet.
pub fn envelope_signature_message(
    fleet_id: &str,
    writer: &str,
    sequence: u64,
    envelope_hash: &str,
) -> Vec<u8> {
    format!("{ENVELOPE_SIGNATURE_DOMAIN}\n{fleet_id}\n{writer}\n{sequence}\n{envelope_hash}")
        .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_member_key_signs_and_only_its_own_signatures_verify() {
        let (key, _) = MemberKey::generate().unwrap();
        let (other, _) = MemberKey::generate().unwrap();
        let message = envelope_signature_message("fleet", "node-a", 7, "hash");
        let signature = key.sign(&message);
        assert!(verify_signature(key.public(), &message, &signature));
        assert!(!verify_signature(other.public(), &message, &signature));
        let changed = envelope_signature_message("fleet", "node-a", 8, "hash");
        assert!(!verify_signature(key.public(), &changed, &signature));
        let other_fleet = envelope_signature_message("fleet-2", "node-a", 7, "hash");
        assert!(!verify_signature(key.public(), &other_fleet, &signature));
        assert!(!verify_signature("not base64!", &message, &signature));
        assert!(!verify_signature(key.public(), &message, "AAAA"));
    }

    #[test]
    fn a_member_key_file_is_private_and_stable() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("fleet/node.key");
        let created = MemberKey::load_or_create(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let directory_mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(directory_mode & 0o777, 0o700);
        let loaded = MemberKey::load_or_create(&path).unwrap();
        assert_eq!(created.public(), loaded.public());

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            MemberKey::load(&path)
                .unwrap_err()
                .to_string()
                .contains("group or other")
        );
    }
}
