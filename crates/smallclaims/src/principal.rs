//! Principals: subjects that hold keys, the delegations between their keys, and the signatures
//! that say which principal wrote a claim.
//!
//! A claim signature sits beside its claim, in the envelope payload, so claim IDs and every
//! existing hash stay as they were. It signs the claim's content (not its batch, which a device
//! cannot know when it signs), a nonce that makes it single use, and the chain of delegation
//! claims from the signing key to a trust root, so the chain cannot be swapped. A delegation is
//! itself a signed claim of kind [`KEY_GRANTED`] on the principal's subject. Trust roots are node
//! keys: the member keys fleet membership admits, or a standalone node's own key.
//!
//! [`judge`] decides a claim's verdict from facts the store looks up; the store folds it over
//! every claim in canonical order and caches the result (see `store::principals`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fleet::{MemberKey, verify_signature};
use crate::hash::{canonical_hash, canonical_json_value};

/// A key delegated to a principal, written on the principal's subject and signed by its issuer.
pub const KEY_GRANTED: &str = "principal.key-granted";
/// A delegated key withdrawn, written on the principal's subject.
pub const KEY_REVOKED: &str = "principal.key-revoked";

const CLAIM_SIGNATURE_DOMAIN: &str = "smallclaims-claim-v1";
const CONTENT_DOMAIN: &str = "smallclaims.claim-content.v1";

/// Which principal a key delegation makes the key speak for, and in what role.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// A person's own key, from which their device keys descend. A node vouches for it.
    Root,
    /// One device of a person: a CLI, stui, a phone, a browser. Issued by the person.
    Device,
    /// An agent's key, minted by the node that runs it.
    Agent,
    /// A plugin's key, issued by a node.
    Plugin,
}

/// The principal families smallclaims knows, by subject prefix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Family {
    /// `host/NAME`: a member machine. Its keys are trust roots.
    Node,
    /// `person/NAME`: keys on the person's devices.
    Person,
    /// `agent/...`: keys held by the node that runs the agent.
    Agent,
    /// `plugin/NAME`: keys held by the plugin.
    Plugin,
}

impl Family {
    pub fn of(subject: &str) -> Option<Self> {
        let (prefix, rest) = subject.split_once('/')?;
        if rest.is_empty() {
            return None;
        }
        match prefix {
            "host" => Some(Self::Node),
            "person" => Some(Self::Person),
            "agent" => Some(Self::Agent),
            "plugin" => Some(Self::Plugin),
            _ => None,
        }
    }

    /// Whether a delegation in `role` to a principal of this family may be issued by a
    /// principal of `issuer`.
    pub fn accepts(self, role: Role, issuer: Family, same_principal: bool) -> bool {
        match (self, role) {
            (Self::Person, Role::Root) => issuer == Self::Node,
            (Self::Person, Role::Device) => issuer == Self::Person && same_principal,
            (Self::Agent, Role::Agent) => issuer == Self::Node,
            (Self::Plugin, Role::Plugin) => issuer == Self::Node,
            _ => false,
        }
    }

    /// Whether a key in `role` may sign claims for a principal of this family. A person's root
    /// key only delegates; their devices sign.
    pub fn signs_with(self, role: Role) -> bool {
        matches!(
            (self, role),
            (Self::Person, Role::Device)
                | (Self::Agent, Role::Agent)
                | (Self::Plugin, Role::Plugin)
        )
    }
}

/// The fields of a [`KEY_GRANTED`] claim.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct KeyGrant {
    pub key: String,
    pub role: Role,
    pub issuer: String,
    pub issuer_key: String,
    /// What the key is for a person to recognise, such as `studio CLI` or `phone`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl KeyGrant {
    pub fn from_fields(fields: &Value) -> Option<Self> {
        serde_json::from_value(fields.clone()).ok()
    }

    pub fn fields(&self) -> BTreeMap<String, Value> {
        serde_json::from_value(serde_json::to_value(self).expect("a key grant serializes"))
            .expect("a key grant is an object")
    }
}

