//! Software device keys and paired credentials share one atomic, private profile.
//! This is client state, never a graph replica or a peer configuration.
mod grant_proof;
pub(crate) mod transport;
use crate::{AssistantPairedSession, AssistantPrincipal, Client, DeviceSignature, MessageSendParameters, PersonPairedSession, PairingComplete};
use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{
    rand::{SecureRandom as _, SystemRandom},
    signature::{self, KeyPair as _},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, IsTerminal as _, Read as _, Write as _},
    os::unix::fs::{
        DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
    },
    path::{Path, PathBuf},
    sync::Arc,
};

/// The longest message, in UTF-8 bytes, whose text stays whole in the claim. A longer one keeps a
/// preview there, and a device signs the preview with the whole text's hash and length.
pub const INLINE_MAX_BYTES: usize = 8192;
const PREVIEW_BYTES: usize = 1024;

/// The start of a long message as the daemon keeps it in the claim: the first 1,024 bytes cut at a
/// character, trailing whitespace trimmed, then `…`. A device signs exactly this.
pub fn long_message_preview(text: &str) -> String {
    let mut end = PREVIEW_BYTES.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut preview = text[..end].trim_end().to_owned();
    preview.push('…');
    preview
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum KeyAlgorithm {
    Ed25519,
    P256,
}

/// DER PKCS#8, base64url encoded in the profile. Debug deliberately omits private material.
#[derive(Clone, Deserialize, Serialize)]
pub struct SigningKey {
    pub algorithm: KeyAlgorithm,
    pkcs8: String,
}

impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningKey")
            .field("algorithm", &self.algorithm)
            .finish_non_exhaustive()
    }
}

impl SigningKey {
    pub fn generate(algorithm: KeyAlgorithm) -> Result<Self> {
        let rng = SystemRandom::new();
        let document = match algorithm {
            KeyAlgorithm::Ed25519 => signature::Ed25519KeyPair::generate_pkcs8(&rng),
            KeyAlgorithm::P256 => signature::EcdsaKeyPair::generate_pkcs8(
                &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                &rng,
            ),
        }
        .map_err(|_| anyhow::anyhow!("Generate device signing key"))?;
        Self::from_pkcs8(algorithm, document.as_ref())
    }

    pub fn from_pkcs8(algorithm: KeyAlgorithm, document: &[u8]) -> Result<Self> {
        let key = Self {
            algorithm,
            pkcs8: URL_SAFE_NO_PAD.encode(document),
        };
        key.public_key()?;
        Ok(key)
    }

    /// Import a private regular DER PKCS#8 file, without following a symlink.
    pub fn import(algorithm: KeyAlgorithm, path: &Path) -> Result<Self> {
        let file = open_private(path)?.context("Private key file does not exist")?;
        ensure!(
            file.metadata()?.len() <= 16 * 1024,
            "Private key file is too large"
        );
        let mut bytes = Vec::new();
        file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 16 * 1024, "Private key file is too large");
        Self::from_pkcs8(algorithm, &bytes)
    }

    fn decoded(&self) -> Result<Vec<u8>> {
        URL_SAFE_NO_PAD
            .decode(&self.pkcs8)
            .context("Invalid private key encoding")
    }

    pub fn public_key(&self) -> Result<String> {
        let document = self.decoded()?;
        match self.algorithm {
            KeyAlgorithm::Ed25519 => {
                let key = signature::Ed25519KeyPair::from_pkcs8_maybe_unchecked(&document)
                    .map_err(|_| anyhow::anyhow!("Invalid Ed25519 PKCS#8 key"))?;
                Ok(URL_SAFE_NO_PAD.encode(key.public_key().as_ref()))
            }
            KeyAlgorithm::P256 => {
                let key = signature::EcdsaKeyPair::from_pkcs8(
                    &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                    &document,
                    &SystemRandom::new(),
                )
                .map_err(|_| anyhow::anyhow!("Invalid P-256 PKCS#8 key"))?;
                Ok(format!(
                    "p256:{}",
                    URL_SAFE_NO_PAD.encode(key.public_key().as_ref())
                ))
            }
        }
    }

    fn sign(&self, bytes: &[u8]) -> Result<String> {
        let document = self.decoded()?;
        let signed = match self.algorithm {
            KeyAlgorithm::Ed25519 => {
                signature::Ed25519KeyPair::from_pkcs8_maybe_unchecked(&document)
                    .map_err(|_| anyhow::anyhow!("Invalid Ed25519 PKCS#8 key"))?
                    .sign(bytes)
            }
            KeyAlgorithm::P256 => signature::EcdsaKeyPair::from_pkcs8(
                &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                &document,
                &SystemRandom::new(),
            )
            .map_err(|_| anyhow::anyhow!("Invalid P-256 PKCS#8 key"))?
            .sign(&SystemRandom::new(), bytes)
            .map_err(|_| anyhow::anyhow!("Sign with device key"))?,
        };
        Ok(URL_SAFE_NO_PAD.encode(signed.as_ref()))
    }

    /// Check actual possession before consuming a pairing code. This is a local self-test,
    /// not a new server authorization protocol or a claim that the delegation verifies.
    fn prove(&self, pairing_id: &str, code: &str) -> Result<()> {
        let public = self.public_key()?;
        let bytes = format!("st-device-key-self-test-v1\n{pairing_id}\n{code}\n{public}");
        let signed = URL_SAFE_NO_PAD.decode(self.sign(bytes.as_bytes())?)?;
        let raw = URL_SAFE_NO_PAD.decode(public.strip_prefix("p256:").unwrap_or(&public))?;
        let algorithm: &dyn signature::VerificationAlgorithm = match self.algorithm {
            KeyAlgorithm::Ed25519 => &signature::ED25519,
            KeyAlgorithm::P256 => &signature::ECDSA_P256_SHA256_FIXED,
        };
        signature::UnparsedPublicKey::new(algorithm, raw)
            .verify(bytes.as_bytes(), &signed)
            .map_err(|_| anyhow::anyhow!("Device signing key failed its possession self-test"))
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Device {
    pub endpoint: String,
    /// Explicit HTTP public-address override, retained as private profile configuration.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_public_http: bool,
    /// Trust supplied separately from the code on the trusted pairing machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_root_fingerprint: Option<String>,
    pub session: PersonPairedSession,
    /// Old observer and legacy stui profiles have no signing key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_key: Option<SigningKey>,
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("endpoint", &self.endpoint)
            .field("device_id", &self.session.device_id)
            .field("person_id", &self.session.person_id)
            .field("signing_key", &self.signing_key)
            .finish_non_exhaustive()
    }
}

