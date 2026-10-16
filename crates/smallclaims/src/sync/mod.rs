//! Sync between members: the signed HTTP exchange two members run to swap the envelopes each
//! lacks, the worker that keeps every member's store in step with its peers, and joining over
//! the same listener.
//!
//! Every request and response carries the fleet's HMAC and, on a member, its member key's
//! signature; [`FleetAuth`] makes and checks both. The worker reaches its store through a
//! [`Backend`]: a program that holds the store itself uses [`Local`], and smalltalk hands the
//! calls to its daemon, the store's only writer.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use axum::http::{HeaderMap, HeaderValue};
use hmac::{Hmac, Mac as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::fleet::{MemberKey, Sender, verify_signature};

mod backend;
pub mod telemetry;
mod worker;

pub use backend::{Backend, Local, redemption_answer};
pub use worker::*;

const PROTOCOL: &str = "st3-replication-v1";
pub const EXCHANGE_PATH: &str = "/v1/peer/exchange";

/// A page of a checkpoint's manifest, for a node that adopts a checkpoint it did not take part
/// in. Older builds neither serve nor call it.
pub const CHECKPOINT_PATH: &str = "/v1/peer/checkpoint";
pub const JOIN_PATH: &str = "/v1/fleet/join";
pub const MAX_JOIN_BYTES: usize = 4096;
// LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-16-wire-fractal-accept-both — DELETE at contraction.
// Senders keep x-st3-* until every member accepts aliases; sunset 60 days after
// the first shipping release. Signature domains and canonical bytes do not change.
const HEADER_FLEET: &str = "x-st3-fleet";
const HEADER_NODE: &str = "x-st3-node";
const HEADER_BODY: &str = "x-st3-body-sha256";
const HEADER_SIGNATURE: &str = "x-st3-signature";
const HEADER_REQUEST: &str = "x-st3-request-digest";
const HEADER_MEMBER_KEY: &str = "x-st3-member-key";
const HEADER_MEMBER_SIGNATURE: &str = "x-st3-member-signature";
const HEADER_ALIASES: [(&str, &str); 7] = [
    (HEADER_FLEET, "x-fractal-fleet"),
    (HEADER_NODE, "x-fractal-node"),
    (HEADER_BODY, "x-fractal-body-sha256"),
    (HEADER_SIGNATURE, "x-fractal-signature"),
    (HEADER_REQUEST, "x-fractal-request-digest"),
    (HEADER_MEMBER_KEY, "x-fractal-member-key"),
    (HEADER_MEMBER_SIGNATURE, "x-fractal-member-signature"),
];
// LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-16-wire-fractal-accept-both
const MEMBER_SIGNATURE_DOMAIN: &str = "st3-member-v1";
pub const MAX_EXCHANGE_BYTES: usize = 64 * 1024 * 1024;

/// The largest whole manifest the worker hands the daemon to adopt. A manifest lists every
/// tombstone so far, about 400 bytes each.
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024 * 1024;

/// The HTTP content coding for large exchange bodies: zlib-wrapped deflate. A requester asks
/// for it with `Accept-Encoding`, and a peer says with the same header in its answer that it
/// takes it in requests. Signatures cover the uncompressed JSON, so an older build, which
/// neither asks nor says, exchanges plain JSON as before.
const EXCHANGE_ENCODING: &str = "deflate";

/// Bodies smaller than this go uncompressed: a quiet exchange is a few kilobytes, while a page
/// of envelopes is megabytes and deflates to about a third.
const DEFLATE_MIN_BYTES: usize = 64 * 1024;

fn deflate(body: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write as _;
    let mut encoder = flate2::write::ZlibEncoder::new(
        Vec::with_capacity(body.len() / 3),
        flate2::Compression::fast(),
    );
    encoder.write_all(body)?;
    Ok(encoder.finish()?)
}

/// Inflate an exchange body, refusing one that would expand past `MAX_EXCHANGE_BYTES`.
fn inflate(body: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut inflated = Vec::with_capacity(body.len() * 3);
    flate2::read::ZlibDecoder::new(body)
        .take(MAX_EXCHANGE_BYTES as u64 + 1)
        .read_to_end(&mut inflated)
        .context("inflate the exchange body")?;
    anyhow::ensure!(
        inflated.len() <= MAX_EXCHANGE_BYTES,
        "the exchange body inflates past {MAX_EXCHANGE_BYTES} bytes"
    );
    Ok(inflated)
}

fn deflated(headers: &HeaderMap) -> bool {
    headers.get("content-encoding").is_some_and(|value| {
        value
            .as_bytes()
            .eq_ignore_ascii_case(EXCHANGE_ENCODING.as_bytes())
    })
}

fn accepts_deflate(headers: &HeaderMap) -> bool {
    headers
        .get_all("accept-encoding")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|coding| {
            coding
                .split(';')
                .next()
                .is_some_and(|name| name.trim().eq_ignore_ascii_case(EXCHANGE_ENCODING))
        })
}

// LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-16-wire-fractal-accept-both — DELETE at contraction.
// Accept both spellings until contraction, 60 days after the first shipping release;
// switch senders only after every member accepts aliases.
fn signed_fields(headers: &HeaderMap) -> Result<[Option<&str>; 7]> {
    let mut fields = [None; 7];
    for ((old, new), field) in HEADER_ALIASES.into_iter().zip(&mut fields) {
        let mut values = headers.get_all(old).iter().chain(headers.get_all(new).iter());
        if let Some(first) = values.next() {
            anyhow::ensure!(
                values.all(|value| value == first),
                "the signed message has conflicting {old}/{new} headers"
            );
            *field = first.to_str().ok();
        }
    }
    Ok(fields)
}
// LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-16-wire-fractal-accept-both

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct FleetAuth {
    fleet_id: String,
    secret: Arc<Vec<u8>>,
    /// This member's key. Set, every request and response also carries a member signature.
    member: Option<Arc<MemberKey>>,
}

impl FleetAuth {
    pub fn load(fleet_id: &str, path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path)
            .with_context(|| format!("inspect the fleet secret {}", path.display()))?;
        anyhow::ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "the fleet secret must not grant group or other permissions"
        );
        let bytes =
            fs::read(path).with_context(|| format!("read the fleet secret {}", path.display()))?;
        let hexadecimal = std::str::from_utf8(&bytes)
            .ok()
            .map(str::trim)
            .filter(|value| {
                value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            });
        let secret = match hexadecimal {
            Some(value) => hex::decode(value).context("decode the hexadecimal fleet secret")?,
            None => bytes,
        };
        anyhow::ensure!(secret.len() == 32, "the fleet secret must contain 32 bytes");
        Ok(Self {
            fleet_id: fleet_id.into(),
            secret: Arc::new(secret),
            member: None,
        })
    }

    /// Sign every request and response with this member key as well.
    pub fn with_member_key(mut self, member: Option<Arc<MemberKey>>) -> Self {
        self.member = member;
        self
    }

    pub fn member_key(&self) -> Option<&str> {
        self.member.as_deref().map(MemberKey::public)
    }

    /// The fleet secret as the hexadecimal text a secret file holds.
    fn secret_hex(&self) -> String {
        hex::encode(self.secret.as_slice())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test(fleet_id: &str, secret: &[u8]) -> Self {
        Self {
            fleet_id: fleet_id.into(),
            secret: Arc::new(secret.to_vec()),
            member: None,
        }
    }

    fn canonical(
        &self,
        method: &str,
        path: &str,
        node: &str,
        body_digest: &str,
        request_digest: Option<&str>,
    ) -> String {
        format!(
            "{PROTOCOL}\n{method}\n{path}\n{}\n{node}\n{body_digest}\n{}",
            self.fleet_id,
            request_digest.unwrap_or_default()
        )
    }

    fn member_message(canonical: &str) -> Vec<u8> {
        format!("{MEMBER_SIGNATURE_DOMAIN}\n{canonical}").into_bytes()
    }

    // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-16-wire-fractal-accept-both — sender hold.
    // Keep old names until every member accepts aliases; sunset 60 days after first shipping release.
    // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-16-wire-fractal-accept-both
    fn add_member_signature(&self, headers: &mut HeaderMap, canonical: &str) -> Result<()> {
        if let Some(member) = &self.member {
            headers.insert(HEADER_MEMBER_KEY, HeaderValue::from_str(member.public())?);
            headers.insert(
                HEADER_MEMBER_SIGNATURE,
                HeaderValue::from_str(&member.sign(&Self::member_message(canonical)))?,
            );
        }
        Ok(())
    }

    pub fn fleet_id(&self) -> &str {
        &self.fleet_id
    }

    pub fn body_digest(body: &[u8]) -> String {
        hex::encode(Sha256::digest(body))
    }

    fn signature(
        &self,
        method: &str,
        path: &str,
        node: &str,
        body_digest: &str,
        request_digest: Option<&str>,
    ) -> String {
        let canonical = self.canonical(method, path, node, body_digest, request_digest);
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts a secret of any length");
        mac.update(canonical.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn request_headers(&self, node: &str, body: &[u8]) -> Result<HeaderMap> {
        self.request_headers_for(EXCHANGE_PATH, node, body)
    }

    pub fn request_headers_for(&self, path: &str, node: &str, body: &[u8]) -> Result<HeaderMap> {
        self.request_headers_method("POST", path, node, body)
    }

    pub fn request_headers_method(
        &self,
        method: &str,
        path: &str,
        node: &str,
        body: &[u8],
    ) -> Result<HeaderMap> {
        let digest = Self::body_digest(body);
        let signature = self.signature(method, path, node, &digest, None);
        let mut headers = headers(&self.fleet_id, node, &digest, &signature, None)?;
        self.add_member_signature(
            &mut headers,
            &self.canonical(method, path, node, &digest, None),
        )?;
        Ok(headers)
    }

    pub fn response_headers_for(
        &self,
        path: &str,
        node: &str,
        body: &[u8],
        request_digest: &str,
    ) -> Result<HeaderMap> {
        let digest = Self::body_digest(body);
        let signature = self.signature("RESPONSE", path, node, &digest, Some(request_digest));
        let mut headers = headers(
            &self.fleet_id,
            node,
            &digest,
            &signature,
            Some(request_digest),
        )?;
        self.add_member_signature(
            &mut headers,
            &self.canonical("RESPONSE", path, node, &digest, Some(request_digest)),
        )?;
        Ok(headers)
    }

    pub fn verify(
        &self,
        headers: &HeaderMap,
        method: &str,
        path: &str,
        body: &[u8],
        expected_node: Option<&str>,
        request_digest: Option<&str>,
    ) -> Result<String> {
        self.verify_sender(headers, method, path, body, expected_node, request_digest)
            .map(|sender| sender.name)
    }

    /// Check the fleet HMAC, then read the optional member key and check its signature.
    pub fn verify_sender(
        &self,
        headers: &HeaderMap,
        method: &str,
        path: &str,
        body: &[u8],
        expected_node: Option<&str>,
        request_digest: Option<&str>,
    ) -> Result<Sender> {
        fn field<'a>(value: Option<&'a str>, name: &str) -> Result<&'a str> {
            value.with_context(|| format!("the signed message has no valid {name} header"))
        }
        let [fleet, node, body_hash, signature, request, member_key, member_signature] =
            signed_fields(headers)?;
        anyhow::ensure!(
            field(fleet, HEADER_FLEET)? == self.fleet_id,
            "the fleet ID does not match"
        );
        let node = field(node, HEADER_NODE)?;
        if let Some(expected) = expected_node {
            anyhow::ensure!(
                node == expected,
                "the signed node does not match the configured peer"
            );
        }
        let digest = Self::body_digest(body);
        anyhow::ensure!(
            field(body_hash, HEADER_BODY)? == digest,
            "the signed body digest does not match"
        );
        if let Some(expected) = request_digest {
            anyhow::ensure!(
                field(request, HEADER_REQUEST)? == expected,
                "the response does not bind to this request"
            );
        }
        let signature = hex::decode(field(signature, HEADER_SIGNATURE)?)
            .context("the replication signature is not hexadecimal")?;
        let canonical = self.canonical(method, path, node, &digest, request_digest);
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts a secret of any length");
        mac.update(canonical.as_bytes());
        mac.verify_slice(&signature)
            .context("the replication signature does not match")?;
        let member_key = member_key.map(str::to_owned);
        let member_signature_valid = match (&member_key, member_signature) {
            (Some(key), Some(signature)) => {
                verify_signature(key, &Self::member_message(&canonical), signature)
            }
            _ => false,
        };
        Ok(Sender {
            name: node.into(),
            member_key,
            member_signature_valid,
        })
    }
}

// LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-16-wire-fractal-accept-both — sender hold.
// Keep old names until every member accepts aliases; sunset 60 days after first shipping release.
// LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-16-wire-fractal-accept-both
fn headers(
    fleet: &str,
    node: &str,
    digest: &str,
    signature: &str,
    request_digest: Option<&str>,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        (HEADER_FLEET, fleet),
        (HEADER_NODE, node),
        (HEADER_BODY, digest),
        (HEADER_SIGNATURE, signature),
    ] {
        headers.insert(name, HeaderValue::from_str(value)?);
    }
    if let Some(request_digest) = request_digest {
        headers.insert(HEADER_REQUEST, HeaderValue::from_str(request_digest)?);
    }
    Ok(headers)
}

/// The envelope of every signed peer answer. Its layout and version are the ones smalltalk's
/// client API answers with, so members on older builds read it unchanged.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PeerResponse<T> {
    pub api_version: String,
    pub request_id: String,
    pub snapshot_host: String,
    pub store_index: u64,
    pub value: T,
}

/// The version every signed peer answer carries.
pub const PEER_API_VERSION: &str = "st3.v1";

impl<T> PeerResponse<T> {
    pub fn new(node: &str, store_index: u64, value: T) -> Self {
        Self {
            api_version: PEER_API_VERSION.into(),
            request_id: uuid::Uuid::now_v7().to_string(),
            snapshot_host: node.into(),
            store_index,
            value,
        }
    }
}

