//! The complete pre-candidate authorization model, maintained only during an online upgrade.
//! It reads its own signature, verdict and dependency relations. Candidate inventory never
//! supplies facts to this model; activation permanently switches readers to the reconciled
//! candidate relations. This is a one-way schema transition, not a failure fallback.
use super::*;
const VERDICT_ROOTS: &str = "claim_verdict_roots";

pub(super) const TABLES: [&str; 5] = [
    "legacy_claim_signatures",
    "legacy_claim_verdicts",
    "legacy_claim_verdict_links",
    "legacy_claim_verdict_queue",
    "legacy_claim_verdict_fresh",
];
pub(super) fn table_exists(connection: &Connection) -> Result<bool> {
    Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='legacy_claim_signatures')", [], |row| row.get(0))?)
}
// The control row avoids rescanning the whole schema for every trim/removal.
pub(super) fn present(connection: &Connection) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT legacy_present FROM claim_signature_migration WHERE id=1",
        [],
        |row| row.get(0),
    )?)
}
pub(super) fn active(connection: &Connection) -> Result<bool> {
    let (phase, serving, fault): (String, bool, Option<String>) = connection.query_row(
        "SELECT phase,serving_legacy,legacy_fault FROM claim_signature_migration WHERE id=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    anyhow::ensure!(
        matches!(phase.as_str(), "legacy" | "envelopes" | "judging" | "done"),
        "signature-state-corrupt: unknown bootstrap phase {phase}"
    );
    anyhow::ensure!(
        !serving || phase != "done",
        "signature-state-corrupt: completed bootstrap cannot serve legacy authorization"
    );
    if serving && let Some(fault) = fault {
        anyhow::bail!("signature-state-corrupt: legacy projection: {fault}");
    }
    Ok(serving)
}

fn pending(connection: &Connection) -> Result<bool> {
    Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM legacy_claim_verdict_fresh) OR EXISTS(SELECT 1 FROM legacy_claim_verdict_queue)", [], |row| row.get(0))?)
}
pub(super) fn stored_signature(
    connection: &Connection,
    claim_id: &str,
) -> Result<Option<ClaimSignature>> {
    let text = connection
        .prepare_cached("SELECT signature FROM legacy_claim_signatures WHERE claim_id=?1")?
        .query_row([claim_id], |row| row.get::<_, String>(0))
        .optional()?;
    text.map(|text| Ok(serde_json::from_str(&text)?))
        .transpose()
}

/// Keep `signature` for `claim_id`, and mark the claim for the next verdict pass. A signature is
/// never replaced: the first one stored is the one the claim's envelope carried.
pub(super) fn store_claim_signature_tx(
    connection: &Connection,
    claim_id: &str,
    signature: &ClaimSignature,
) -> Result<()> {
    let stored = connection
        .prepare_cached(
            "INSERT OR IGNORE INTO legacy_claim_signatures(claim_id, signer, key, nonce, signature)
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
            .prepare_cached(
                "INSERT OR IGNORE INTO legacy_claim_verdict_fresh(claim_id) VALUES (?1)",
            )?
            .execute([claim_id])?;
    }
    Ok(())
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
    fault: RefCell<Option<anyhow::Error>>,
}