impl Device {
    pub fn client(&self) -> Result<Client> {
        let mut client = Client::fabric_loopback(&self.endpoint, &self.session.credential);
        client.http = transport::client(&self.endpoint, self.allow_public_http)?;
        client.device_http_policy = Some(self.allow_public_http);
        if self.signing_key.is_some() {
            client.signing_device = Some(Arc::new(self.clone()));
        }
        Ok(client)
    }

    /// Sign the seven fields in device-signing-v1 (nine for a message past `INLINE_MAX_BYTES`),
    /// including the canonical message subject.
    /// These fields contain only strings, nulls, arrays of strings and one whole number, so
    /// serde_json's compact encoding is their JCS encoding (no floating point or object ordering
    /// to normalize).
    pub fn sign_message(
        &self,
        idempotency_key: &str,
        parameters: &MessageSendParameters,
    ) -> Result<DeviceSignature> {
        let key = self
            .signing_key
            .as_ref()
            .context("This device has no enrolled signing key")?;
        ensure!(
            !self.session.device_key_chain.is_empty()
                && self
                    .session
                    .scopes
                    .iter()
                    .any(|scope| scope == "control.messages"),
            "This device is not delegated message signing"
        );
        let nonce = random_nonce()?;
        let signed_at_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis()
            .try_into()?;
        self.sign_message_at(idempotency_key, parameters, &nonce, signed_at_unix_ms, key)
    }

