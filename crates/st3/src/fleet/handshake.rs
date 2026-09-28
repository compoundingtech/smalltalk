//! The join handshake. One request and one answer.
//!
//! The joiner proves the invite token with an HMAC over the request and signs it with its new
//! member key. The sponsor seals the fleet secret with ChaCha20-Poly1305 under a key from
//! HKDF over an X25519 agreement between fresh ephemeral keys, salted with the token, and signs
//! the whole answer with its member key. The joiner checks that key against the fingerprint in
//! the code before it opens anything.

use anyhow::{Context as _, Result};
use base64::Engine as _;
use hmac::{Hmac, Mac as _};
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use ring::agreement::{self, EphemeralPrivateKey, UnparsedPublicKey, X25519};
use ring::hkdf;
use ring::rand::{SecureRandom as _, SystemRandom};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::code::{JoinCode, fingerprint};
use super::keys::{MemberKey, verify_signature};

pub const PROTOCOL: &str = "st3-join-v1";
const SEAL_INFO: &[u8] = b"st3-join-v1 seal";

type HmacSha256 = Hmac<Sha256>;

fn encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn decode(text: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .context("the join message has invalid base64")
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WriterHead {
    pub sequence: u64,
    pub hash: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JoinRequest {
    pub protocol: String,
    pub invite: String,
    pub name: String,
    pub mode: String,
    pub member_key: String,
    pub ephemeral: String,
    #[serde(default)]
    pub writer_head: Option<WriterHead>,
    pub build: String,
    #[serde(default)]
    pub migrate: bool,
    pub proof: String,
    pub signature: String,
}

impl JoinRequest {
    /// Every field except the proof and signature, as JSON with sorted keys.
    pub fn transcript(&self) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "build": self.build,
            "ephemeral": self.ephemeral,
            "invite": self.invite,
            "member_key": self.member_key,
            "migrate": self.migrate,
            "mode": self.mode,
            "name": self.name,
            "protocol": self.protocol,
            "writer_head": self.writer_head,
        }))
        .expect("a JSON value serializes")
    }
}

/// What the sponsor seals for the joiner. A migration carries no secret.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SealedJoin {
    pub fleet_id: String,
    #[serde(default)]
    pub secret: Option<String>,
    pub anchor_key: String,
    pub sponsor: String,
    #[serde(default)]
    pub writer_floor: Option<u64>,
    #[serde(default)]
    pub fabric_protocol: Option<String>,
    #[serde(default)]
    pub admitted_claim: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JoinResponse {
    pub protocol: String,
    pub sponsor: String,
    pub sponsor_key: String,
    pub ephemeral: String,
    pub nonce: String,
    pub sealed: String,
    pub signature: String,
}

impl JoinResponse {
    fn signed_bytes(&self, request: &JoinRequest) -> Vec<u8> {
        let mut bytes = request.transcript();
        for part in [
            &request.proof,
            &self.sponsor,
            &self.sponsor_key,
            &self.ephemeral,
            &self.nonce,
            &self.sealed,
        ] {
            bytes.push(b'\n');
            bytes.extend(part.as_bytes());
        }
        bytes
    }
}

fn proof(token: &[u8], transcript: &[u8]) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(token).expect("HMAC accepts any key length");
    mac.update(transcript);
    mac
}

fn aad(transcript: &[u8], sponsor_key: &str, sponsor_ephemeral: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(transcript);
    digest.update(sponsor_key.as_bytes());
    digest.update(sponsor_ephemeral);
    digest.finalize().into()
}

struct SealKeyLength;

impl hkdf::KeyType for SealKeyLength {
    fn len(&self) -> usize {
        32
    }
}

fn seal_key(shared: &[u8], token: &[u8], aad: &[u8; 32]) -> Result<LessSafeKey> {
    let mut key = [0_u8; 32];
    hkdf::Salt::new(hkdf::HKDF_SHA256, token)
        .extract(shared)
        .expand(&[SEAL_INFO, aad], SealKeyLength)
        .map_err(|_| anyhow::anyhow!("derive the join seal key"))?
        .fill(&mut key)
        .map_err(|_| anyhow::anyhow!("derive the join seal key"))?;
    Ok(LessSafeKey::new(
        UnboundKey::new(&CHACHA20_POLY1305, &key)
            .map_err(|_| anyhow::anyhow!("build the join seal key"))?,
    ))
}

/// The joiner's half of a handshake in progress. The ephemeral private key lives only here.
pub struct Joiner {
    ephemeral: EphemeralPrivateKey,
    token: [u8; 16],
    fingerprint: [u8; 16],
    request: JoinRequest,
}

