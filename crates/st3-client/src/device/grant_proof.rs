//! Verify the returned enrollment's content hashes and device -> person root -> node links.
//! The node key comes from the gateway: this checks the chain's binding, not gateway identity.
use super::*;
use serde_json::Value;

#[derive(Deserialize)]
struct Grant {
    id: String,
    batch_id: String,
    subject: String,
    kind: String,
    origin: String,
    actor: Option<String>,
    body: Value,
    predecessors: Vec<String>,
    signature: GrantSignature,
}

#[derive(Deserialize)]
struct GrantSignature {
    signer: String,
    #[serde(default)]
    on_behalf: Option<String>,
    key: String,
    chain: Vec<String>,
    nonce: String,
    signed_at_unix_ms: u64,
    signature: String,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    signed_fields: Vec<String>,
}

fn canonical(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonical).collect()),
        Value::Object(fields) => {
            let mut fields = fields.iter().collect::<Vec<_>>();
            fields.sort_unstable_by_key(|(key, _)| *key);
            Value::Object(
                fields
                    .into_iter()
                    .map(|(k, v)| (k.clone(), canonical(v)))
                    .collect(),
            )
        }
        _ => value.clone(),
    }
}

fn digest(value: &impl Serialize) -> Result<String> {
    let mut bytes = Vec::new();
    ciborium::into_writer(value, &mut bytes)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

impl Grant {
    fn verify(&self, id: &str, person: &str) -> Result<()> {
        ensure!(
            self.id == id,
            "Enrollment proof does not match the returned chain"
        );
        ensure!(
            self.kind == "principal.key-granted" && self.subject == person,
            "Enrollment proof is not a grant for the paired person"
        );
        let body = canonical(&self.body);
        let hash = digest(&(
            &self.batch_id,
            &self.subject,
            &self.kind,
            &self.origin,
            self.actor.as_deref(),
            &body,
            &self.predecessors,
        ))?;
        ensure!(
            hash == self.id,
            "Enrollment grant content does not match its claim ID"
        );
        let signature = &self.signature;
        ensure!(
            signature.format.is_none()
                && signature.signed_fields.is_empty()
                && signature.on_behalf.is_none(),
            "Unsupported enrollment grant signature"
        );
        let content = digest(&(
            "smallclaims.claim-content.v1",
            &self.subject,
            &self.kind,
            self.actor.as_deref(),
            &body,
        ))?;
        let bytes = format!(
            "smallclaims-claim-v1\n{content}\n{}\n\n{}\n{}\n{}\n{}",
            signature.signer,
            signature.key,
            signature.chain.join(","),
            signature.nonce,
            signature.signed_at_unix_ms,
        );
        let raw = URL_SAFE_NO_PAD.decode(
            signature
                .key
                .strip_prefix("p256:")
                .unwrap_or(&signature.key),
        )?;
        let signed = URL_SAFE_NO_PAD.decode(&signature.signature)?;
        let algorithm: &dyn signature::VerificationAlgorithm = if signature.key.starts_with("p256:")
        {
            &signature::ECDSA_P256_SHA256_FIXED
        } else {
            &signature::ED25519
        };
        signature::UnparsedPublicKey::new(algorithm, raw)
            .verify(bytes.as_bytes(), &signed)
            .map_err(|_| anyhow::anyhow!("Enrollment grant signature does not verify"))
    }

    fn field(&self, name: &str) -> Option<&str> {
        self.body.get("fields")?.get(name)?.as_str()
    }
}

pub(super) fn validate(session: &PairedSession, expected_key: &str) -> Result<()> {
    verify(
        &session.person_id,
        &session.device_key_chain,
        &session.device_key_proofs,
        expected_key,
    )
}

pub(super) fn verify(
    person: &str,
    chain: &[String],
    proofs: &[Value],
    expected_key: &str,
) -> Result<()> {
    ensure!(
        chain.len() == 2 && proofs.len() == 2,
        "Member did not return verifiable device and root grants; upgrade the member"
    );
    let mut grants = Vec::new();
    for (proof, id) in proofs.iter().zip(chain) {
        ensure!(
            serde_json::to_vec(proof)?.len() <= 64 * 1024,
            "Enrollment proof is too large"
        );
        // Do not expose an untrusted response value in a deserializer's error text.
        let grant: Grant = serde_json::from_value(proof.clone())
            .map_err(|_| anyhow::anyhow!("Incomplete enrollment grant proof"))?;
        grant.verify(id, person)?;
        grants.push(grant);
    }
    let (device, root) = (&grants[0], &grants[1]);
    ensure!(
        device.field("key") == Some(expected_key)
            && device.field("role") == Some("device")
            && device.actor.as_deref() == Some(person)
            && device.field("issuer") == Some(person)
            && device.signature.signer == person
            && device.field("issuer_key") == root.field("key")
            && Some(device.signature.key.as_str()) == root.field("key")
            && device.signature.chain == chain[1..],
        "Returned device grant does not bind our public key to the paired person's root"
    );
    ensure!(
        root.field("role") == Some("root")
            && root
                .field("issuer")
                .is_some_and(|issuer| issuer.starts_with("host/") && issuer.len() > 5)
            && root.field("issuer") == Some(root.signature.signer.as_str())
            && root.field("issuer_key") == Some(root.signature.key.as_str())
            && root.signature.chain.is_empty(),
        "Returned person root grant is not issued by the node authority"
    );
    Ok(())
}
