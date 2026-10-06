//! Claim signatures in the store: signing this node's claims as their batches are sealed,
//! minting the keys it holds for people and agents, and the verdict each claim's signature gets.
//!
//! Verdicts are a fold over the claims in canonical order (see [`crate::principal::judge`]). The
//! store keeps it current incrementally and caches it in local tables that are derived only from
//! admitted claims, never synced, and outside every digest:
//!
//! - `claim_verdicts`: one row per signed claim, keyed by the claim ID (a content hash), so a
//!   different claim never reuses a verdict.
//! - `claim_verdict_links`: what each verdict relied on (delegations, keys, its nonce, the trust
//!   roots). A claim that changes one of those queues every verdict linked to it.
//! - `claim_verdict_queue`: claims to judge again.
//! - `claim_verdict_fresh`: claims whose signature was just stored, at sealing or admission; a
//!   pass judges them and queues whatever they change. Nothing scans the claims.
//!
//! An unsigned claim has no row: its verdict is `unsigned`. [`Store::recheck_claim_verdicts`]
//! recomputes every verdict from the claims and reports any cached one that differed.

use std::cell::RefCell;
use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, BTreeSet};

use rusqlite::types::Value as SqlValue;

use super::*;
use crate::principal::{
    ClaimSignature, Delegation, Facts, Family, HeldKey, KEY_GRANTED, KEY_REVOKED, KeyGrant, Role,
    Verdict, content_digest, judge,
};