/// A claim's signature, carried beside the claim.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClaimSignature {
    pub signer: String,
    /// The principal the signer acts for, when it is not the signer itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf: Option<String>,
    pub key: String,
    /// Delegation claim IDs from `key` to a trust root: the grant of `key` first. Empty when
    /// `key` is a node key.
    #[serde(default)]
    pub chain: Vec<String>,
    pub nonce: String,
    pub signed_at_unix_ms: u64,
    pub signature: String,
}

/// The digest of what a signature covers in a claim: everything its writer chose, and nothing
/// the store adds when it batches the claim.
pub fn content_digest(subject: &str, kind: &str, actor: Option<&str>, body: &Value) -> String {
    canonical_hash(&(
        CONTENT_DOMAIN,
        subject,
        kind,
        actor,
        canonical_json_value(body),
    ))
    .expect("claim content encodes")
}

fn signing_bytes(
    content: &str,
    signer: &str,
    on_behalf: Option<&str>,
    key: &str,
    chain: &[String],
    nonce: &str,
    signed_at_unix_ms: u64,
) -> Vec<u8> {
    format!(
        "{CLAIM_SIGNATURE_DOMAIN}\n{content}\n{signer}\n{}\n{key}\n{}\n{nonce}\n{signed_at_unix_ms}",
        on_behalf.unwrap_or_default(),
        chain.join(",")
    )
    .into_bytes()
}

fn fresh_nonce() -> String {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).expect("the system has randomness");
    hex::encode(bytes)
}

impl ClaimSignature {
    /// Sign claim content as `signer` with `key`.
    pub fn sign(
        key: &MemberKey,
        content: &str,
        signer: &str,
        on_behalf: Option<&str>,
        chain: Vec<String>,
        signed_at_unix_ms: u64,
    ) -> Self {
        let nonce = fresh_nonce();
        let signature = key.sign(&signing_bytes(
            content,
            signer,
            on_behalf,
            key.public(),
            &chain,
            &nonce,
            signed_at_unix_ms,
        ));
        Self {
            signer: signer.into(),
            on_behalf: on_behalf.map(str::to_owned),
            key: key.public().into(),
            chain,
            nonce,
            signed_at_unix_ms,
            signature,
        }
    }

    /// Whether the signature is genuine for this content: the key signed exactly these fields.
    pub fn verifies(&self, content: &str) -> bool {
        verify_signature(
            &self.key,
            &signing_bytes(
                content,
                &self.signer,
                self.on_behalf.as_deref(),
                &self.key,
                &self.chain,
                &self.nonce,
                self.signed_at_unix_ms,
            ),
            &self.signature,
        )
    }
}

/// What admission concluded about a claim's signature.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "verdict", content = "reason", rename_all = "kebab-case")]
pub enum Verdict {
    /// The signature checks and its chain reaches a trust root.
    Verified,
    /// The claim carries no signature.
    Unsigned,
    /// A delegation in the chain has not arrived. Judged again when it does.
    Held(String),
    /// The signature or its chain is not acceptable, for the reason given.
    Invalid(String),
}

impl Verdict {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Unsigned => "unsigned",
            Self::Held(_) => "held",
            Self::Invalid(_) => "invalid",
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Held(reason) | Self::Invalid(reason) => Some(reason),
            _ => None,
        }
    }
}

/// A claim as admission judges it.
#[derive(Clone, Debug)]
pub struct Judged<'a> {
    pub id: &'a str,
    pub subject: &'a str,
    pub kind: &'a str,
    pub content: String,
    pub fields: &'a Value,
}

/// A delegation the chain names, with its own signature and verdict.
#[derive(Clone, Debug)]
pub struct Delegation {
    pub subject: String,
    pub grant: KeyGrant,
    pub verdict: Verdict,
}