    fn sign_message_at(
        &self,
        idempotency_key: &str,
        parameters: &MessageSendParameters,
        nonce: &str,
        signed_at_unix_ms: u64,
        key: &SigningKey,
    ) -> Result<DeviceSignature> {
        let public = key.public_key()?;
        let subject_hash = Sha256::digest(idempotency_key.as_bytes());
        let subject = format!(
            "message/{:016x}",
            u64::from_be_bytes(subject_hash[..8].try_into()?)
        );
        // Past `INLINE_MAX_BYTES` the daemon keeps only a preview in the claim and names the
        // whole text by its hash and length, so that is what is signed (docs/st3/long-messages.md).
        let long = parameters.content.len() > INLINE_MAX_BYTES;
        let mut fields = Vec::with_capacity(9);
        if long {
            fields.push((
                "body_bytes",
                serde_json::to_value(parameters.content.len() as u64)?,
            ));
            fields.push((
                "body_sha256",
                serde_json::to_value(
                    Sha256::digest(parameters.content.as_bytes())
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>(),
                )?,
            ));
        }
        fields.extend([
            (
                "content",
                if long {
                    serde_json::to_value(long_message_preview(&parameters.content))?
                } else {
                    serde_json::to_value(&parameters.content)?
                },
            ),
            ("from", serde_json::to_value(&self.session.person_id)?),
            (
                "in_reply_to",
                serde_json::to_value(&parameters.in_reply_to)?,
            ),
            ("session_id", serde_json::to_value(&parameters.session_id)?),
            ("tags", serde_json::to_value(&parameters.tags)?),
            ("title", serde_json::to_value(&parameters.title)?),
            ("to", serde_json::to_value(&parameters.to)?),
        ]);
        let mut lines = vec![
            "smallclaims-claim-fields-v1".to_owned(),
            subject,
            "message.sent".into(),
            self.session.person_id.clone(),
        ];
        for (name, value) in &fields {
            lines.push(format!("{name}={value}"));
        }
        lines.extend([
            self.session.person_id.clone(),
            String::new(),
            public.clone(),
            self.session.device_key_chain.join(","),
            nonce.to_owned(),
            signed_at_unix_ms.to_string(),
        ]);
        Ok(DeviceSignature {
            signer: self.session.person_id.clone(),
            key: public,
            chain: self.session.device_key_chain.clone(),
            nonce: nonce.into(),
            signed_at_unix_ms,
            signature: key.sign(lines.join("\n").as_bytes())?,
            format: "fields-v1".into(),
            signed_fields: fields.iter().map(|(name, _)| (*name).to_owned()).collect(),
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Profile {
    pub devices: Vec<Device>,
}

impl Profile {
    pub fn person(&self) -> Result<&str> {
        let person = &self
            .devices
            .first()
            .context("No paired member; run st devices complete URL PAIRING_ID")?
            .session
            .person_id;
        ensure!(
            person.starts_with("person/") && person.len() > 7 && person.matches('/').count() == 1,
            "Invalid paired person"
        );
        ensure!(
            self.devices
                .iter()
                .all(|device| &device.session.person_id == person),
            "All paired members must delegate the same person"
        );
        Ok(person)
    }

    pub fn clients(&self) -> Result<Vec<Client>> {
        self.devices.iter().map(Device::client).collect()
    }

    pub fn load(path: &Path) -> Result<Option<Self>> {
        let Some(file) = open_private(path)? else {
            return Ok(None);
        };
        ensure!(
            file.metadata()?.len() <= 1024 * 1024,
            "Device profile is too large"
        );
        let mut profile: Self = serde_json::from_reader(file).context("Read device profile")?;
        profile.person()?;
        for device in &mut profile.devices {
            if let Some(fingerprint) = &device.person_root_fingerprint {
                validate_fingerprint(fingerprint)?;
            }
            device.endpoint =
                validate_endpoint_with_http_policy(&device.endpoint, device.allow_public_http)?;
            ensure!(
                device.session.credential.len() >= 32,
                "Invalid device credential"
            );
            if let Some(key) = &device.signing_key {
                key.public_key()?;
                ensure!(
                    !device.session.device_key_chain.is_empty()
                        && device
                            .session
                            .scopes
                            .iter()
                            .any(|scope| scope == "control.messages"),
                    "Device signing key has no message delegation"
                );
            }
        }
        Ok(Some(profile))
    }

    fn save_in(&self, path: &Path, parent: &File) -> Result<()> {
        self.person()?;
        save_private(self, path, parent)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let (parent, _lock) = prepare(path)?;
        self.save_in(path, &parent)
    }
}

/// External assistant credentials live separately from person device delegation.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantDevice {
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_public_http: bool,
    pub session: AssistantPairedSession,
    pub signing_key: SigningKey,
}

impl std::fmt::Debug for AssistantDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssistantDevice")
            .field("endpoint", &self.endpoint)
            .field("device_id", &self.session.device_id)
            .field("grant_id", &self.session.grant.id)
            .field("signing_key", &self.signing_key)
            .finish_non_exhaustive()
    }
}

impl AssistantDevice {
    pub fn client(&self) -> Result<Client> {
        let endpoint = validate_endpoint_with_http_policy(&self.endpoint, self.allow_public_http)?;
        let mut client = Client::fabric_loopback(&endpoint, &self.session.credential);
        client.http = transport::client(&endpoint, self.allow_public_http)?;
        // Never install the assistant key as a person message-signing device.
        Ok(client)
    }