impl Joiner {
    pub fn start(
        code: &JoinCode,
        member: &MemberKey,
        name: &str,
        mode: &str,
        writer_head: Option<WriterHead>,
        build: &str,
    ) -> Result<Self> {
        let ephemeral = EphemeralPrivateKey::generate(&X25519, &SystemRandom::new())
            .map_err(|_| anyhow::anyhow!("generate an ephemeral key"))?;
        let public = ephemeral
            .compute_public_key()
            .map_err(|_| anyhow::anyhow!("compute an ephemeral key"))?;
        let mut request = JoinRequest {
            protocol: PROTOCOL.into(),
            invite: code.invite_id(),
            name: name.into(),
            mode: mode.into(),
            member_key: member.public().into(),
            ephemeral: encode(public.as_ref()),
            writer_head,
            build: build.into(),
            migrate: code.migrate,
            proof: String::new(),
            signature: String::new(),
        };
        let transcript = request.transcript();
        request.proof = encode(&proof(&code.token, &transcript).finalize().into_bytes());
        request.signature = member.sign(&transcript);
        Ok(Self {
            ephemeral,
            token: code.token,
            fingerprint: code.fingerprint,
            request,
        })
    }

    pub fn request(&self) -> &JoinRequest {
        &self.request
    }

    /// Check the sponsor and open what it sealed.
    pub fn open(self, response: &JoinResponse) -> Result<SealedJoin> {
        anyhow::ensure!(
            response.protocol == PROTOCOL,
            "the sponsor speaks another join protocol"
        );
        anyhow::ensure!(
            fingerprint(&response.sponsor_key) == self.fingerprint,
            "the sponsor's key does not match the join code; this is not the machine that made it"
        );
        anyhow::ensure!(
            verify_signature(
                &response.sponsor_key,
                &response.signed_bytes(&self.request),
                &response.signature
            ),
            "the sponsor's answer is not signed by its key"
        );
        let sponsor_ephemeral = decode(&response.ephemeral)?;
        let transcript = self.request.transcript();
        let aad = aad(&transcript, &response.sponsor_key, &sponsor_ephemeral);
        let token = self.token;
        let key = agreement::agree_ephemeral(
            self.ephemeral,
            &UnparsedPublicKey::new(&X25519, &sponsor_ephemeral),
            |shared| seal_key(shared, &token, &aad),
        )
        .map_err(|_| anyhow::anyhow!("the sponsor's ephemeral key is invalid"))??;
        let nonce: [u8; 12] = decode(&response.nonce)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("the join nonce is invalid"))?;
        let mut sealed = decode(&response.sealed)?;
        let opened = key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut sealed,
            )
            .map_err(|_| anyhow::anyhow!("the sealed join answer does not open"))?;
        serde_json::from_slice(opened).context("decode the sealed join answer")
    }
}

/// The sponsor's checks on a request, given the stored token. Every failure has the same
/// public meaning; the variant is for the sponsor's own records.
#[derive(Debug, Eq, PartialEq)]
pub enum RequestFault {
    Protocol,
    Proof,
    Signature,
}

pub fn verify_request(request: &JoinRequest, token: &[u8]) -> Result<(), RequestFault> {
    if request.protocol != PROTOCOL {
        return Err(RequestFault::Protocol);
    }
    let transcript = request.transcript();
    let expected = decode(&request.proof).map_err(|_| RequestFault::Proof)?;
    proof(token, &transcript)
        .verify_slice(&expected)
        .map_err(|_| RequestFault::Proof)?;
    if !verify_signature(&request.member_key, &transcript, &request.signature) {
        return Err(RequestFault::Signature);
    }
    Ok(())
}