impl StoreFacts<'_> {
    fn read<T>(&self, result: Result<T>, neutral: T) -> T {
        match result {
            Ok(value) => value,
            Err(error) => {
                self.fault.borrow_mut().get_or_insert(error);
                neutral
            }
        }
    }
    fn before(&self, other: &str) -> bool {
        self.read(position(self.connection, other), None)
            .is_some_and(|other| other.compare(&self.position) == CmpOrdering::Less)
    }

    fn verdict_of(&self, claim_id: &str) -> Verdict {
        if let Some(verdict) = self.memo.borrow().get(claim_id) {
            return verdict.clone();
        }
        if let Some(verdict) = self.read(cached_verdict(self.connection, claim_id), None) {
            return verdict;
        }
        if self.depth >= MAX_CHAIN_DEPTH {
            return Verdict::Invalid("the chain is too deep or loops".into());
        }
        let verdict = self.read(
            judge_claim(
                self.connection,
                self.roots,
                claim_id,
                self.depth + 1,
                self.memo,
            )
            .map(|(verdict, _)| verdict),
            Verdict::Held("signature-state-corrupt: legacy facts could not be read".into()),
        );
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
        let row = self.read(claim_row(self.connection, claim_id), None)?;
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
                "SELECT claims.id FROM claims JOIN legacy_claim_signatures ON legacy_claim_signatures.claim_id=claims.id
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
            .map_err(anyhow::Error::from);
        let revocations = self.read(revocations, Vec::new());
        revocations.into_iter().any(|revocation| {
            revocation != self.claim_id
                && self.before(&revocation)
                && self.verdict_of(&revocation) == Verdict::Verified
                && self
                    .read(stored_signature(self.connection, &revocation), None)
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
                "SELECT claim_id FROM legacy_claim_signatures WHERE key=?1 AND nonce=?2 AND claim_id<>?3",
            )
            .and_then(|mut statement| {
                statement
                    .query_map(params![key, nonce, self.claim_id], |row| {
                        row.get::<_, String>(0)
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .map_err(anyhow::Error::from);
        let others = self.read(others, Vec::new());
        others.iter().any(|other| self.before(other))
    }
}

pub(super) fn cached_verdict(connection: &Connection, claim_id: &str) -> Result<Option<Verdict>> {
    Ok(connection
        .prepare_cached("SELECT verdict, reason FROM legacy_claim_verdicts WHERE claim_id=?1")?
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
        fault: RefCell::default(),
    };
    let mut fields = row.body.get("fields").cloned().unwrap_or(Value::Null);
    // A long message's claim holds a preview, and names its whole text in an `attachments` entry
    // of type text/plain. A device that signs such a message names `body_sha256` and `body_bytes`
    // among its signed fields; they are those entry's hash and size, not stored fields.
    if row.kind == "message.sent"
        && let Some(body) = fields
            .get("attachments")
            .and_then(crate::principal::message_body_entry)
            .cloned()
    {
        fields["body_sha256"] = body.get("sha256").cloned().unwrap_or(Value::Null);
        fields["body_bytes"] = body.get("size").cloned().unwrap_or(Value::Null);
    }
    let judged = crate::principal::Judged {
        id: &row.id,
        subject: &row.subject,
        kind: &row.kind,
        actor: row.actor.as_deref(),
        content: content_digest(&row.subject, &row.kind, row.actor.as_deref(), &row.body),
        fields: &fields,
    };
    let verdict = judge(&judged, signature.as_ref(), &facts);
    if let Some(error) = facts.fault.into_inner() {
        return Err(error.context("legacy authorization facts"));
    }
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
        .prepare_cached("DELETE FROM legacy_claim_verdict_links WHERE claim_id=?1")?
        .execute([claim_id])?;
    for link in links {
        connection
            .prepare_cached(
                "INSERT OR IGNORE INTO legacy_claim_verdict_links(link, claim_id) VALUES (?1, ?2)",
            )?
            .execute(params![link, claim_id])?;
    }
    connection
        .prepare_cached(
            "INSERT OR REPLACE INTO legacy_claim_verdicts(claim_id, verdict, reason, signer, on_behalf)
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
            "INSERT OR IGNORE INTO legacy_claim_verdict_queue(claim_id)
             SELECT claim_id FROM legacy_claim_verdict_links WHERE link=?1",
        )?
        .execute([link])?;
    Ok(())
}

/// The links a newly arrived signed claim can change: the claims waiting on it as a
/// delegation, the claims signed by a key it revokes, and the claims sharing its nonce.
pub(super) fn queue_affected_by_tx(connection: &Connection, claim_id: &str) -> Result<()> {
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

pub(super) fn judge_claims(store: &Store, check_roots: bool) -> Result<usize> {
    let result = judge_claims_inner(store, check_roots);
    if let Err(error) = &result {
        let contention=error.downcast_ref::<St3Error>().is_some_and(St3Error::is_sqlite_contention)
            || error.downcast_ref::<rusqlite::Error>().is_some_and(|error|matches!(error,rusqlite::Error::SqliteFailure(code,_) if matches!(code.code,rusqlite::ErrorCode::DatabaseBusy|rusqlite::ErrorCode::DatabaseLocked)));
        if !contention {
            store.connection.write().execute("UPDATE claim_signature_migration SET legacy_fault=?1 WHERE id=1 AND serving_legacy=1",[format!("{error:#}")])?;
        }
    }
    result
}
fn judge_claims_inner(store: &Store, check_roots: bool) -> Result<usize> {
    if check_roots {
        let mut connection = store.connection.write();
        let transaction = connection.transaction()?;
        if !active(&transaction)? {
            return Ok(0);
        }
        let roots = store.trust_roots(&transaction, &store.origin)?;
        let roots_digest = crate::hash::canonical_hash(&roots)?;
        if fleet_meta(&transaction, VERDICT_ROOTS)?.as_deref() != Some(roots_digest.as_str()) {
            queue_linked_tx(&transaction, "root")?;
            set_meta_tx(&transaction, VERDICT_ROOTS, &roots_digest)?;
        }
        transaction.commit()?;
    }
    if !store.read_snapshot(|_| {
        let connection = store.readers.get();
        Ok(active(&connection)? && pending(&connection)?)
    })? {
        return Ok(0);
    }
    let mut judged = 0;
    loop {
        let mut connection = store.connection.write();
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if !active(&transaction)? {
            return Ok(judged);
        }
        let roots = store.trust_roots(&transaction, &store.origin)?;
        let fresh = transaction
            .prepare_cached("SELECT claim_id FROM legacy_claim_verdict_fresh LIMIT ?1")?
            .query_map([JUDGE_CHUNK as i64], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for claim_id in &fresh {
            transaction
                .prepare_cached("DELETE FROM legacy_claim_verdict_fresh WHERE claim_id=?1")?
                .execute([claim_id])?;
            // A new delegation, revocation or reused nonce changes what relied on it.
            queue_affected_by_tx(&transaction, claim_id)?;
            transaction
                .prepare_cached(
                    "INSERT OR IGNORE INTO legacy_claim_verdict_queue(claim_id) VALUES (?1)",
                )?
                .execute([claim_id])?;
        }
        let queued = transaction
            .prepare_cached("SELECT claim_id FROM legacy_claim_verdict_queue LIMIT ?1")?
            .query_map([JUDGE_CHUNK as i64], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let memo = RefCell::new(BTreeMap::new());
        for claim_id in &queued {
            transaction
                .prepare_cached("DELETE FROM legacy_claim_verdict_queue WHERE claim_id=?1")?
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