    fn validate(&mut self) -> Result<()> {
        self.endpoint = validate_endpoint_with_http_policy(&self.endpoint, self.allow_public_http)?;
        let grant = &self.session.grant;
        // The selected gateway returns the member-sealed issuer. Check its concrete person
        // shape, not a deployment identity; assistant keys never require person grant proofs.
        let issuer = grant.issuer.strip_prefix("person/");
        ensure!(
            self.session.kind == "assistant-paired-session"
                && self.session.api_version == crate::API_VERSION
                && self.session.principal == AssistantPrincipal::Chatgpt
                && self.session.credential.len() >= 32
                && self.session.device_id == grant.device_id
                && self.session.principal == grant.principal
                && self.session.sender_class == grant.sender_class
                && self.session.session_actor == grant.session_actor
                && grant.session_actor == format!("assistant/chatgpt/session/{}", grant.id)
                && self.session.scopes == grant.scopes
                && self.session.expires_at == grant.expires_at
                && issuer.is_some_and(|person| {
                    !person.is_empty()
                        && !person.contains('/')
                        && !person.chars().any(char::is_whitespace)
                })
                && grant.kind == "assistant-grant"
                && grant.policy_revision >= 1
                && grant.disclosure_consent
                && !grant.consent_evidence.trim().is_empty()
                && grant.consent_evidence.len() <= 512
                && grant.recipient_allowlist.iter().all(|id| id.starts_with("agent/") && !id.chars().any(char::is_whitespace))
                && !grant.revoked,
            "Invalid assistant session or grant"
        );
        let key_id = format!("{:x}", Sha256::digest(self.signing_key.public_key()?.as_bytes()));
        ensure!(key_id == grant.key_id, "Assistant grant key does not match the local key");
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantProfile {
    pub devices: Vec<AssistantDevice>,
}

impl AssistantProfile {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let Some(file) = open_private(path)? else { return Ok(None); };
        ensure!(file.metadata()?.len() <= 1024 * 1024, "Assistant profile is too large");
        let mut profile: Self = serde_json::from_reader(file).context("Read assistant profile")?;
        for device in &mut profile.devices { device.validate()?; }
        Ok(Some(profile))
    }

    pub fn clients(&self) -> Result<Vec<Client>> {
        self.devices.iter().map(AssistantDevice::client).collect()
    }
}

fn save_private(value: &impl Serialize, path: &Path, parent: &File) -> Result<()> {
    ensure!(
        parent.metadata()?.permissions().mode() & 0o200 != 0,
        "Device profile directory is not writable"
    );
    let mut temporary = tempfile::NamedTempFile::new_in(path.parent().context("Device profile needs a parent directory")?)?;
    temporary.as_file().set_permissions(fs::Permissions::from_mode(0o600))?;
    serde_json::to_writer(&mut temporary, value)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    parent.sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    // The atomic commit succeeded; a later directory sync cannot undo it.
    let _ = parent.sync_all();
    Ok(())
}

fn open_private(path: &Path) -> Result<Option<File>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.permissions().mode() & 0o077 == 0
            && metadata.uid() == unsafe { libc::geteuid() },
        "Device credentials and private keys must be private regular files owned by this user (chmod 600)"
    );
    Ok(Some(file))
}

/// Hold an advisory lock across remote completion and the local commit, so simultaneous
/// completions cannot silently replace each other's newly paired members or rotate a key.
fn prepare(path: &Path) -> Result<(File, File)> {
    ensure!(
        path.is_absolute() && path.file_name().is_some(),
        "Use an absolute device profile path"
    );
    let parent_path = path
        .parent()
        .context("Device profile needs a parent directory")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent_path)?;
    let parent = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(parent_path)?;
    let metadata = parent.metadata()?;
    ensure!(
        metadata.permissions().mode() & 0o022 == 0 && metadata.uid() == unsafe { libc::geteuid() },
        "Device profile directory must belong to this user and not be writable by others"
    );
    let lock_path = path.with_extension("lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(lock_path)?;
    let metadata = lock.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.permissions().mode() & 0o077 == 0
            && metadata.uid() == unsafe { libc::geteuid() },
        "Device profile lock must be a private regular file"
    );
    lock.try_lock()
        .context("Another process is updating this device profile; retry when it finishes")?;
    // Preflight writes before consuming the code. No previous credential or key is touched.
    let test_file = tempfile::NamedTempFile::new_in(parent_path)?;
    test_file.as_file().sync_all()?;
    parent.sync_all()?;
    Ok((parent, lock))
}

pub fn profile_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("Set XDG_CONFIG_HOME or HOME for the device profile")?;
    ensure!(base.is_absolute(), "The config directory must be absolute");
    // Preserve the existing stui location: both entrypoints now use this profile.
    Ok(base.join("st3/stui-devices.json"))
}

pub fn assistant_profile_path() -> Result<PathBuf> {
    Ok(profile_path()?.with_file_name("assistant-devices.json"))
}