/// What [`judge`] asks the store. Every answer about order is in the canonical order of claims,
/// never arrival.
pub trait Facts {
    /// A delegation claim by ID, when it has been admitted, and its verdict.
    fn delegation(&self, claim_id: &str) -> Option<Delegation>;
    /// Whether `key` is a trust root for node `subject`.
    fn is_root(&self, subject: &str, key: &str) -> bool;
    /// Whether `key` of `principal` was revoked before the judged claim.
    fn revoked_before(&self, principal: &str, key: &str) -> bool;
    /// Whether an earlier claim already carried this key and nonce.
    fn nonce_used_before(&self, key: &str, nonce: &str) -> bool;
}

/// Decide a claim's verdict. `signature` is the one carried beside it, if any.
pub fn judge(claim: &Judged<'_>, signature: Option<&ClaimSignature>, facts: &dyn Facts) -> Verdict {
    let Some(signature) = signature else {
        return Verdict::Unsigned;
    };
    if !signature.verifies(&claim.content) {
        return Verdict::Invalid("the signature does not match the claim".into());
    }
    if facts.nonce_used_before(&signature.key, &signature.nonce) {
        return Verdict::Invalid("the signature was used by an earlier claim".into());
    }
    if let Some(on_behalf) = &signature.on_behalf
        && on_behalf == &signature.signer
    {
        return Verdict::Invalid("a signer cannot act on its own behalf".into());
    }
    let Some(family) = Family::of(&signature.signer) else {
        return Verdict::Invalid(format!("`{}` is not a principal", signature.signer));
    };
    // A grant claim speaks for its issuer.
    if claim.kind == KEY_GRANTED {
        match KeyGrant::from_fields(claim.fields) {
            Some(grant)
                if grant.issuer == signature.signer && grant.issuer_key == signature.key =>
            {
                let Some(subject_family) = Family::of(claim.subject) else {
                    return Verdict::Invalid(format!("`{}` cannot hold keys", claim.subject));
                };
                if !subject_family.accepts(grant.role, family, claim.subject == grant.issuer) {
                    return Verdict::Invalid(format!(
                        "a {family:?} cannot grant a {:?} key to `{}`",
                        grant.role, claim.subject
                    ));
                }
            }
            Some(_) => {
                return Verdict::Invalid("a key grant must be signed by its issuer's key".into());
            }
            None => return Verdict::Invalid("the key grant's fields are malformed".into()),
        }
    }
    if family == Family::Node {
        if !signature.chain.is_empty() {
            return Verdict::Invalid("a node key needs no chain".into());
        }
        // Membership may admit the node later; its claims wait for it.
        return if facts.is_root(&signature.signer, &signature.key) {
            Verdict::Verified
        } else {
            Verdict::Held(format!(
                "fleet membership admits no such key for `{}`",
                signature.signer
            ))
        };
    }
    judge_chain(claim, signature, family, facts)
}