#[cfg(test)]
mod header_tests {
    use super::*;

    fn signed_response() -> (FleetAuth, HeaderMap) {
        let member = Arc::new(MemberKey::generate().unwrap().0);
        let auth = FleetAuth::test("fleet", &[9; 32]).with_member_key(Some(member));
        let headers = auth
            .response_headers_for(EXCHANGE_PATH, "peer", b"body", "request")
            .unwrap();
        (auth, headers)
    }

    fn verify(auth: &FleetAuth, headers: &HeaderMap) -> Result<Sender> {
        auth.verify_sender(
            headers, "RESPONSE", EXCHANGE_PATH, b"body", Some("peer"), Some("request"),
        )
    }

    #[test]
    fn signed_headers_accept_old_new_mixed_and_equal_duplicates() {
        let (auth, old) = signed_response();
        assert_eq!(old.len(), HEADER_ALIASES.len());
        for (name, alias) in HEADER_ALIASES {
            assert!(old.contains_key(name), "sender must keep {name}");
            assert!(!old.contains_key(alias), "sender must not emit {alias}");
        }
        let mut new = HeaderMap::new();
        let mut mixed = HeaderMap::new();
        let mut both = old.clone();
        for (index, (name, alias)) in HEADER_ALIASES.into_iter().enumerate() {
            let value = old.get(name).unwrap();
            new.insert(alias, value.clone());
            mixed.insert(if index % 2 == 0 { name } else { alias }, value.clone());
            both.append(name, value.clone());
            both.append(alias, value.clone());
            both.append(alias, value.clone());
        }
        for headers in [&old, &new, &mixed, &both] {
            let sender = verify(&auth, headers).unwrap();
            assert_eq!(sender.name, "peer");
            assert_eq!(sender.member_key.as_deref(), auth.member_key());
            assert!(sender.member_signature_valid);
            assert!(auth.verify_sender(
                headers, "RESPONSE", EXCHANGE_PATH, b"tampered", Some("peer"), Some("request"),
            ).is_err());
            assert!(auth.verify_sender(
                headers, "RESPONSE", EXCHANGE_PATH, b"body", Some("peer"), Some("other"),
            ).is_err());
        }
    }

    #[test]
    fn signed_headers_reject_every_conflicting_alias_or_duplicate() {
        let (auth, old) = signed_response();
        for (name, alias) in HEADER_ALIASES {
            for duplicate_name in [name, alias] {
                let mut headers = old.clone();
                if duplicate_name == alias {
                    let value = headers.remove(name).unwrap();
                    headers.insert(alias, value);
                }
                headers.append(duplicate_name, HeaderValue::from_static("conflict"));
                let error = verify(&auth, &headers).unwrap_err().to_string();
                assert!(error.contains("conflicting"), "{name}: {error}");
            }
            let mut headers = old.clone();
            headers.insert(alias, HeaderValue::from_static("conflict"));
            let error = verify(&auth, &headers).unwrap_err().to_string();
            assert!(error.contains("conflicting"), "{name}: {error}");
        }
    }

    #[test]
    fn requests_reject_conflicting_unused_request_digest() {
        let (auth, _) = signed_response();
        let mut headers = auth.request_headers("peer", b"body").unwrap();
        headers.insert(HEADER_REQUEST, HeaderValue::from_static("one"));
        headers.insert("x-fractal-request-digest", HeaderValue::from_static("two"));
        let error = auth.verify(
            &headers, "POST", EXCHANGE_PATH, b"body", Some("peer"), None,
        ).unwrap_err().to_string();
        assert!(error.contains("conflicting"), "{error}");
    }
}