/// Consume an assistant challenge into its own private profile, never a person chain.
/// Trust the returned member-sealed issuer only from the selected gateway completion;
/// this path neither delegates person authority nor verifies a person signing chain.
pub async fn complete_assistant(
    path: &Path,
    endpoint: &str,
    pairing_id: &str,
    code: &str,
    key: SigningKey,
    allow_public_http: bool,
) -> Result<AssistantDevice> {
    let endpoint = validate_endpoint_with_http_policy(endpoint, allow_public_http)?;
    ensure!(!code.trim().is_empty(), "A pairing code is required");
    key.prove(pairing_id, code.trim())?;
    let public = key.public_key()?;
    let (parent, _lock) = prepare(path)?;
    let mut profile = AssistantProfile::load(path)?.unwrap_or_default();
    let mut client = Client::fabric_pairing(&endpoint);
    client.http = transport::client(&endpoint, allow_public_http)?;
    let session: crate::Envelope<AssistantPairedSession> = client.post(
        &format!("/v1/client/pairings/{}/complete", crate::percent_encode(pairing_id.trim_start_matches("pairing/"))),
        &PairingComplete {
            api_version: crate::API_VERSION.into(),
            code: code.trim().into(),
            device_public_key: public,
            key_storage: Some("software".into()),
        },
    ).await.context("Complete assistant pairing")?;
    let mut device = AssistantDevice { endpoint, allow_public_http, session: session.value, signing_key: key };
    let committed = (|| -> Result<()> {
        device.validate()?;
        profile.devices.retain(|existing| existing.endpoint != device.endpoint);
        profile.devices.push(device.clone());
        save_private(&profile, path, &parent)
    })();
    committed.context("The server consumed the assistant pairing code, but local completion failed. Inspect and revoke the new assistant grant on the trusted member before pairing again; the previous profile was retained")?;
    Ok(device)
}

pub fn validate_endpoint(endpoint: &str) -> Result<String> {
    validate_endpoint_with_http_policy(endpoint, false)
}

fn validate_endpoint_with_http_policy(endpoint: &str, allow_public_http: bool) -> Result<String> {
    let url =
        reqwest::Url::parse(endpoint).context("Member gateway must be an HTTP or HTTPS URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/",
        "Use the member gateway's HTTP or HTTPS origin, without credentials or a path"
    );
    transport::validate(&url, allow_public_http)?;
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn random_nonce() -> Result<String> {
    let mut nonce = [0u8; 16];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| anyhow::anyhow!("Generate device nonce"))?;
    Ok(URL_SAFE_NO_PAD.encode(nonce))
}

/// Consume the existing single-use challenge and atomically retain the returned bearer,
/// delegation chain and private key together. An observer retains no private signing key.
/// Network/server failures or local failures before the commit preserve the previous profile.
/// Supply independent trust with `complete_with_options`; this legacy entry point has no pin
/// and therefore refuses before submitting the code.
pub async fn complete(
    path: &Path,
    endpoint: &str,
    pairing_id: &str,
    code: &str,
    key: SigningKey,
) -> Result<Device> {
    complete_with_http_policy(path, endpoint, pairing_id, code, key, false).await
}

/// Legacy HTTP policy entry point. Use `complete_with_options` to also supply identity trust.
/// An HTTP override alone cannot bypass the required fingerprint.
pub async fn complete_with_http_policy(
    path: &Path,
    endpoint: &str,
    pairing_id: &str,
    code: &str,
    key: SigningKey,
    allow_public_http: bool,
) -> Result<Device> {
    complete_with_options(
        path,
        endpoint,
        pairing_id,
        code,
        key,
        CompletionOptions {
            allow_public_http,
            ..Default::default()
        },
    )
    .await
}

/// An out-of-band pin is required unless the caller explicitly chooses unpinned enrollment.
#[derive(Clone, Copy, Debug, Default)]
pub struct CompletionOptions<'a> {
    pub allow_public_http: bool,
    pub fingerprint: Option<&'a str>,
    pub unpinned: bool,
}

impl CompletionOptions<'_> {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !(self.unpinned && self.fingerprint.is_some()),
            "Choose --fingerprint or --unpinned, never both"
        );
        if let Some(fingerprint) = self.fingerprint {
            validate_fingerprint(fingerprint)?;
        } else {
            ensure!(
                self.unpinned,
                "A person-root --fingerprint from the trusted machine is required; use --unpinned only to explicitly bypass identity verification"
            );
        }
        Ok(())
    }
}

/// SHA-256 of the canonical public-key string, retaining the full digest.
pub fn person_root_fingerprint(key: &str) -> Result<String> {
    let raw = key.strip_prefix("p256:").unwrap_or(key);
    let bytes = URL_SAFE_NO_PAD
        .decode(raw)
        .context("Invalid person-root public key")?;
    ensure!(
        URL_SAFE_NO_PAD.encode(&bytes) == raw
            && if key.starts_with("p256:") {
                bytes.len() == 65 && bytes[0] == 4
            } else {
                bytes.len() == 32
            },
        "Invalid person-root public key"
    );
    Ok(format!(
        "sha256:{}",
        URL_SAFE_NO_PAD.encode(Sha256::digest(key.as_bytes()))
    ))
}