fn judge_chain(
    claim: &Judged<'_>,
    signature: &ClaimSignature,
    family: Family,
    facts: &dyn Facts,
) -> Verdict {
    if signature.chain.is_empty() {
        return Verdict::Invalid("the signature names no delegation for its key".into());
    }
    // Walk the chain from the signing key up: each delegation grants the key the step below
    // signed with, and is signed by the key the next delegation grants. Every key on the way
    // must still stand at this claim: revoking a person's root key cuts every device below it.
    let mut principal = signature.signer.clone();
    let mut key = signature.key.clone();
    for (index, id) in signature.chain.iter().enumerate() {
        let Some(delegation) = facts.delegation(id) else {
            return Verdict::Held(format!("delegation {id} has not arrived"));
        };
        if delegation.subject != principal || delegation.grant.key != key {
            return Verdict::Invalid(format!(
                "delegation {id} does not grant the key the chain needs"
            ));
        }
        if index == 0 {
            // A grant claim is signed by its issuer, whose key may be a root key, which only
            // delegates. Every other claim needs a signing role.
            if claim.kind != KEY_GRANTED && !family.signs_with(delegation.grant.role) {
                return Verdict::Invalid(format!(
                    "a {:?} key does not sign claims",
                    delegation.grant.role
                ));
            }
        } else if !matches!(delegation.grant.role, Role::Root) {
            return Verdict::Invalid(format!("delegation {id} cannot issue keys"));
        }
        if facts.revoked_before(&principal, &key) {
            return Verdict::Invalid(format!("the key of `{principal}` in the chain was revoked"));
        }
        match &delegation.verdict {
            Verdict::Verified => {}
            Verdict::Held(reason) => return Verdict::Held(reason.clone()),
            Verdict::Unsigned => return Verdict::Invalid(format!("delegation {id} is unsigned")),
            Verdict::Invalid(reason) => {
                return Verdict::Invalid(format!("delegation {id} is invalid: {reason}"));
            }
        }
        if Family::of(&delegation.grant.issuer) == Some(Family::Node) {
            // The delegation's own verdict checked the node key.
            return if index + 1 == signature.chain.len() {
                Verdict::Verified
            } else {
                Verdict::Invalid("the chain goes on past a node".into())
            };
        }
        principal = delegation.grant.issuer.clone();
        key = delegation.grant.issuer_key.clone();
    }
    Verdict::Invalid("the chain does not reach a node".into())
}

/// A key this node holds for a principal, and the chain that delegates it.
#[derive(Clone, Debug)]
pub struct HeldKey {
    pub principal: String,
    pub role: Role,
    pub key: Arc<MemberKey>,
    pub chain: Vec<String>,
}

/// The keys this node signs with: its node key, and the keys it holds for people and agents.
/// Each private key is a `0600` file in the key directory, named by its public key; which
/// principal holds it and its chain are rows of the store's local `held_keys` table.
#[derive(Debug, Default)]
pub struct Keyring {
    directory: RwLock<Option<PathBuf>>,
    node: RwLock<Option<Arc<MemberKey>>>,
    /// By public key.
    keys: RwLock<BTreeMap<String, HeldKey>>,
    /// The public key each principal signs claims with.
    signing: RwLock<BTreeMap<String, String>>,
}

impl Keyring {
    pub fn set_directory(&self, directory: &Path) {
        *self
            .directory
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(directory.to_path_buf());
    }