/// Seal `payload` for the joiner that sent `request` and sign the answer.
pub fn seal_response(
    request: &JoinRequest,
    token: &[u8],
    sponsor: &str,
    sponsor_key: &MemberKey,
    payload: &SealedJoin,
) -> Result<JoinResponse> {
    let random = SystemRandom::new();
    let ephemeral = EphemeralPrivateKey::generate(&X25519, &random)
        .map_err(|_| anyhow::anyhow!("generate an ephemeral key"))?;
    let public = ephemeral
        .compute_public_key()
        .map_err(|_| anyhow::anyhow!("compute an ephemeral key"))?;
    let joiner_ephemeral = decode(&request.ephemeral)?;
    let transcript = request.transcript();
    let aad = aad(&transcript, sponsor_key.public(), public.as_ref());
    let key = agreement::agree_ephemeral(
        ephemeral,
        &UnparsedPublicKey::new(&X25519, &joiner_ephemeral),
        |shared| seal_key(shared, token, &aad),
    )
    .map_err(|_| anyhow::anyhow!("the joiner's ephemeral key is invalid"))??;
    let mut nonce = [0_u8; 12];
    random
        .fill(&mut nonce)
        .map_err(|_| anyhow::anyhow!("generate a nonce"))?;
    let mut sealed = serde_json::to_vec(payload)?;
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(aad),
        &mut sealed,
    )
    .map_err(|_| anyhow::anyhow!("seal the join answer"))?;
    let mut response = JoinResponse {
        protocol: PROTOCOL.into(),
        sponsor: sponsor.into(),
        sponsor_key: sponsor_key.public().into(),
        ephemeral: encode(public.as_ref()),
        nonce: encode(&nonce),
        sealed: encode(&sealed),
        signature: String::new(),
    };
    response.signature = sponsor_key.sign(&response.signed_bytes(request));
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::code::CodeEndpoint;

    fn fixture() -> (JoinCode, MemberKey, MemberKey, SealedJoin) {
        let sponsor = MemberKey::generate().unwrap().0;
        let joiner = MemberKey::generate().unwrap().0;
        let code = JoinCode {
            fleet_id: uuid::Uuid::now_v7(),
            invite: [7; 16],
            token: [8; 16],
            fingerprint: fingerprint(sponsor.public()),
            expires_at: 0,
            name: Some("laptop".into()),
            migrate: false,
            endpoints: vec![CodeEndpoint::Loopback("127.0.0.1:1".into())],
        };
        let payload = SealedJoin {
            fleet_id: code.fleet_id.to_string(),
            secret: Some("ab".repeat(32)),
            anchor_key: sponsor.public().into(),
            sponsor: "studio".into(),
            writer_floor: Some(12),
            fabric_protocol: None,
            admitted_claim: Some("claim".into()),
        };
        (code, sponsor, joiner, payload)
    }

    #[test]
    fn the_handshake_delivers_the_secret_to_the_token_holder() {
        let (code, sponsor, joiner, payload) = fixture();
        let session = Joiner::start(&code, &joiner, "laptop", "dial-out", None, "test").unwrap();
        verify_request(session.request(), &code.token).unwrap();
        let response =
            seal_response(session.request(), &code.token, "studio", &sponsor, &payload).unwrap();
        // The answer never carries the secret in the clear.
        let wire = serde_json::to_string(&response).unwrap();
        assert!(!wire.contains(payload.secret.as_deref().unwrap()));
        assert_eq!(session.open(&response).unwrap(), payload);
    }

    #[test]
    fn a_wrong_token_or_a_foreign_signature_is_refused() {
        let (code, _, joiner, _) = fixture();
        let session = Joiner::start(&code, &joiner, "laptop", "dial-out", None, "test").unwrap();
        assert_eq!(
            verify_request(session.request(), &[9; 16]),
            Err(RequestFault::Proof)
        );
        let mut request = session.request().clone();
        request.member_key = MemberKey::generate().unwrap().0.public().into();
        request.proof = encode(
            &proof(&code.token, &request.transcript())
                .finalize()
                .into_bytes(),
        );
        assert_eq!(
            verify_request(&request, &code.token),
            Err(RequestFault::Signature)
        );
        let mut renamed = session.request().clone();
        renamed.name = "other".into();
        assert_eq!(
            verify_request(&renamed, &code.token),
            Err(RequestFault::Proof)
        );
    }

    #[test]
    fn a_replayed_request_gets_an_answer_the_replayer_cannot_open() {
        let (code, sponsor, joiner, payload) = fixture();
        let original = Joiner::start(&code, &joiner, "laptop", "dial-out", None, "test").unwrap();
        // The replayer has the request and the code, but not the original ephemeral key.
        let replayed = original.request().clone();
        let answer = seal_response(&replayed, &code.token, "studio", &sponsor, &payload).unwrap();
        let replayer = Joiner::start(&code, &joiner, "laptop", "dial-out", None, "test").unwrap();
        let mut forged = replayer;
        forged.request = replayed;
        assert!(forged.open(&answer).is_err());
        // The rightful joiner, still holding its ephemeral key, can open a fresh answer.
        let again = seal_response(
            original.request(),
            &code.token,
            "studio",
            &sponsor,
            &payload,
        )
        .unwrap();
        assert_eq!(original.open(&again).unwrap(), payload);
    }

    #[test]
    fn the_joiner_refuses_a_sponsor_key_that_does_not_match_the_fingerprint() {
        let (code, _, joiner, payload) = fixture();
        let impostor = MemberKey::generate().unwrap().0;
        let session = Joiner::start(&code, &joiner, "laptop", "dial-out", None, "test").unwrap();
        let answer = seal_response(
            session.request(),
            &code.token,
            "studio",
            &impostor,
            &payload,
        )
        .unwrap();
        let error = session.open(&answer).unwrap_err();
        assert!(error.to_string().contains("does not match"), "{error}");
    }

    #[test]
    fn a_tampered_answer_is_refused() {
        let (code, sponsor, joiner, payload) = fixture();
        let session = Joiner::start(&code, &joiner, "laptop", "dial-out", None, "test").unwrap();
        let mut answer =
            seal_response(session.request(), &code.token, "studio", &sponsor, &payload).unwrap();
        answer.sponsor = "someone-else".into();
        assert!(session.open(&answer).is_err());
    }
}