pub fn validate_fingerprint(fingerprint: &str) -> Result<()> {
    let raw = fingerprint
        .strip_prefix("sha256:")
        .context("Use the full sha256: person-root fingerprint printed on the trusted machine")?;
    let bytes = URL_SAFE_NO_PAD
        .decode(raw)
        .context("Invalid person-root fingerprint")?;
    ensure!(
        bytes.len() == 32 && URL_SAFE_NO_PAD.encode(bytes) == raw,
        "Use the full sha256: person-root fingerprint printed on the trusted machine"
    );
    Ok(())
}

pub async fn complete_with_options(
    path: &Path,
    endpoint: &str,
    pairing_id: &str,
    code: &str,
    key: SigningKey,
    options: CompletionOptions<'_>,
) -> Result<Device> {
    options.validate()?;
    let allow_public_http = options.allow_public_http;
    if options.unpinned {
        eprintln!(
            "WARNING: UNPINNED PAIRING. An active attacker can forge the identity-key chain. Obtain --fingerprint separately from the trusted machine to verify the person's root key."
        );
    }
    let endpoint = validate_endpoint_with_http_policy(endpoint, allow_public_http)?;
    let mut client = Client::fabric_pairing(&endpoint);
    client.http = transport::client(&endpoint, allow_public_http)?;
    ensure!(!code.trim().is_empty(), "A pairing code is required");
    key.prove(pairing_id, code.trim())?;
    let public = key.public_key()?;
    let (parent, _lock) = prepare(path)?;
    let mut profile = Profile::load(path)?.unwrap_or_default();
    if profile
        .devices
        .iter()
        .any(|device| device.endpoint == endpoint && device.person_root_fingerprint.is_none())
    {
        eprintln!(
            "WARNING: This existing pairing has no trusted person-root pin. Re-pair with the fingerprint from the trusted machine; --unpinned explicitly keeps this risk."
        );
    }
    let capabilities: serde_json::Value = client.get("/v1/client/capabilities").await
        .context("Member cannot advertise pairing proof support; upgrade the member before retrying. The pairing code was not submitted")?;
    ensure!(
        capabilities["api_version"] == crate::API_VERSION
            && capabilities["capabilities"]
                .as_array()
                .is_some_and(|capabilities| capabilities.iter().any(|capability| {
                    capability["id"] == "device-key-proofs"
                        && capability["version"] == 1
                        && capability["state"] == "granted"
                })),
        "Member does not support verifiable device grants; upgrade it before retrying. The pairing code was not submitted"
    );
    let session = client.pairing_complete_person(pairing_id,
    &PairingComplete {
        api_version: crate::API_VERSION.into(),
        code: code.trim().into(),
        device_public_key: public.clone(),
        key_storage: Some("software".into()),
    },)
        .await
        .context("Complete device pairing")?
        .value;
    let created_device = session.device_id.clone();
    let committed = (|| -> Result<Device> {
        if let Some(existing) = profile.devices.first() {
            ensure!(
                existing.session.person_id == session.person_id,
                "This member delegates a different person; use a separate profile"
            );
        }
        ensure!(
            session.credential.len() >= 32,
            "Pairing returned an invalid credential"
        );
        let signs = session
            .scopes
            .iter()
            .any(|scope| scope == "control.messages");
        ensure!(
            !signs || !session.device_key_chain.is_empty(),
            "Pairing granted messages without enrolling the signing key; upgrade the member"
        );
        ensure!(
            signs || (session.device_key_chain.is_empty() && session.device_key_proofs.is_empty()),
            "Read-only pairing unexpectedly enrolled a signing key"
        );
        if signs {
            if let Some(fingerprint) = options.fingerprint {
                verify_device_key_proofs_pinned(
                    &session.person_id,
                    &session.device_key_chain,
                    &session.device_key_proofs,
                    &public,
                    fingerprint,
                )?;
            } else {
                grant_proof::validate(&session, &public)?;
            }
        } else if let Some(proof) = &session.person_root_key_proof {
            verify_person_root_key_proof(&session.person_id, proof, options.fingerprint)?;
        } else {
            ensure!(
                options.unpinned,
                "Member did not return a verifiable person-root grant for read-only pairing; upgrade the member or explicitly use --unpinned"
            );
        }
        let device = Device {
            endpoint,
            allow_public_http,
            person_root_fingerprint: options.fingerprint.map(str::to_owned),
            session,
            signing_key: signs.then_some(key),
        };
        profile
            .devices
            .retain(|existing| existing.endpoint != device.endpoint);
        profile.devices.push(device.clone());
        profile.save_in(path, &parent)?;
        Ok(device)
    })();
    committed.with_context(|| {
        let safe_id = created_device.strip_prefix("device/").filter(|suffix| {
            suffix.len() == 24 && suffix.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
        if safe_id.is_some() {
            format!("The server consumed the pairing code, but local completion failed. Possible orphaned device {created_device}; inspect it on the trusted member and revoke with `st devices revoke {created_device} --as <your-person-id>` before starting a new pairing. The previous profile was retained")
        } else {
            "The server consumed the pairing code, but local completion failed and returned an unusable device ID. Inspect `st devices ls --as <your-person-id>` on the trusted member and revoke the new device before starting a new pairing. The previous profile was retained".into()
        }
    })
}

/// Verify public enrollment receipts without adding authority or trusting a node key independently.
pub fn verify_device_key_proofs(
    person: &str,
    chain: &[String],
    proofs: &[serde_json::Value],
    expected_key: &str,
) -> Result<()> {
    grant_proof::verify(person, chain, proofs, expected_key)
}

/// Verify the chain and bind its person-root key to independently supplied trust.
pub fn verify_device_key_proofs_pinned(
    person: &str,
    chain: &[String],
    proofs: &[serde_json::Value],
    expected_key: &str,
    fingerprint: &str,
) -> Result<()> {
    validate_fingerprint(fingerprint)?;
    grant_proof::verify(person, chain, proofs, expected_key)?;
    verify_person_root_key_proof(person, &proofs[1], Some(fingerprint))
}

/// A read-only device gets the root receipt without a grant to sign as the person.
pub fn verify_person_root_key_proof(
    person: &str,
    proof: &serde_json::Value,
    fingerprint: Option<&str>,
) -> Result<()> {
    grant_proof::verify_root(person, proof, fingerprint)
}

pub fn read_pairing_code() -> Result<String> {
    if !io::stdin().is_terminal() {
        let mut code = String::new();
        io::stdin().read_line(&mut code)?;
        ensure!(
            !code.trim().is_empty(),
            "A pairing code is required on stdin"
        );
        return Ok(code);
    }
    use crossterm::{
        event::{self, Event, KeyCode, KeyModifiers},
        terminal,
    };
    eprint!("Pairing code: ");
    io::stderr().flush()?;
    terminal::enable_raw_mode()?;
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = terminal::disable_raw_mode();
            eprintln!();
        }
    }
    let _restore = Restore;
    let stopping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, stopping.clone())?;
    }
    let mut code = String::new();
    loop {
        ensure!(
            !stopping.load(std::sync::atomic::Ordering::Relaxed),
            "Pairing cancelled"
        );
        if !event::poll(std::time::Duration::from_millis(100))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Enter => break,
                KeyCode::Esc => anyhow::bail!("Pairing cancelled"),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    anyhow::bail!("Pairing cancelled")
                }
                KeyCode::Char(character) => code.push(character),
                KeyCode::Backspace => {
                    code.pop();
                }
                _ => {}
            }
        }
    }
    ensure!(!code.trim().is_empty(), "A pairing code is required");
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn assistant_session_issuer_is_a_concrete_person_not_a_fixed_deployment_owner() {
        let paired: crate::Envelope<AssistantPairedSession> = serde_json::from_str(include_str!(
            "../../../docs/st3/client-v0/fixtures/assistant-pairing.json"
        ))
        .unwrap();
        let signing_key = SigningKey::generate(KeyAlgorithm::Ed25519).unwrap();
        let mut device = AssistantDevice {
            endpoint: "https://member.example".into(),
            allow_public_http: false,
            session: paired.value,
            signing_key,
        };
        device.session.grant.key_id = format!(
            "{:x}",
            Sha256::digest(device.signing_key.public_key().unwrap().as_bytes())
        );
        for issuer in ["person/operator", "person/other"] {
            device.session.grant.issuer = issuer.into();
            device.validate().unwrap();
        }
        for issuer in [
            "",
            "person/",
            "person/operator/session/example",
            "agent/operator",
            "assistant/chatgpt",
            "person/operator name",
            "person/operator\n",
        ] {
            device.session.grant.issuer = issuer.into();
            assert!(device.validate().is_err(), "Accepted invalid issuer {issuer:?}");
        }
    }

    #[test]
    fn shared_signer_matches_the_protocol_vectors_and_keeps_secrets_out_of_debug() {
        let vectors: Value = serde_json::from_str(include_str!(
            "../../../fixtures/clients/device-signing-v1.json"
        ))
        .unwrap();
        let key = SigningKey::from_pkcs8(
            KeyAlgorithm::P256,
            &hex::decode(vectors["key_pkcs8_hex"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        for case in vectors["cases"].as_array().unwrap() {
            let device = Device {
                endpoint: "https://member.example".into(),
                allow_public_http: false,
                person_root_fingerprint: None,
                session: PersonPairedSession { kind: "paired-session".into(),
                device_id: "device/vector".into(),
                person_id: case["signer"].as_str().unwrap().into(),
                session_actor: "person/avery/session/vector".into(),
                credential: "test-only-bearer-000000000000000000000".into(),
                scopes: vec!["control.messages".into()],
                expires_at: "2026-11-01T00:00:00Z".into(),
                device_key_chain: serde_json::from_value(case["chain"].clone()).unwrap(),
                device_key_proofs: Vec::new(),
                person_root_key_proof: None, },
                signing_key: Some(key.clone()),
            };
            let fields = &case["fields"];
            // A long case carries what the person sent as a unit and a count; its `content`
            // field is only the preview that is signed.
            let content = match case.get("body") {
                Some(body) => body["unit"]
                    .as_str()
                    .unwrap()
                    .repeat(body["repeat"].as_u64().unwrap() as usize),
                None => fields["content"].as_str().unwrap().to_owned(),
            };
            let parameters = MessageSendParameters {
                to: fields["to"].as_str().unwrap().into(),
                content,
                title: serde_json::from_value(fields["title"].clone()).unwrap(),
                in_reply_to: serde_json::from_value(fields["in_reply_to"].clone()).unwrap(),
                session_id: serde_json::from_value(fields["session_id"].clone()).unwrap(),
                tags: serde_json::from_value(fields["tags"].clone()).unwrap(),
                kind: crate::MessageKind::Wake,
                attachments: vec![],
                signature: None,
            };
            let signed = device
                .sign_message_at(
                    case["idempotency_key"].as_str().unwrap(),
                    &parameters,
                    case["nonce"].as_str().unwrap(),
                    case["signed_at_unix_ms"].as_u64().unwrap(),
                    &key,
                )
                .unwrap();
            assert_eq!(signed.key, case["key"]);
            assert_eq!(
                signed.signed_fields,
                serde_json::from_value::<Vec<String>>(case["signed_fields"].clone()).unwrap(),
                "{}",
                case["name"]
            );
            let raw = URL_SAFE_NO_PAD
                .decode(signed.key.strip_prefix("p256:").unwrap())
                .unwrap();
            signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, raw)
                .verify(
                    case["signed_bytes"].as_str().unwrap().as_bytes(),
                    &URL_SAFE_NO_PAD.decode(&signed.signature).unwrap(),
                )
                .unwrap();
            let debug = format!(
                "{device:?} {:?} {:?}",
                device.client().unwrap(),
                Profile {
                    devices: vec![device.clone()]
                }
            );
            assert!(!debug.contains(&device.session.credential));
            assert!(!debug.contains(&key.pkcs8));
        }
    }

    #[test]
    fn both_key_algorithms_generate_import_and_prove_possession_without_accepting_public_keys() {
        let root = tempfile::tempdir().unwrap();
        for algorithm in [KeyAlgorithm::Ed25519, KeyAlgorithm::P256] {
            let key = SigningKey::generate(algorithm).unwrap();
            key.prove("pairing/test", "single-use-code").unwrap();
            let path = root.path().join("private.der");
            fs::write(&path, key.decoded().unwrap()).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let imported = SigningKey::import(algorithm, &path).unwrap();
            assert_eq!(key.public_key().unwrap(), imported.public_key().unwrap());
            let wrong = match algorithm {
                KeyAlgorithm::Ed25519 => KeyAlgorithm::P256,
                KeyAlgorithm::P256 => KeyAlgorithm::Ed25519,
            };
            assert!(SigningKey::import(wrong, &path).is_err());
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(SigningKey::import(algorithm, &path).is_err());
            let public = key.public_key().unwrap();
            assert!(
                SigningKey::from_pkcs8(
                    algorithm,
                    &URL_SAFE_NO_PAD
                        .decode(public.strip_prefix("p256:").unwrap_or(&public))
                        .unwrap()
                )
                .is_err()
            );
        }
        let link = root.path().join("link.der");
        std::os::unix::fs::symlink(root.path().join("private.der"), &link).unwrap();
        assert!(SigningKey::import(KeyAlgorithm::P256, &link).is_err());
        // RFC 8410/OpenSSL's Ed25519 PKCS#8 v1 has the seed but no embedded public key.
        let mut document = hex::decode("302e020100300506032b657004220420").unwrap();
        document.extend([7_u8; 32]);
        SigningKey::from_pkcs8(KeyAlgorithm::Ed25519, &document)
            .unwrap()
            .prove("pairing/v1", "code")
            .unwrap();
    }
}