    pub fn directory(&self) -> Option<PathBuf> {
        self.directory
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn set_node(&self, key: Option<Arc<MemberKey>>) {
        *self.node.write().unwrap_or_else(PoisonError::into_inner) = key;
    }

    pub fn node(&self) -> Option<Arc<MemberKey>> {
        self.node
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The key `principal` signs claims with, when this node holds one.
    pub fn signing(&self, principal: &str) -> Option<HeldKey> {
        let public = self
            .signing
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(principal)
            .cloned()?;
        self.by_public(&public)
    }

    pub fn by_public(&self, public: &str) -> Option<HeldKey> {
        self.keys
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(public)
            .cloned()
    }

    /// Hold `held`; a key in a signing role becomes its principal's signing key.
    pub fn insert(&self, held: HeldKey) {
        let public = held.key.public().to_owned();
        if !matches!(held.role, Role::Root) {
            self.signing
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(held.principal.clone(), public.clone());
        }
        self.keys
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(public, held);
    }

    fn key_path(directory: &Path, public: &str) -> PathBuf {
        let digest =
            canonical_hash(&("smallclaims.held-key.v1", public)).expect("a public key encodes");
        directory.join(format!("{}.key", &digest[..32]))
    }

    /// A new key, written to the key directory when there is one.
    pub fn create(&self) -> Result<Arc<MemberKey>> {
        let (key, document) = MemberKey::generate()?;
        if let Some(directory) = self.directory() {
            crate::fleet::join::write_private(&Self::key_path(&directory, key.public()), &document)
                .context("store a principal key")?;
        }
        Ok(Arc::new(key))
    }

    /// The private key for `public` from the key directory, if it is there.
    pub fn load(&self, public: &str) -> Result<Option<Arc<MemberKey>>> {
        let Some(directory) = self.directory() else {
            return Ok(None);
        };
        let path = Self::key_path(&directory, public);
        if !path.exists() {
            return Ok(None);
        }
        let key = MemberKey::load(&path)?;
        anyhow::ensure!(
            key.public() == public,
            "{} holds another key than its name says",
            path.display()
        );
        Ok(Some(Arc::new(key)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Knows one node key, for `host/studio`, and nothing else.
    struct NoFacts(String);

    impl Facts for NoFacts {
        fn delegation(&self, _: &str) -> Option<Delegation> {
            None
        }
        fn is_root(&self, subject: &str, key: &str) -> bool {
            subject == "host/studio" && key == self.0
        }
        fn revoked_before(&self, _: &str, _: &str) -> bool {
            false
        }
        fn nonce_used_before(&self, _: &str, _: &str) -> bool {
            false
        }
    }

    fn claim<'a>(fields: &'a Value) -> Judged<'a> {
        Judged {
            id: "claim/1",
            subject: "note/plans",
            kind: "example.note",
            content: content_digest("note/plans", "example.note", Some("person/ada"), fields),
            fields,
        }
    }

    #[test]
    fn a_node_signature_verifies_and_any_change_to_the_claim_breaks_it() {
        let (node, _) = MemberKey::generate().unwrap();
        let fields = serde_json::json!({"fields": {"text": "hello"}});
        let judged = claim(&fields);
        let facts = NoFacts(node.public().into());
        let signature = ClaimSignature::sign(
            &node,
            &judged.content,
            "host/studio",
            Some("person/ada"),
            vec![],
            1,
        );
        assert_eq!(judge(&judged, Some(&signature), &facts), Verdict::Verified);
        assert_eq!(judge(&judged, None, &facts), Verdict::Unsigned);

        let other = serde_json::json!({"fields": {"text": "goodbye"}});
        assert!(matches!(
            judge(&claim(&other), Some(&signature), &facts),
            Verdict::Invalid(_)
        ));

        let mut swapped = signature.clone();
        swapped.on_behalf = Some("person/grace".into());
        assert!(matches!(
            judge(&judged, Some(&swapped), &facts),
            Verdict::Invalid(_)
        ));
        let mut chained = signature.clone();
        chained.chain = vec!["claim/other".into()];
        assert!(matches!(
            judge(&judged, Some(&chained), &facts),
            Verdict::Invalid(_)
        ));
        let mut stranger = signature;
        stranger.signer = "host/elsewhere".into();
        assert!(matches!(
            judge(&judged, Some(&stranger), &facts),
            Verdict::Invalid(_)
        ));
        let (unknown, _) = MemberKey::generate().unwrap();
        let unadmitted =
            ClaimSignature::sign(&unknown, &judged.content, "host/studio", None, vec![], 1);
        assert!(matches!(
            judge(&judged, Some(&unadmitted), &facts),
            Verdict::Held(_)
        ));
    }

    #[test]
    fn grant_roles_follow_the_families() {
        assert!(Family::Person.accepts(Role::Root, Family::Node, false));
        assert!(Family::Person.accepts(Role::Device, Family::Person, true));
        assert!(!Family::Person.accepts(Role::Device, Family::Person, false));
        assert!(!Family::Person.accepts(Role::Root, Family::Person, true));
        assert!(Family::Agent.accepts(Role::Agent, Family::Node, false));
        assert!(!Family::Agent.accepts(Role::Agent, Family::Person, false));
        assert!(!Family::Person.signs_with(Role::Root));
        assert!(Family::Person.signs_with(Role::Device));
        assert_eq!(Family::of("person/ada"), Some(Family::Person));
        assert_eq!(Family::of("person/"), None);
        assert_eq!(Family::of("message/1"), None);
    }
}