/// The tables of this module, created with the store's schema.
pub const PRINCIPAL_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS claim_signatures (
    claim_id TEXT PRIMARY KEY,
    signer TEXT NOT NULL,
    key TEXT NOT NULL,
    nonce TEXT NOT NULL,
    signature TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS claim_signatures_nonce_index ON claim_signatures(key, nonce);
CREATE TABLE IF NOT EXISTS claim_verdicts (
    claim_id TEXT PRIMARY KEY,
    verdict TEXT NOT NULL,
    reason TEXT,
    signer TEXT NOT NULL,
    on_behalf TEXT
);
CREATE INDEX IF NOT EXISTS claim_verdicts_verdict_index ON claim_verdicts(verdict);
CREATE TABLE IF NOT EXISTS claim_verdict_links (
    link TEXT NOT NULL,
    claim_id TEXT NOT NULL,
    PRIMARY KEY(link, claim_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS claim_verdict_links_claim_index ON claim_verdict_links(claim_id);
CREATE TABLE IF NOT EXISTS claim_verdict_queue (
    claim_id TEXT PRIMARY KEY
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS claim_verdict_fresh (
    claim_id TEXT PRIMARY KEY
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS expected_claim_signatures (
    subject TEXT NOT NULL,
    kind TEXT NOT NULL,
    actor TEXT NOT NULL,
    signature TEXT NOT NULL,
    PRIMARY KEY(subject, kind, actor)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS held_keys (
    public_key TEXT PRIMARY KEY,
    principal TEXT NOT NULL,
    role TEXT NOT NULL,
    chain TEXT NOT NULL
);
"#;

/// Queued claims judged per writer loan, so queued writes run between chunks.
const JUDGE_CHUNK: usize = 256;
/// The longest chain admission follows: a device, its person's root, and the node that vouched.
const MAX_CHAIN_DEPTH: usize = 8;
const VERDICT_ROOTS: &str = "claim_verdict_roots";
const OWN_NODE_KEY: &str = "principal_node_key";

/// A claim's position in the canonical order, as comparable values.
#[derive(Clone, Debug)]
struct Position(Vec<SqlValue>);

impl Position {
    fn compare(&self, other: &Self) -> CmpOrdering {
        for (left, right) in self.0.iter().zip(&other.0) {
            let ordering = match (left, right) {
                (SqlValue::Integer(left), SqlValue::Integer(right)) => left.cmp(right),
                (SqlValue::Text(left), SqlValue::Text(right)) => left.cmp(right),
                (SqlValue::Null, SqlValue::Null) => CmpOrdering::Equal,
                (SqlValue::Null, _) => CmpOrdering::Less,
                (_, SqlValue::Null) => CmpOrdering::Greater,
                (SqlValue::Integer(_), _) => CmpOrdering::Less,
                (_, SqlValue::Integer(_)) => CmpOrdering::Greater,
                _ => CmpOrdering::Equal,
            };
            if ordering != CmpOrdering::Equal {
                return ordering;
            }
        }
        CmpOrdering::Equal
    }
}

fn position(connection: &Connection, claim_id: &str) -> Result<Option<Position>> {
    let components = super::canonical::components("claims");
    let count = components.len();
    let sql = format!("SELECT {} FROM claims WHERE id=?1", components.join(", "));
    Ok(connection
        .prepare_cached(&sql)?
        .query_row([claim_id], |row| {
            (0..count)
                .map(|index| row.get::<_, SqlValue>(index))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .optional()?
        .map(Position))
}

fn stored_signature(connection: &Connection, claim_id: &str) -> Result<Option<ClaimSignature>> {
    let text = connection
        .prepare_cached("SELECT signature FROM claim_signatures WHERE claim_id=?1")?
        .query_row([claim_id], |row| row.get::<_, String>(0))
        .optional()?;
    Ok(text.and_then(|text| serde_json::from_str(&text).ok()))
}

/// Keep `signature` for `claim_id`, and mark the claim for the next verdict pass. A signature is
/// never replaced: the first one stored is the one the claim's envelope carried.
pub fn store_claim_signature_tx(
    connection: &Connection,
    claim_id: &str,
    signature: &ClaimSignature,
) -> Result<()> {
    let stored = connection
        .prepare_cached(
            "INSERT OR IGNORE INTO claim_signatures(claim_id, signer, key, nonce, signature)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?
        .execute(params![
            claim_id,
            signature.signer,
            signature.key,
            signature.nonce,
            serde_json::to_string(signature)?
        ])?;
    if stored != 0 {
        connection
            .prepare_cached("INSERT OR IGNORE INTO claim_verdict_fresh(claim_id) VALUES (?1)")?
            .execute([claim_id])?;
    }
    Ok(())
}

/// Keep a signature a device made for the claim this node is about to write on `subject`, so the
/// write stores it in its own transaction.
pub fn attach_expected_signature_tx(
    transaction: &Transaction<'_>,
    claim_id: &str,
    subject: &str,
    kind: &str,
    actor: Option<&str>,
) -> Result<()> {
    let Some(actor) = actor else {
        return Ok(());
    };
    let expected = transaction
        .prepare_cached(
            "SELECT signature FROM expected_claim_signatures
             WHERE subject=?1 AND kind=?2 AND actor=?3",
        )?
        .query_row(params![subject, kind, actor], |row| row.get::<_, String>(0))
        .optional()?;
    if let Some(text) = expected {
        transaction
            .prepare_cached(
                "DELETE FROM expected_claim_signatures WHERE subject=?1 AND kind=?2 AND actor=?3",
            )?
            .execute(params![subject, kind, actor])?;
        if let Ok(signature) = serde_json::from_str::<ClaimSignature>(&text) {
            store_claim_signature_tx(transaction, claim_id, &signature)?;
        }
    }
    Ok(())
}

struct ClaimRow {
    id: String,
    subject: String,
    kind: String,
    actor: Option<String>,
    body: Value,
}

fn claim_row(connection: &Connection, claim_id: &str) -> Result<Option<ClaimRow>> {
    Ok(connection
        .prepare_cached("SELECT id, subject, kind, actor, body FROM claims WHERE id=?1")?
        .query_row([claim_id], |row| {
            Ok(ClaimRow {
                id: row.get(0)?,
                subject: row.get(1)?,
                kind: row.get(2)?,
                actor: row.get(3)?,
                body: serde_json::from_str(&row.get::<_, String>(4)?).unwrap_or(Value::Null),
            })
        })
        .optional()?)
}

/// The trust roots: `(host/NAME, key)` for every member incarnation fleet membership admits, and
/// this node's own key.
fn roots(connection: &Connection, origin: &str) -> Result<BTreeSet<(String, String)>> {
    let mut roots = fleet_membership_tx(connection)?
        .incarnations()
        .map(|incarnation| {
            (
                format!("host/{}", incarnation.name),
                incarnation.member_key.clone(),
            )
        })
        .collect::<BTreeSet<_>>();
    if let Some(own) = fleet_meta(connection, OWN_NODE_KEY)? {
        roots.insert((format!("host/{origin}"), own));
    }
    Ok(roots)
}

/// Answers for one claim being judged: every question about order is about claims before it.
struct StoreFacts<'a> {
    connection: &'a Connection,
    roots: &'a BTreeSet<(String, String)>,
    position: Position,
    claim_id: String,
    depth: usize,
    /// Verdicts worked out during this pass for claims not yet cached.
    memo: &'a RefCell<BTreeMap<String, Verdict>>,
    /// What the verdict relied on.
    links: RefCell<BTreeSet<String>>,
}

impl StoreFacts<'_> {
    fn before(&self, other: &str) -> bool {
        position(self.connection, other)
            .ok()
            .flatten()
            .is_some_and(|other| other.compare(&self.position) == CmpOrdering::Less)
    }

    fn verdict_of(&self, claim_id: &str) -> Verdict {
        if let Some(verdict) = self.memo.borrow().get(claim_id) {
            return verdict.clone();
        }
        if let Ok(Some(verdict)) = cached_verdict(self.connection, claim_id) {
            return verdict;
        }
        if self.depth >= MAX_CHAIN_DEPTH {
            return Verdict::Invalid("the chain is too deep or loops".into());
        }
        let verdict = judge_claim(
            self.connection,
            self.roots,
            claim_id,
            self.depth + 1,
            self.memo,
        )
        .map(|(verdict, _)| verdict)
        .unwrap_or_else(|error| Verdict::Invalid(format!("could not judge: {error:#}")));
        self.memo
            .borrow_mut()
            .insert(claim_id.to_owned(), verdict.clone());
        verdict
    }
}

impl Facts for StoreFacts<'_> {
    fn delegation(&self, claim_id: &str) -> Option<Delegation> {
        self.links
            .borrow_mut()
            .insert(format!("delegation:{claim_id}"));
        let row = claim_row(self.connection, claim_id).ok().flatten()?;
        if row.kind != KEY_GRANTED {
            return None;
        }
        let grant = KeyGrant::from_fields(row.body.get("fields")?)?;
        self.links
            .borrow_mut()
            .insert(format!("key:{}|{}", row.subject, grant.key));
        Some(Delegation {
            subject: row.subject,
            grant,
            verdict: self.verdict_of(claim_id),
        })
    }

    fn is_root(&self, subject: &str, key: &str) -> bool {
        self.links.borrow_mut().insert("root".into());
        self.roots.contains(&(subject.to_owned(), key.to_owned()))
    }

    fn revoked_before(&self, principal: &str, key: &str) -> bool {
        self.links
            .borrow_mut()
            .insert(format!("key:{principal}|{key}"));
        let revocations = self
            .connection
            .prepare_cached(
                "SELECT claims.id FROM claims JOIN claim_signatures ON claim_signatures.claim_id=claims.id
                 WHERE claims.subject=?1 AND claims.kind=?2
                   AND json_extract(claims.body, '$.fields.key')=?3",
            )
            .and_then(|mut statement| {
                statement
                    .query_map(params![principal, KEY_REVOKED, key], |row| {
                        row.get::<_, String>(0)
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap_or_default();
        revocations.into_iter().any(|revocation| {
            revocation != self.claim_id
                && self.before(&revocation)
                && self.verdict_of(&revocation) == Verdict::Verified
                && stored_signature(self.connection, &revocation)
                    .ok()
                    .flatten()
                    .is_some_and(|signature| {
                        signature.signer == principal
                            || Family::of(&signature.signer) == Some(Family::Node)
                    })
        })
    }

    fn nonce_used_before(&self, key: &str, nonce: &str) -> bool {
        self.links
            .borrow_mut()
            .insert(format!("nonce:{key}|{nonce}"));
        let others = self
            .connection
            .prepare_cached(
                "SELECT claim_id FROM claim_signatures WHERE key=?1 AND nonce=?2 AND claim_id<>?3",
            )
            .and_then(|mut statement| {
                statement
                    .query_map(params![key, nonce, self.claim_id], |row| {
                        row.get::<_, String>(0)
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap_or_default();
        others.iter().any(|other| self.before(other))
    }
}

fn cached_verdict(connection: &Connection, claim_id: &str) -> Result<Option<Verdict>> {
    Ok(connection
        .prepare_cached("SELECT verdict, reason FROM claim_verdicts WHERE claim_id=?1")?
        .query_row([claim_id], |row| {
            Ok(verdict_from(&row.get::<_, String>(0)?, row.get(1)?))
        })
        .optional()?)
}

fn verdict_from(name: &str, reason: Option<String>) -> Verdict {
    let reason = reason.unwrap_or_default();
    match name {
        "verified" => Verdict::Verified,
        "unsigned" => Verdict::Unsigned,
        "held" => Verdict::Held(reason),
        _ => Verdict::Invalid(reason),
    }
}

/// Judge one claim from the claims as they stand, with what it relied on.
fn judge_claim(
    connection: &Connection,
    roots: &BTreeSet<(String, String)>,
    claim_id: &str,
    depth: usize,
    memo: &RefCell<BTreeMap<String, Verdict>>,
) -> Result<(Verdict, BTreeSet<String>)> {
    let Some(row) = claim_row(connection, claim_id)? else {
        return Ok((
            Verdict::Held(format!("claim {claim_id} is not here")),
            BTreeSet::new(),
        ));
    };
    let signature = stored_signature(connection, claim_id)?;
    let Some(position) = position(connection, claim_id)? else {
        return Ok((
            Verdict::Held(format!("claim {claim_id} is not here")),
            BTreeSet::new(),
        ));
    };
    let facts = StoreFacts {
        connection,
        roots,
        position,
        claim_id: row.id.clone(),
        depth,
        memo,
        links: RefCell::default(),
    };
    let fields = row.body.get("fields").cloned().unwrap_or(Value::Null);
    let judged = crate::principal::Judged {
        id: &row.id,
        subject: &row.subject,
        kind: &row.kind,
        actor: row.actor.as_deref(),
        content: content_digest(&row.subject, &row.kind, row.actor.as_deref(), &row.body),
        fields: &fields,
    };
    let verdict = judge(&judged, signature.as_ref(), &facts);
    Ok((verdict, facts.links.into_inner()))
}

fn write_verdict_tx(
    connection: &Connection,
    claim_id: &str,
    signature: &ClaimSignature,
    verdict: &Verdict,
    links: &BTreeSet<String>,
) -> Result<bool> {
    let previous = cached_verdict(connection, claim_id)?;
    connection
        .prepare_cached("DELETE FROM claim_verdict_links WHERE claim_id=?1")?
        .execute([claim_id])?;
    for link in links {
        connection
            .prepare_cached(
                "INSERT OR IGNORE INTO claim_verdict_links(link, claim_id) VALUES (?1, ?2)",
            )?
            .execute(params![link, claim_id])?;
    }
    connection
        .prepare_cached(
            "INSERT OR REPLACE INTO claim_verdicts(claim_id, verdict, reason, signer, on_behalf)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?
        .execute(params![
            claim_id,
            verdict.name(),
            verdict.reason(),
            signature.signer,
            signature.on_behalf
        ])?;
    Ok(previous.as_ref() != Some(verdict))
}

/// Queue every verdict linked to `link`.
fn queue_linked_tx(connection: &Connection, link: &str) -> Result<()> {
    connection
        .prepare_cached(
            "INSERT OR IGNORE INTO claim_verdict_queue(claim_id)
             SELECT claim_id FROM claim_verdict_links WHERE link=?1",
        )?
        .execute([link])?;
    Ok(())
}

/// The links a newly arrived signed claim can change: the claims waiting on it as a
/// delegation, the claims signed by a key it revokes, and the claims sharing its nonce.
fn queue_affected_by_tx(connection: &Connection, claim_id: &str) -> Result<()> {
    let Some(row) = claim_row(connection, claim_id)? else {
        return Ok(());
    };
    let Some(signature) = stored_signature(connection, claim_id)? else {
        return Ok(());
    };
    queue_linked_tx(
        connection,
        &format!("nonce:{}|{}", signature.key, signature.nonce),
    )?;
    let field_key = row
        .body
        .pointer("/fields/key")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match (row.kind.as_str(), field_key) {
        (KEY_GRANTED, _) => queue_linked_tx(connection, &format!("delegation:{claim_id}"))?,
        (KEY_REVOKED, Some(key)) => {
            queue_linked_tx(connection, &format!("key:{}|{key}", row.subject))?
        }
        _ => {}
    }
    Ok(())
}

/// One claim this node is about to seal, as signing needs it.
pub struct Unsealed<'a> {
    pub subject: &'a str,
    pub kind: &'a str,
    pub actor: Option<&'a str>,
    pub body: &'a Value,
    pub accepted_at_unix_ms: u128,
}

/// What a verdict recheck found.
#[derive(Clone, Debug, Serialize)]
pub struct VerdictMismatch {
    pub claim_id: String,
    pub cached: Option<String>,
    pub recomputed: String,
}

impl Store {
    /// Write `input` with a signature a device made for it, in the same transaction. The
    /// signature must already verify for `input`; an idempotent repeat keeps the first claim and
    /// its signature.
    pub fn append_signed_claim(
        &self,
        input: &ClaimInput,
        signature: &ClaimSignature,
    ) -> Result<(ClaimRecord, bool), St3Error> {
        let actor = input
            .actor
            .clone()
            .ok_or_else(|| St3Error::new("invalid-signature", "a signed claim names its actor"))?;
        let key = (input.subject.clone(), input.kind.clone(), actor);
        let text = serde_json::to_string(signature).map_err(internal)?;
        {
            let connection = self.connection.write();
            connection
                .execute(
                    "INSERT OR REPLACE INTO expected_claim_signatures(subject, kind, actor, signature)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![key.0, key.1, key.2, text],
                )
                .map_err(internal)?;
        }
        let appended = self.append_claim_outcome(input);
        // Nothing waits for a signature once the write has happened or failed.
        let connection = self.connection.write();
        connection
            .execute(
                "DELETE FROM expected_claim_signatures WHERE subject=?1 AND kind=?2 AND actor=?3",
                params![key.0, key.1, key.2],
            )
            .map_err(internal)?;
        appended
    }

    /// Whether another claim already carries this key and nonce.
    pub fn signature_nonce_used(&self, key: &str, nonce: &str) -> Result<bool> {
        let connection = self.readers.get();
        Ok(connection
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM claim_signatures WHERE key=?1 AND nonce=?2)",
            )?
            .query_row(params![key, nonce], |row| row.get(0))?)
    }

    /// Enrol a device of `person`: the person's root key, which this node holds, grants `key` as
    /// a device key. Returns the chain the device signs with: its grant, then the root's.
    pub fn enroll_device_key(
        &self,
        person: &str,
        key: &str,
        label: &str,
    ) -> Result<Vec<String>, St3Error> {
        if Family::of(person) != Some(Family::Person) {
            return Err(St3Error::new(
                "invalid-person",
                "only a person enrols devices",
            ));
        }
        self.ensure_principal_key(person)?;
        let root = self.held_root(person).ok_or_else(|| {
            St3Error::new(
                "no-person-key",
                "this node holds no root key for the person, so it cannot enrol a device",
            )
        })?;
        let (granted, _) = self.runtime.append_claim(
            self,
            &ClaimInput {
                subject: person.into(),
                kind: KEY_GRANTED.into(),
                actor: Some(person.into()),
                fields: KeyGrant {
                    key: key.into(),
                    role: Role::Device,
                    issuer: person.into(),
                    issuer_key: root.key.public().into(),
                    label: Some(label.into()),
                }
                .fields(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("device-key:{person}:{key}")),
            },
        )?;
        let mut chain = vec![granted.id];
        chain.extend(root.chain.iter().cloned());
        Ok(chain)
    }

    /// Withdraw a device key of `person`, as the node.
    pub fn revoke_device_key(&self, person: &str, key: &str, reason: &str) -> Result<(), St3Error> {
        self.append_claim(&ClaimInput {
            subject: person.into(),
            kind: KEY_REVOKED.into(),
            actor: None,
            fields: BTreeMap::from([
                ("key".into(), Value::String(key.into())),
                ("reason".into(), Value::String(reason.into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("device-key-revoked:{person}:{key}")),
        })
        .map(|_| ())
    }

    /// Use `directory` for the private keys this node mints for people and agents, and load the
    /// keys it already holds.
    pub fn use_key_directory(&self, directory: &Path) -> Result<()> {
        self.keyring.set_directory(directory);
        let rows = {
            let connection = self.readers.get();
            let mut statement =
                connection.prepare("SELECT public_key, principal, role, chain FROM held_keys")?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (public, principal, role, chain) in rows {
            let Some(key) = self.keyring.load(&public)? else {
                continue;
            };
            let Ok(role) = serde_json::from_value::<Role>(Value::String(role)) else {
                continue;
            };
            self.keyring.insert(HeldKey {
                principal,
                role,
                key,
                chain: serde_json::from_str(&chain).unwrap_or_default(),
            });
        }
        Ok(())
    }

    /// The key that signs this node's own claims and vouches for the people and agents it
    /// speaks for. A fleet member's node key is its member key.
    pub fn set_node_key(&self, key: Arc<crate::fleet::MemberKey>) -> Result<()> {
        let public = key.public().to_owned();
        self.keyring.set_node(Some(key));
        let connection = self.connection.write();
        connection.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![OWN_NODE_KEY, public],
        )?;
        Ok(())
    }

    fn node_subject(&self) -> String {
        format!("host/{}", self.origin)
    }

    /// Make sure this node holds a signing key for `actor` when it is a person or an agent,
    /// minting one and writing its delegation the first time. Without a node key nothing is
    /// minted, and claims stay unsigned as before.
    pub fn ensure_principal_key(&self, actor: &str) -> Result<(), St3Error> {
        let family = match Family::of(actor) {
            Some(family @ (Family::Person | Family::Agent)) => family,
            _ => return Ok(()),
        };
        if self.keyring.signing(actor).is_some() {
            return Ok(());
        }
        let Some(node) = self.keyring.node() else {
            return Ok(());
        };
        let _minting = self
            .principal_minting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.keyring.signing(actor).is_some() {
            return Ok(());
        }
        let node_subject = self.node_subject();
        let grant = |subject: &str,
                     key: &crate::fleet::MemberKey,
                     role: Role,
                     issuer: &str,
                     issuer_key: &str,
                     label: Option<String>|
         -> Result<ClaimRecord, St3Error> {
            self.runtime
                .append_claim(
                    self,
                    &ClaimInput {
                        subject: subject.into(),
                        kind: KEY_GRANTED.into(),
                        actor: Some(issuer.into()),
                        fields: KeyGrant {
                            key: key.public().into(),
                            role,
                            issuer: issuer.into(),
                            issuer_key: issuer_key.into(),
                            label,
                        }
                        .fields(),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: None,
                    },
                )
                .map(|(claim, _)| claim)
        };
        let label = Some(format!("{} on {}", actor, self.origin));
        match family {
            Family::Agent => {
                let key = self.keyring.create().map_err(internal)?;
                let granted = grant(
                    actor,
                    &key,
                    Role::Agent,
                    &node_subject,
                    node.public(),
                    label,
                )?;
                self.hold_key(actor, Role::Agent, key, vec![granted.id])?;
            }
            Family::Person => {
                let root = match self.held_root(actor) {
                    Some(root) => root,
                    None => {
                        let key = self.keyring.create().map_err(internal)?;
                        let granted =
                            grant(actor, &key, Role::Root, &node_subject, node.public(), None)?;
                        self.hold_key(actor, Role::Root, key, vec![granted.id])?
                    }
                };
                let device = self.keyring.create().map_err(internal)?;
                let granted = grant(
                    actor,
                    &device,
                    Role::Device,
                    actor,
                    root.key.public(),
                    label,
                )?;
                let mut chain = vec![granted.id];
                chain.extend(root.chain.iter().cloned());
                self.hold_key(actor, Role::Device, device, chain)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn held_root(&self, principal: &str) -> Option<HeldKey> {
        let connection = self.readers.get();
        let public = connection
            .query_row(
                "SELECT public_key FROM held_keys WHERE principal=?1 AND role='root'",
                [principal],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()?;
        self.keyring.by_public(&public)
    }

    fn hold_key(
        &self,
        principal: &str,
        role: Role,
        key: Arc<crate::fleet::MemberKey>,
        chain: Vec<String>,
    ) -> Result<HeldKey, St3Error> {
        let role_name = serde_json::to_value(role)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        self.connection
            .write()
            .execute(
                "INSERT OR REPLACE INTO held_keys(public_key, principal, role, chain)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    key.public(),
                    principal,
                    role_name,
                    serde_json::to_string(&chain).map_err(internal)?
                ],
            )
            .map_err(internal)?;
        let held = HeldKey {
            principal: principal.into(),
            role,
            key,
            chain,
        };
        self.keyring.insert(held.clone());
        Ok(held)
    }

    /// Sign one of this node's claims as it is sealed: a key grant by its issuer, a claim by a
    /// principal this node holds a key for with that key, anything else by the node on the
    /// writer's behalf. `None` without a node key.
    pub(crate) fn sign_unsealed(&self, claim: &Unsealed<'_>) -> Option<ClaimSignature> {
        let node = self.keyring.node()?;
        let content = content_digest(claim.subject, claim.kind, claim.actor, claim.body);
        let signed_at = u64::try_from(claim.accepted_at_unix_ms).unwrap_or(u64::MAX);
        let node_subject = self.node_subject();
        if claim.kind == KEY_GRANTED
            && let Some(grant) = claim.body.get("fields").and_then(KeyGrant::from_fields)
        {
            if grant.issuer == node_subject && grant.issuer_key == node.public() {
                return Some(ClaimSignature::sign(
                    &node,
                    &content,
                    &node_subject,
                    None,
                    Vec::new(),
                    signed_at,
                ));
            }
            if let Some(issuer) = self.keyring.by_public(&grant.issuer_key)
                && issuer.principal == grant.issuer
            {
                return Some(ClaimSignature::sign(
                    &issuer.key,
                    &content,
                    &issuer.principal,
                    None,
                    issuer.chain,
                    signed_at,
                ));
            }
        }
        if let Some(actor) = claim.actor
            && let Some(held) = self.keyring.signing(actor)
        {
            return Some(ClaimSignature::sign(
                &held.key, &content, actor, None, held.chain, signed_at,
            ));
        }
        let on_behalf = claim.actor.filter(|actor| *actor != node_subject);
        Some(ClaimSignature::sign(
            &node,
            &content,
            &node_subject,
            on_behalf,
            Vec::new(),
            signed_at,
        ))
    }

    /// Whether a pass has anything to judge: a newly signed claim, or a queued one. Two probes
    /// of tiny tables on a reader.
    fn verdicts_pending(&self) -> Result<bool> {
        let connection = self.readers.get();
        Ok(connection
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM claim_verdict_fresh)
                     OR EXISTS(SELECT 1 FROM claim_verdict_queue)",
            )?
            .query_row([], |row| row.get(0))?)
    }

    /// Judge every signed claim admitted since the last pass and every queued one. With
    /// `check_roots`, first queue every verdict that relied on the trust roots if fleet
    /// membership changed them. Each chunk takes the writer once; queued writes run between
    /// chunks. A pass with nothing to do reads one row.
    pub fn judge_claims(&self, check_roots: bool) -> Result<usize> {
        if check_roots {
            let mut connection = self.connection.write();
            let transaction = connection.transaction()?;
            let roots = roots(&transaction, &self.origin)?;
            let roots_digest = crate::hash::canonical_hash(&roots)?;
            if fleet_meta(&transaction, VERDICT_ROOTS)?.as_deref() != Some(roots_digest.as_str()) {
                queue_linked_tx(&transaction, "root")?;
                set_meta_tx(&transaction, VERDICT_ROOTS, &roots_digest)?;
            }
            transaction.commit()?;
        }
        if !self.verdicts_pending()? {
            return Ok(0);
        }
        let mut judged = 0;
        loop {
            let mut connection = self.connection.write();
            let transaction = connection.transaction()?;
            let roots = roots(&transaction, &self.origin)?;
            let fresh = transaction
                .prepare_cached("SELECT claim_id FROM claim_verdict_fresh LIMIT ?1")?
                .query_map([JUDGE_CHUNK as i64], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for claim_id in &fresh {
                transaction
                    .prepare_cached("DELETE FROM claim_verdict_fresh WHERE claim_id=?1")?
                    .execute([claim_id])?;
                // A new delegation, revocation or reused nonce changes what relied on it.
                queue_affected_by_tx(&transaction, claim_id)?;
                transaction
                    .prepare_cached(
                        "INSERT OR IGNORE INTO claim_verdict_queue(claim_id) VALUES (?1)",
                    )?
                    .execute([claim_id])?;
            }
            let queued = transaction
                .prepare_cached("SELECT claim_id FROM claim_verdict_queue LIMIT ?1")?
                .query_map([JUDGE_CHUNK as i64], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let memo = RefCell::new(BTreeMap::new());
            for claim_id in &queued {
                transaction
                    .prepare_cached("DELETE FROM claim_verdict_queue WHERE claim_id=?1")?
                    .execute([claim_id])?;
                let Some(signature) = stored_signature(&transaction, claim_id)? else {
                    continue;
                };
                // A signature can arrive for a claim a checkpoint has since dropped.
                if claim_row(&transaction, claim_id)?.is_none() {
                    continue;
                }
                let (verdict, links) = judge_claim(&transaction, &roots, claim_id, 0, &memo)?;
                if write_verdict_tx(&transaction, claim_id, &signature, &verdict, &links)? {
                    // A changed delegation or revocation changes what relied on it.
                    queue_affected_by_tx(&transaction, claim_id)?;
                }
                judged += 1;
            }
            transaction.commit()?;
            if fresh.is_empty() && queued.is_empty() {
                return Ok(judged);
            }
        }
    }

    /// A claim's verdict as the cache holds it. A claim with no signature is `unsigned`.
    pub fn claim_verdict(&self, claim_id: &str) -> Result<Verdict> {
        let connection = self.readers.get();
        Ok(cached_verdict(&connection, claim_id)?.unwrap_or(Verdict::Unsigned))
    }

    /// A claim's signature, when it has one.
    pub fn claim_signature(&self, claim_id: &str) -> Result<Option<ClaimSignature>> {
        stored_signature(&self.readers.get(), claim_id)
    }

    /// How many claims have each verdict, read only. This node's own claims are signed as their
    /// batches are sealed: claims in batches not sealed yet count as `unsealed`, and every other
    /// claim without a cached verdict is `unsigned`.
    pub fn claim_verdict_counts(&self) -> Result<BTreeMap<String, u64>> {
        let connection = self.readers.get();
        let unsealed: u64 = connection.query_row(
            "SELECT COUNT(*) FROM batches JOIN claims ON claims.batch_id=batches.id
             WHERE batches.rowid>?1",
            [self.seeded_batch_rowid.load(Ordering::Acquire)],
            |row| row.get(0),
        )?;
        let mut counts = connection
            .prepare("SELECT verdict, COUNT(*) FROM claim_verdicts GROUP BY verdict")?
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
            })?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        let total: u64 =
            connection.query_row("SELECT COUNT(*) FROM claims", [], |row| row.get(0))?;
        let judged = counts.values().sum::<u64>();
        *counts.entry("unsigned".into()).or_default() +=
            total.saturating_sub(judged).saturating_sub(unsealed);
        if unsealed != 0 {
            counts.insert("unsealed".into(), unsealed);
        }
        Ok(counts)
    }

    /// Recompute every verdict from the claims alone and replace the cache with the result,
    /// reporting each claim whose cached verdict differed. A cache row edited, deleted or added
    /// by hand shows here and never survives.
    pub fn recheck_claim_verdicts(&self) -> Result<Vec<VerdictMismatch>> {
        let cached = {
            let connection = self.readers.get();
            connection
                .prepare("SELECT claim_id, verdict FROM claim_verdicts")?
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<BTreeMap<_, _>>>()?
        };
        {
            let mut connection = self.connection.write();
            let transaction = connection.transaction()?;
            transaction.execute_batch(
                "DELETE FROM claim_verdicts; DELETE FROM claim_verdict_links;
                 DELETE FROM claim_verdict_queue;
                 INSERT OR IGNORE INTO claim_verdict_fresh(claim_id) SELECT claim_id FROM claim_signatures;",
            )?;
            transaction.execute("DELETE FROM meta WHERE key=?1", params![VERDICT_ROOTS])?;
            transaction.commit()?;
        }
        self.judge_claims(true)?;
        let connection = self.readers.get();
        let recomputed = connection
            .prepare("SELECT claim_id, verdict FROM claim_verdicts")?
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        let mut mismatches = Vec::new();
        for claim_id in cached
            .keys()
            .chain(recomputed.keys())
            .collect::<BTreeSet<_>>()
        {
            let before = cached.get(claim_id);
            let after = recomputed
                .get(claim_id)
                .cloned()
                .unwrap_or_else(|| "unsigned".into());
            if before.map(String::as_str).unwrap_or("unsigned") != after {
                mismatches.push(VerdictMismatch {
                    claim_id: claim_id.clone(),
                    cached: before.cloned(),
                    recomputed: after,
                });
            }
        }
        Ok(mismatches)
    }
}

fn set_meta_tx(connection: &Connection, key: &str, value: &str) -> Result<()> {
    connection
        .prepare_cached(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        )?
        .execute(params![key, value])?;
    Ok(())
}

/// The rules as the graph holds them: for each `rule/NAME`, its latest [`crate::rules::RULE_SET`]
/// claim in canonical order.
pub fn current_rules_tx(connection: &Connection) -> Result<Vec<crate::rules::NamedRule>> {
    let mut statement = connection.prepare_cached(&format!(
        "SELECT claims.subject, claims.id, claims.body FROM claims
         WHERE claims.kind=?1 ORDER BY {CANONICAL_ORDER}"
    ))?;
    let mut latest = BTreeMap::new();
    for row in statement.query_map([crate::rules::RULE_SET], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (subject, id, body) = row?;
        latest.insert(subject, (id, body));
    }
    Ok(latest
        .into_iter()
        .filter_map(|(subject, (claim_id, body))| {
            let body: Value = serde_json::from_str(&body).ok()?;
            Some(crate::rules::NamedRule {
                name: subject.strip_prefix("rule/")?.to_owned(),
                claim_id,
                rule: crate::rules::Rule::from_fields(body.get("fields")?)?,
            })
        })
        .collect())
}

/// Check one local write against the rules, inside the write's own transaction: refuse it with
/// `rule-denied` under a rule in enforce mode, and record each rule in audit mode that would have
/// refused it beside the write. Writes without an actor, and the audit records themselves, are
/// the node's own and pass; a person may always set a rule, so no rule can lock its owner out.
/// With no rules, one indexed probe.
pub fn rules_gate_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    actor: &str,
    kind: &str,
    subject: &str,
) -> Result<()> {
    if kind == crate::rules::RULE_SET {
        if Family::of(actor) == Some(Family::Person) {
            return Ok(());
        }
        return Err(anyhow::Error::new(St3Error::new(
            "rule-denied",
            format!("only a person sets rules, not {actor}"),
        )));
    }
    if kind == crate::rules::RULE_AUDITED {
        return Ok(());
    }
    let any: bool = transaction
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM claims WHERE kind=?1)")?
        .query_row([crate::rules::RULE_SET], |row| row.get(0))?;
    if !any {
        return Ok(());
    }
    let rules = current_rules_tx(transaction)?;
    let decision = crate::rules::decide(&rules, actor, kind, subject);
    if let Some(denied) = decision.denied {
        return Err(anyhow::Error::new(St3Error::new(
            "rule-denied",
            format!(
                "rule `{}` refuses {actor} writing {kind} on {subject}{}",
                denied.name,
                if denied.rule.description.is_empty() {
                    String::new()
                } else {
                    format!(": {}", denied.rule.description)
                }
            ),
        )));
    }
    for rule in decision.audited {
        append_claim_record_tx(
            transaction,
            origin,
            &format!("rule/{}", rule.name),
            crate::rules::RULE_AUDITED,
            None,
            &serde_json::json!({
                "fields": {
                    "rule": rule.claim_id,
                    "actor": actor,
                    "action": kind,
                    "target": subject,
                },
                "evidence": [],
            }),
            &[],
            None,
        )?;
    }
    Ok(())
}

impl Store {
    /// Set `rule/NAME` as `person` sets it.
    pub fn set_rule(
        &self,
        name: &str,
        rule: &crate::rules::Rule,
        person: &str,
    ) -> Result<ClaimRecord, St3Error> {
        if name.is_empty() || name.contains('/') {
            return Err(St3Error::new(
                "invalid-rule-name",
                "a rule name is one path segment",
            ));
        }
        self.append_claim(&ClaimInput {
            subject: format!("rule/{name}"),
            kind: crate::rules::RULE_SET.into(),
            actor: Some(person.into()),
            fields: rule.fields(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
    }

    /// The newest audit records, of one rule or of all, newest first.
    pub fn rule_audits(&self, rule: Option<&str>, limit: usize) -> Result<Vec<ClaimRecord>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(&format!(
            "SELECT {CLAIM_COLUMNS} FROM claims JOIN batches ON batches.id=claims.batch_id
             WHERE claims.kind=?1 AND (?2 IS NULL OR claims.subject=?2)
             ORDER BY claims.store_index DESC LIMIT ?3"
        ))?;
        let subject = rule.map(|name| format!("rule/{name}"));
        Ok(statement
            .query_map(
                params![crate::rules::RULE_AUDITED, subject, limit as i64],
                claim_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The rules, from memory; read again after any rule changes.
    pub fn current_rules(&self) -> Result<Arc<Vec<crate::rules::NamedRule>>> {
        if !self.rules_stale.load(Ordering::Acquire)
            && let Some(rules) = self
                .rules
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        {
            return Ok(rules);
        }
        self.rules_stale.store(false, Ordering::Release);
        let rules = Arc::new(current_rules_tx(&self.readers.get())?);
        *self.rules.write().unwrap_or_else(PoisonError::into_inner) = Some(rules.clone());
        Ok(rules)
    }
}
