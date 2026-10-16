//! Sealing prepares each chunk before it takes the writer. These controls seal real batches
//! (plain notes, notes that name a stored blob or a missing one, claims signed before sealing
//! or by it) through sealing and through a frozen copy of the sealing functions as they were
//! before preparation (main at 5bf3105b9), and compare every stored row. Then they change the
//! store between preparation and the writer: a blob stored or dropped, a claim signature that
//! arrives, the member key, the fleet id or the node key replaced, an agent's key replaced, a
//! keyring change in progress at the loan, keys added for principals no claim is signed as, a
//! batch sealed by another pass, a claim deleted, an own envelope's signature removed. And they
//! hold a chunk to its payload budget.
use super::runtime::Plain;
use super::*;
use crate::fleet::{MemberKey, envelope_signature_message, verify_signature};
use crate::principal::{
    ClaimSignature, HeldKey, Judged, KEY_GRANTED, KeyGrant, Role, content_digest,
};
use std::cell::Cell;

const FLEET: &str = "3e7a9c41-5d2b-4f86-b0c3-9a1e6d4f2b58";
const OTHER_FLEET: &str = "c2b5e8f1-7a3d-4c69-9e04-1f6b3a8d5c27";

fn node(name: &str) -> Arc<Store> {
    let store = Arc::new(Store::open_memory(name, Arc::new(Plain)).unwrap());
    store.bind_fleet(FLEET).unwrap();
    store
}

/// Give `store` a member key, which is also its node key: sealing signs claims and envelopes.
fn keyed(store: &Store) -> Arc<MemberKey> {
    let key = Arc::new(MemberKey::generate().unwrap().0);
    store.load_member_key(Some(key.clone())).unwrap();
    key
}

fn note(store: &Store, subject: &str, fields: BTreeMap<String, Value>) -> ClaimRecord {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "example.note".into(),
            actor: None,
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
}

fn claim_by(
    store: &Store,
    subject: &str,
    kind: &str,
    actor: Option<&str>,
    fields: BTreeMap<String, Value>,
) -> ClaimRecord {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: actor.map(str::to_owned),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
}

fn naming(hash: &str) -> BTreeMap<String, Value> {
    BTreeMap::from([("hash".into(), Value::String(hash.into()))])
}

fn unsealed(claim: &ClaimRecord) -> principals::Unsealed<'_> {
    principals::Unsealed {
        subject: &claim.subject,
        kind: &claim.kind,
        actor: claim.actor.as_deref(),
        body: &claim.body,
        accepted_at_unix_ms: claim.accepted_at_unix_ms,
    }
}

fn claims(connection: &Connection) -> Vec<ClaimRecord> {
    connection
        .prepare(
            "SELECT id, store_index, batch_id, subject, kind, origin, actor, body, predecessors, accepted_at_unix_ms
             FROM claims ORDER BY store_index",
        )
        .unwrap()
        .query_map([], claim_from_row)
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

/// Whether `signature` is a genuine signature of `claim`, checked independently of sealing.
fn verifies(claim: &ClaimRecord, signature: &ClaimSignature) -> bool {
    signature.verifies(&Judged {
        id: &claim.id,
        subject: &claim.subject,
        kind: &claim.kind,
        actor: claim.actor.as_deref(),
        content: content_digest(
            &claim.subject,
            &claim.kind,
            claim.actor.as_deref(),
            &claim.body,
        ),
        fields: &Value::Null,
    })
}

/// Run `change` once, after sealing prepares its next chunk and before it takes the writer.
fn before_the_loan(change: impl FnOnce(&Store) + 'static) {
    SEAL_BEFORE_LOAN.with(|hook| *hook.borrow_mut() = Some(Box::new(change)));
}

/// Seal everything written so far, and return how many chunks were prepared again.
fn seal(store: &Store) -> usize {
    SEALS_PREPARED_AGAIN.with(|count| count.set(0));
    store.seal_local_batches().unwrap();
    SEALS_PREPARED_AGAIN.with(Cell::get)
}

/// Run the sealing passes as they ran before preparation, chunk by chunk, in a transaction
/// that is rolled back, and return what it stored, read with `read`.
fn sealed_by_the_old_passes<T>(store: &Store, read: impl FnOnce(&Connection) -> T) -> T {
    let target = max_batch_rowid(&store.readers.get()).unwrap();
    let mut connection = store.connection.write();
    let transaction = connection.transaction().unwrap();
    let mut seeded_through = store.seeded_batch_rowid.load(Ordering::Acquire);
    while seeded_through < target {
        let through: i64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(rowid), ?2) FROM (
                    SELECT rowid FROM batches WHERE rowid>?1 AND rowid<=?2
                    ORDER BY rowid LIMIT ?3)",
                params![seeded_through, target, 64],
                |row| row.get(0),
            )
            .unwrap();
        legacy_seed_replica_envelopes_signed_tx(
            &transaction,
            &store.origin,
            Some(seeded_through),
            Some(through),
            Some(&|claim: &principals::Unsealed<'_>| store.sign_unsealed(claim)),
        )
        .unwrap();
        legacy_sign_own_envelopes_range_tx(
            store,
            &transaction,
            Some(seeded_through),
            Some(through),
        )
        .unwrap();
        seeded_through = through;
    }
    read(&transaction)
}

/// Every row sealing writes, in key order, without the time a signature was stored.
fn sealed_rows(connection: &Connection) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
    [
        "SELECT writer, sequence, envelope_hash, previous_hash, accepted_at_unix_ms, payload,
                batch_id, relay, receipt_state, received_at_unix_ms
         FROM replica_envelopes ORDER BY writer, sequence, envelope_hash",
        "SELECT record_ref, writer, sequence, envelope_hash, position, raw, state, claim_id,
                subject_hint, kind_hint, updated_at_unix_ms
         FROM replica_records ORDER BY record_ref",
        "SELECT writer, sequence, envelope_hash, member_key, signature
         FROM replica_envelope_signatures ORDER BY writer, sequence, envelope_hash, member_key",
        "SELECT claim_id, signer, key, nonce, signature FROM claim_signatures ORDER BY claim_id",
        "SELECT claim_id FROM claim_verdict_fresh ORDER BY claim_id",
    ]
    .iter()
    .map(|sql| {
        let mut statement = connection.prepare(sql).unwrap();
        let columns = statement.column_count();
        statement
            .query_map([], |row| {
                (0..columns)
                    .map(|index| row.get::<_, rusqlite::types::Value>(index))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    })
    .collect()
}

/// Each stored claim signature without its nonce and signature bytes, which every signing
/// makes afresh.
fn signers(connection: &Connection) -> Vec<(String, ClaimSignature)> {
    let mut statement = connection
        .prepare("SELECT claim_id, signature FROM claim_signatures ORDER BY claim_id")
        .unwrap();
    statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .map(|row| {
            let (claim_id, text) = row.unwrap();
            let mut signature: ClaimSignature = serde_json::from_str(&text).unwrap();
            signature.nonce.clear();
            signature.signature.clear();
            (claim_id, signature)
        })
        .collect()
}

/// The decoded payload of each envelope, by its claims' subjects.
fn payloads(connection: &Connection) -> BTreeMap<String, ReplicaEnvelopePayload> {
    let mut statement = connection
        .prepare("SELECT payload FROM replica_envelopes ORDER BY writer, sequence")
        .unwrap();
    statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(|payload| {
            let payload: ReplicaEnvelopePayload =
                ciborium::from_reader(payload.unwrap().as_slice()).unwrap();
            let subjects = payload
                .batch
                .claims
                .iter()
                .map(|claim| claim.subject.as_str())
                .collect::<Vec<_>>()
                .join(",");
            (subjects, payload)
        })
        .collect()
}

/// Check that every envelope this node sealed carries one signature, by `key` for `fleet_id`,
/// and return how many there are.
fn envelopes_signed_by(connection: &Connection, key: &MemberKey, fleet_id: &str) -> usize {
    let envelopes: usize = connection
        .query_row("SELECT COUNT(*) FROM replica_envelopes", [], |row| {
            row.get(0)
        })
        .unwrap();
    let mut statement = connection
        .prepare(
            "SELECT writer, sequence, envelope_hash, member_key, signature
             FROM replica_envelope_signatures",
        )
        .unwrap();
    let signatures = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    for (writer, sequence, hash, member_key, signature) in &signatures {
        assert_eq!(member_key, key.public());
        assert!(
            verify_signature(
                member_key,
                &envelope_signature_message(fleet_id, writer, *sequence, hash),
                signature
            ),
            "the signature of {writer}/{sequence} is for fleet {fleet_id}"
        );
    }
    assert_eq!(signatures.len(), envelopes);
    envelopes
}

#[test]
fn prepared_sealing_stores_exactly_what_the_old_sealing_stores() {
    let store = node("aspen");
    keyed(&store);
    let stored = store.put_blob(b"bytes a note names").unwrap();
    let missing = "ab".repeat(32);
    // More than one chunk of 64 batches.
    for index in 0..70 {
        note(&store, &format!("note/plain-{index}"), BTreeMap::new());
    }
    note(&store, "note/names-a-stored-blob", naming(&stored));
    note(&store, "note/names-a-missing-blob", naming(&missing));
    // A claim signature carries a fresh nonce; sign each claim once, before both passes, so
    // both seal the same bytes.
    let written = claims(&store.readers.get());
    {
        let connection = store.connection.write();
        for claim in &written {
            let signature = store.sign_unsealed(&unsealed(claim)).unwrap();
            principals::store_claim_signature_tx(&connection, &claim.id, &signature).unwrap();
        }
    }
    let old = sealed_by_the_old_passes(&store, sealed_rows);
    assert_eq!(
        seal(&store),
        0,
        "nothing changed between preparation and the writer"
    );
    let target = max_batch_rowid(&store.readers.get()).unwrap();
    assert_eq!(store.seeded_batch_rowid.load(Ordering::Acquire), target);
    let new = sealed_rows(&store.readers.get());
    assert_eq!(new[0].len(), 72, "one envelope per batch");
    assert_eq!(new[2].len(), 72, "one member signature per envelope");
    assert_eq!(new, old);
    let payloads = payloads(&store.readers.get());
    assert!(
        payloads["note/names-a-stored-blob"]
            .blobs
            .contains_key(&stored)
    );
    assert!(payloads["note/names-a-missing-blob"].blobs.is_empty());
}

#[test]
fn a_claim_signed_while_preparing_is_signed_as_the_old_sealing_signs_it() {
    let store = node("birch");
    let key = keyed(&store);
    for index in 0..3 {
        note(&store, &format!("note/unsigned-{index}"), BTreeMap::new());
    }
    let old = sealed_by_the_old_passes(&store, signers);
    assert_eq!(old.len(), 3);
    assert_eq!(seal(&store), 0);
    let reader = store.readers.get();
    assert_eq!(signers(&reader), old);
    for (subject, payload) in payloads(&reader) {
        let claim = &payload.batch.claims[0];
        let stored: String = reader
            .query_row(
                "SELECT signature FROM claim_signatures WHERE claim_id=?1",
                [&claim.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            payload.claim_signatures.get(&claim.id),
            Some(&serde_json::from_str::<ClaimSignature>(&stored).unwrap()),
            "{subject} carries the signature stored for it"
        );
        assert!(
            verifies(claim, &payload.claim_signatures[&claim.id]),
            "{subject}'s signature is genuine"
        );
    }
    assert_eq!(envelopes_signed_by(&reader, &key, FLEET), 3);
}

#[test]
fn a_named_blob_stored_or_dropped_after_preparation_is_sealed_as_the_store_holds_it() {
    for stored_late in [true, false] {
        let store = node("cedar");
        keyed(&store);
        let bytes = b"bytes that arrive or go".to_vec();
        let hash = hex::encode(Sha256::digest(&bytes));
        if !stored_late {
            // The note below promotes the upload into the shared table.
            store.put_blob(&bytes).unwrap();
        }
        note(&store, "note/names-a-blob", naming(&hash));
        let named = hash.clone();
        before_the_loan(move |store| {
            if stored_late {
                // Shared blobs arrive only with a claim: here a second note promotes it.
                let hash = store.put_blob(&bytes).unwrap();
                note(store, "note/stores-the-blob", naming(&hash));
            } else {
                store
                    .connection
                    .write()
                    .execute("DELETE FROM blobs WHERE hash=?1", [&named])
                    .unwrap();
            }
        });
        assert_eq!(seal(&store), 1, "stored_late={stored_late}");
        let payloads = payloads(&store.readers.get());
        let carried = payloads["note/names-a-blob"].blobs.get(&hash);
        if stored_late {
            assert_eq!(
                carried.map(Vec::as_slice),
                Some(&b"bytes that arrive or go"[..])
            );
        } else {
            assert!(carried.is_none());
        }
    }
}

#[test]
fn a_claim_signature_stored_after_preparation_is_the_one_sealed() {
    let store = node("dogwood");
    keyed(&store);
    let claim = note(&store, "note/signed-late", BTreeMap::new());
    // Signing again makes another nonce, so this differs from what preparation signs.
    let late = store.sign_unsealed(&unsealed(&claim)).unwrap();
    let (claim_id, signature) = (claim.id.clone(), late.clone());
    before_the_loan(move |store| {
        principals::store_claim_signature_tx(&store.connection.write(), &claim_id, &signature)
            .unwrap();
    });
    assert_eq!(seal(&store), 1);
    let reader = store.readers.get();
    let payloads = payloads(&reader);
    assert_eq!(
        payloads["note/signed-late"].claim_signatures.get(&claim.id),
        Some(&late)
    );
    let stored: String = reader
        .query_row(
            "SELECT signature FROM claim_signatures WHERE claim_id=?1",
            [&claim.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<ClaimSignature>(&stored).unwrap(),
        late
    );
}

#[test]
fn a_member_key_fleet_id_or_keyring_changed_after_preparation_is_signed_again() {
    for change in ["member key", "fleet id", "keyring"] {
        let store = node("elm");
        let first = keyed(&store);
        note(&store, "note/signed-envelope", BTreeMap::new());
        let second = Arc::new(MemberKey::generate().unwrap().0);
        let replacement = second.clone();
        before_the_loan(move |store| match change {
            // Only the member key: `load_member_key` would also replace the node key and move
            // the keyring's generation, which masks the member-key compare.
            "member key" => {
                *store
                    .member_key
                    .write()
                    .unwrap_or_else(PoisonError::into_inner) = Some(replacement);
            }
            "fleet id" => {
                store
                    .connection
                    .write()
                    .execute(
                        "UPDATE meta SET value=?1 WHERE key='fleet_id'",
                        [OTHER_FLEET],
                    )
                    .unwrap();
            }
            // Only the node key, which signs these claims; the member key stays.
            _ => store
                .keyring
                .set_node(Some(Arc::new(MemberKey::generate().unwrap().0))),
        });
        assert_eq!(seal(&store), 1, "{change}");
        let (key, fleet_id) = match change {
            "member key" => (&second, FLEET),
            "fleet id" => (&first, OTHER_FLEET),
            _ => (&first, FLEET),
        };
        assert_eq!(
            envelopes_signed_by(&store.readers.get(), key, fleet_id),
            1,
            "{change}"
        );
    }
}

#[test]
fn a_batch_sealed_by_another_pass_after_preparation_is_not_sealed_again() {
    let store = node("fir");
    let key = keyed(&store);
    for index in 0..3 {
        note(
            &store,
            &format!("note/sealed-elsewhere-{index}"),
            BTreeMap::new(),
        );
    }
    before_the_loan(|store| {
        let mut connection = store.connection.write();
        let transaction = connection.transaction().unwrap();
        seed_replica_envelopes_tx(&transaction, &store.origin, None).unwrap();
        transaction.commit().unwrap();
    });
    assert_eq!(seal(&store), 1);
    let reader = store.readers.get();
    assert_eq!(payloads(&reader).len(), 3, "one envelope per batch");
    // The other pass did not sign envelopes; sealing signs the envelopes it finds unsigned.
    assert_eq!(envelopes_signed_by(&reader, &key, FLEET), 3);
    assert_eq!(
        store.seeded_batch_rowid.load(Ordering::Acquire),
        max_batch_rowid(&reader).unwrap()
    );
}

#[test]
fn a_claim_deleted_after_preparation_is_not_sealed() {
    let store = node("hazel");
    keyed(&store);
    let dropped = note(&store, "note/deleted-before-the-loan", BTreeMap::new());
    note(&store, "note/kept", BTreeMap::new());
    let id = dropped.id.clone();
    before_the_loan(move |store| {
        store
            .connection
            .write()
            .execute("DELETE FROM claims WHERE id=?1", [&id])
            .unwrap();
    });
    assert_eq!(seal(&store), 1);
    let reader = store.readers.get();
    let payloads = payloads(&reader);
    // The batch keeps its header and is sealed without the claim, as the old sealing seals it.
    assert_eq!(
        payloads.keys().map(String::as_str).collect::<Vec<_>>(),
        ["", "note/kept"]
    );
    let records: usize = reader
        .query_row(
            "SELECT COUNT(*) FROM replica_records WHERE claim_id=?1",
            [&dropped.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(records, 0);
}

#[test]
fn an_own_envelope_found_unsigned_under_the_writer_is_prepared_again() {
    let store = node("ivy");
    let key = keyed(&store);
    let first = note(&store, "note/sealed-before", BTreeMap::new());
    note(&store, "note/sealed-now", BTreeMap::new());
    // Seal and sign the first batch without moving the frontier, so it lies inside the chunk.
    let first_rowid: i64 = store
        .readers
        .get()
        .query_row(
            "SELECT rowid FROM batches WHERE id=?1",
            [&first.batch_id],
            |row| row.get(0),
        )
        .unwrap();
    {
        let mut connection = store.connection.write();
        let transaction = connection.transaction().unwrap();
        seed_replica_envelopes_range_tx(&transaction, &store.origin, None, Some(first_rowid))
            .unwrap();
        assert_eq!(store.sign_own_envelopes_tx(&transaction, None).unwrap(), 1);
        transaction.commit().unwrap();
    }
    assert_eq!(store.seeded_batch_rowid.load(Ordering::Acquire), 0);
    // Preparation sees the first envelope signed and signs only the one it seals. Its signature
    // is gone by the loan, so the writer finds an unsigned envelope it holds no signature for.
    let batch_id = first.batch_id.clone();
    before_the_loan(move |store| {
        store
            .connection
            .write()
            .execute(
                "DELETE FROM replica_envelope_signatures WHERE envelope_hash=(
                     SELECT envelope_hash FROM replica_envelopes WHERE batch_id=?1)",
                [&batch_id],
            )
            .unwrap();
    });
    assert_eq!(seal(&store), 1);
    assert_eq!(envelopes_signed_by(&store.readers.get(), &key, FLEET), 2);
}

#[test]
fn a_claim_whose_signing_key_changes_after_preparation_is_signed_with_the_new_key() {
    let store = node("juniper");
    keyed(&store);
    // An agent's claim: the node mints the agent a key and grants it first.
    let claim = store
        .append_claim(&ClaimInput {
            subject: "note/by-an-agent".into(),
            kind: "example.note".into(),
            actor: Some("agent/scribe".into()),
            fields: BTreeMap::new(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let minted = store
        .keyring
        .signing("agent/scribe")
        .expect("the node holds a key for its agent");
    let replacement = Arc::new(MemberKey::generate().unwrap().0);
    let held = replacement.clone();
    before_the_loan(move |store| {
        store.keyring.insert(HeldKey {
            principal: "agent/scribe".into(),
            role: Role::Agent,
            key: held,
            chain: vec!["claim/replacement-grant".into()],
        });
    });
    assert_eq!(seal(&store), 1);
    let payloads = payloads(&store.readers.get());
    let signature = &payloads["note/by-an-agent"].claim_signatures[&claim.id];
    assert_ne!(signature.key, minted.key.public());
    assert_eq!(signature.key, replacement.public());
    assert_eq!(signature.signer, "agent/scribe");
    assert_eq!(signature.chain, ["claim/replacement-grant"]);
    assert!(verifies(&claim, signature));
}

#[test]
fn a_keyring_change_in_progress_at_the_loan_prepares_again() {
    let store = node("larch");
    keyed(&store);
    note(&store, "note/sealed-at-a-boundary", BTreeMap::new());
    let target = max_batch_rowid(&store.readers.get()).unwrap();
    let prepared = store.prepare_seal(target, usize::MAX).unwrap().unwrap();
    let mut connection = store.connection.write();
    let written = store.keyring.changing_while(|| {
        let transaction = connection.transaction().unwrap();
        store.seal_prepared_tx(&transaction, &prepared).unwrap()
    });
    assert!(!written, "keys being changed are not a settled keyring");
    let transaction = connection.transaction().unwrap();
    assert!(
        store.seal_prepared_tx(&transaction, &prepared).unwrap(),
        "once the change completes without touching these claims' keys, the chunk is current"
    );
}

thread_local! {
    static BYSTANDERS: Cell<usize> = const { Cell::new(0) };
}

/// Give the keyring a key for a principal no claim is signed as, and again before the next loan.
fn add_a_bystander_key(store: &Store) {
    let count = BYSTANDERS.with(|count| {
        count.set(count.get() + 1);
        count.get()
    });
    store.keyring.insert(HeldKey {
        principal: format!("agent/bystander-{count}"),
        role: Role::Agent,
        key: Arc::new(MemberKey::generate().unwrap().0),
        chain: Vec::new(),
    });
    before_the_loan(add_a_bystander_key);
}

#[test]
fn keys_added_for_other_principals_before_every_loan_do_not_hold_sealing_back() {
    let store = node("maple");
    keyed(&store);
    // Three chunks.
    for index in 0..130 {
        note(
            &store,
            &format!("note/among-bystanders-{index}"),
            BTreeMap::new(),
        );
    }
    BYSTANDERS.with(|count| count.set(0));
    before_the_loan(add_a_bystander_key);
    assert_eq!(seal(&store), 0);
    SEAL_BEFORE_LOAN.with(|hook| hook.borrow_mut().take());
    assert_eq!(
        BYSTANDERS.with(Cell::get),
        3,
        "a key arrived before each loan"
    );
    assert_eq!(
        store.seeded_batch_rowid.load(Ordering::Acquire),
        max_batch_rowid(&store.readers.get()).unwrap()
    );
}

#[test]
fn the_writer_selects_each_distinct_signer_once_however_many_claims_share_it() {
    let store = node("nutmeg");
    keyed(&store);
    for index in 0..20 {
        for actor in [None, Some("agent/left"), Some("agent/right")] {
            let subject = format!("note/shared-{index}-{}", actor.unwrap_or("node"));
            claim_by(&store, &subject, "example.note", actor, BTreeMap::new());
        }
    }
    // 60 notes and the node's grant of each agent's key: 62 batches, one chunk.
    assert_eq!(claims(&store.readers.get()).len(), 62);
    SIGNERS_SELECTED.with(|count| count.set(0));
    assert_eq!(seal(&store), 0);
    // The notes without an actor, each agent's notes, and the two grants, which the node issues
    // with its own key: four signer inputs, each selected once under the writer.
    assert_eq!(SIGNERS_SELECTED.with(Cell::get), 4);
    assert_eq!(payloads(&store.readers.get()).len(), 62);
}

#[test]
fn two_key_grants_by_one_actor_with_different_issuers_are_selected_apart() {
    let store = node("oak");
    let node_key = keyed(&store);
    claim_by(
        &store,
        "note/mints-the-agent-key",
        "example.note",
        Some("agent/scribe"),
        BTreeMap::new(),
    );
    let agent = store.keyring.signing("agent/scribe").unwrap();
    let grant = |issuer: &str, issuer_key: &str| {
        KeyGrant {
            key: MemberKey::generate().unwrap().0.public().to_owned(),
            role: Role::Agent,
            issuer: issuer.into(),
            issuer_key: issuer_key.into(),
            label: None,
        }
        .fields()
    };
    // Both grants are written by the node, so they share an actor and a kind; their issuers
    // differ, and so do their signers.
    let by_node = claim_by(
        &store,
        "agent/granted-by-the-node",
        KEY_GRANTED,
        Some("host/oak"),
        grant("host/oak", node_key.public()),
    );
    let by_agent = claim_by(
        &store,
        "agent/granted-by-the-agent",
        KEY_GRANTED,
        Some("host/oak"),
        grant("agent/scribe", agent.key.public()),
    );
    SIGNERS_SELECTED.with(|count| count.set(0));
    assert_eq!(seal(&store), 0);
    // The agent's note, the node's grants (the minted key's and `by_node`, the same input), and
    // `by_agent`.
    assert_eq!(SIGNERS_SELECTED.with(Cell::get), 3);
    let payloads = payloads(&store.readers.get());
    let first = &payloads["agent/granted-by-the-node"].claim_signatures[&by_node.id];
    assert_eq!(
        (first.key.as_str(), first.signer.as_str()),
        (node_key.public(), "host/oak")
    );
    assert!(verifies(&by_node, first));
    let second = &payloads["agent/granted-by-the-agent"].claim_signatures[&by_agent.id];
    assert_eq!(
        (second.key.as_str(), second.signer.as_str()),
        (agent.key.public(), "agent/scribe")
    );
    assert_eq!(second.chain, agent.chain);
    assert!(verifies(&by_agent, second));
}

#[test]
fn a_preparation_is_written_only_to_the_store_it_was_prepared_from() {
    let prepared_on = node("pine");
    keyed(&prepared_on);
    note(&prepared_on, "note/prepared-elsewhere", BTreeMap::new());
    let target = max_batch_rowid(&prepared_on.readers.get()).unwrap();
    let prepared = prepared_on
        .prepare_seal(target, usize::MAX)
        .unwrap()
        .unwrap();
    let other = node("quince");
    keyed(&other);
    let mut connection = other.connection.write();
    let transaction = connection.transaction().unwrap();
    assert!(!other.seal_prepared_tx(&transaction, &prepared).unwrap());
    drop(transaction);
    drop(connection);
    let mut connection = prepared_on.connection.write();
    let transaction = connection.transaction().unwrap();
    assert!(
        prepared_on
            .seal_prepared_tx(&transaction, &prepared)
            .unwrap()
    );
}

#[test]
fn preparation_keeps_a_chunk_within_its_byte_budget_and_a_larger_batch_alone() {
    let store = node("ginkgo");
    keyed(&store);
    for index in 0..3 {
        note(&store, &format!("note/budget-{index}"), BTreeMap::new());
    }
    let target = max_batch_rowid(&store.readers.get()).unwrap();
    let all = store.prepare_seal(target, usize::MAX).unwrap().unwrap();
    assert_eq!((all.batches.len(), all.through), (3, target));
    let sizes = all
        .batches
        .iter()
        .map(|sealed| sealed.payload.len())
        .collect::<Vec<_>>();
    // The batches kept within `budget`, and their payload bytes.
    let kept = |budget: usize| {
        let prepared = store.prepare_seal(target, budget).unwrap().unwrap();
        let last = prepared.batches.last().unwrap().batch.rowid;
        assert_eq!(
            prepared.through, last,
            "the frontier stops after the last batch kept"
        );
        let bytes = prepared
            .batches
            .iter()
            .map(|sealed| sealed.payload.len())
            .sum::<usize>();
        (prepared.batches.len(), bytes)
    };
    assert_eq!(kept(1), (1, sizes[0]), "a larger first batch alone");
    assert_eq!(kept(sizes[0] + sizes[1]), (2, sizes[0] + sizes[1]));
    assert_eq!(kept(sizes[0] + sizes[1] - 1), (1, sizes[0]));
    // Two batches leave room, but not for all of the third: it waits for the next chunk.
    assert_eq!(
        kept(sizes[0] + sizes[1] + sizes[2] / 2),
        (2, sizes[0] + sizes[1])
    );
    assert_eq!(seal(&store), 0);
    assert_eq!(payloads(&store.readers.get()).len(), 3);
}

#[test]
fn an_oversized_batch_after_a_small_one_waits_and_is_then_sealed_alone() {
    let store = node("hawthorn");
    keyed(&store);
    let large = store.put_blob(&[7_u8; 64 * 1024]).unwrap();
    note(&store, "note/small-before", BTreeMap::new());
    note(&store, "note/oversized", naming(&large));
    note(&store, "note/small-after", BTreeMap::new());
    let target = max_batch_rowid(&store.readers.get()).unwrap();
    let rowids = store
        .readers
        .get()
        .prepare("SELECT rowid FROM batches ORDER BY rowid")
        .unwrap()
        .query_map([], |row| row.get::<_, i64>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rowids.len(), 3);
    let all = store.prepare_seal(target, usize::MAX).unwrap().unwrap();
    let budget = all.batches[0].payload.len() * 2;
    assert!(all.batches[1].payload.len() > budget);
    let prepared = store.prepare_seal(target, budget).unwrap().unwrap();
    assert_eq!(prepared.batches.len(), 1, "the oversized batch waits");
    assert_eq!(prepared.through, rowids[0]);
    // Small, then oversized alone, then small: one chunk each.
    for through in &rowids {
        assert!(store.seal_local_batch_chunk_within(target, budget).unwrap());
        assert_eq!(store.seeded_batch_rowid.load(Ordering::Acquire), *through);
    }
    assert!(!store.seal_local_batch_chunk_within(target, budget).unwrap());
    let payloads = payloads(&store.readers.get());
    assert_eq!(payloads.len(), 3);
    assert!(payloads["note/oversized"].blobs.contains_key(&large));
}

// The sealing functions as they were before preparation, verbatim from main at 5bf3105b9
// (`seed_replica_envelopes_signed_tx`, and `Store::sign_own_envelopes_range_tx` with `self` as
// `store`).

#[allow(clippy::type_complexity)]
fn legacy_seed_replica_envelopes_signed_tx(
    transaction: &Transaction<'_>,
    relay: &str,
    after_rowid: Option<i64>,
    through_rowid: Option<i64>,
    sign: Option<&dyn Fn(&principals::Unsealed<'_>) -> Option<crate::principal::ClaimSignature>>,
) -> Result<()> {
    let order = if after_rowid.is_some() {
        "batches.rowid"
    } else {
        "origin, replica_sequence, id"
    };
    let mut statement = transaction.prepare(&format!(
        "SELECT id, origin, replica_sequence, previous_hash, hash, accepted_at_unix_ms
         FROM batches
         WHERE batches.rowid>=?1 AND batches.rowid<=?2
           AND NOT EXISTS (
             SELECT 1 FROM replica_envelopes WHERE replica_envelopes.batch_id=batches.id
         )
         ORDER BY {order}"
    ))?;
    let headers = statement
        .query_map(
            params![
                after_rowid.map_or(i64::MIN, |rowid| rowid.saturating_add(1)),
                through_rowid.unwrap_or(i64::MAX)
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for (id, writer, sequence, previous_hash, legacy_hash, accepted_at) in headers {
        let mut claim_statement = transaction.prepare(
            "SELECT id, store_index, batch_id, subject, kind, origin, actor, body, predecessors, accepted_at_unix_ms
             FROM claims WHERE batch_id=?1 ORDER BY store_index",
        )?;
        let claims = claim_statement
            .query_map([&id], claim_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        drop(claim_statement);
        let mut blobs = BTreeMap::new();
        collect_referenced_blobs(transaction, &claims, &mut blobs)?;
        let mut claim_signatures = BTreeMap::new();
        for claim in &claims {
            let existing = transaction
                .prepare_cached("SELECT signature FROM claim_signatures WHERE claim_id=?1")?
                .query_row([&claim.id], |row| row.get::<_, String>(0))
                .optional()?
                .and_then(|text| serde_json::from_str(&text).ok());
            let signature = match (existing, sign) {
                (Some(signature), _) => Some(signature),
                (None, Some(sign)) => sign(&principals::Unsealed {
                    subject: &claim.subject,
                    kind: &claim.kind,
                    actor: claim.actor.as_deref(),
                    body: &claim.body,
                    accepted_at_unix_ms: claim.accepted_at_unix_ms,
                }),
                (None, None) => None,
            };
            if let Some(signature) = signature {
                principals::store_claim_signature_tx(transaction, &claim.id, &signature)?;
                claim_signatures.insert(claim.id.clone(), signature);
            }
        }
        let batch = ReplicaBatch {
            id: id.clone(),
            origin: writer.clone(),
            replica_sequence: sequence,
            previous_hash: previous_hash.clone(),
            hash: legacy_hash,
            accepted_at_unix_ms: accepted_at.parse().unwrap_or_default(),
            claims: claims.clone(),
        };
        let mut payload = Vec::new();
        ciborium::into_writer(
            &ReplicaEnvelopePayload {
                batch,
                blobs,
                claim_signatures,
            },
            &mut payload,
        )?;
        let envelope_hash = replica_envelope_hash(
            &writer,
            sequence,
            previous_hash.as_deref(),
            accepted_at.parse().unwrap_or_default(),
            &payload,
        );
        transaction.execute(
            "INSERT OR IGNORE INTO replica_envelopes(
                 writer, sequence, envelope_hash, previous_hash, accepted_at_unix_ms,
                 payload, batch_id, relay, receipt_state, received_at_unix_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'validated', ?5)",
            params![
                writer,
                sequence,
                envelope_hash,
                previous_hash,
                accepted_at,
                payload,
                id,
                relay
            ],
        )?;
        for (position, claim) in claims.iter().enumerate() {
            let mut raw = Vec::new();
            ciborium::into_writer(claim, &mut raw)?;
            transaction.execute(
                "INSERT OR IGNORE INTO replica_records(
                     record_ref, writer, sequence, envelope_hash, position, raw, state,
                     claim_id, subject_hint, kind_hint, updated_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'valid', ?7, ?8, ?9, ?10)",
                params![
                    replica_record_ref(&writer, sequence, &envelope_hash, position as u64),
                    writer,
                    sequence,
                    envelope_hash,
                    position as u64,
                    raw,
                    claim.id,
                    claim.subject,
                    claim.kind,
                    accepted_at,
                ],
            )?;
        }
    }
    Ok(())
}

fn legacy_sign_own_envelopes_range_tx(
    store: &Store,
    transaction: &Transaction<'_>,
    after_batch_rowid: Option<i64>,
    through_batch_rowid: Option<i64>,
) -> Result<usize> {
    let _timing = time_stage(&store.replication_timers.signing);
    let Some(key) = store
        .member_key
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
    else {
        return Ok(0);
    };
    let Some(fleet_id) = fleet_meta(transaction, "fleet_id")? else {
        return Ok(0);
    };
    let from = if after_batch_rowid.is_some() {
        "FROM batches CROSS JOIN replica_envelopes AS envelopes
         ON envelopes.batch_id=batches.id WHERE batches.rowid>?2 AND batches.rowid<=?4 AND envelopes.writer=?1"
    } else {
        "FROM replica_envelopes AS envelopes WHERE envelopes.writer=?1 AND envelopes.rowid>?2 AND envelopes.rowid<=?4"
    };
    let mut statement = transaction.prepare(&format!(
        "SELECT envelopes.sequence, envelopes.envelope_hash {from}
           AND NOT EXISTS (
             SELECT 1 FROM replica_envelope_signatures AS signatures
             WHERE signatures.writer=envelopes.writer
               AND signatures.sequence=envelopes.sequence
               AND signatures.envelope_hash=envelopes.envelope_hash
               AND signatures.member_key=?3
           )
         ORDER BY envelopes.sequence"
    ))?;
    let unsigned = statement
        .query_map(
            params![
                store.origin,
                after_batch_rowid.unwrap_or(i64::MIN),
                key.public(),
                through_batch_rowid.unwrap_or(i64::MAX),
            ],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let now = now_ms().to_string();
    for (sequence, envelope_hash) in &unsigned {
        let signature = key.sign(&crate::fleet::envelope_signature_message(
            &fleet_id,
            &store.origin,
            *sequence,
            envelope_hash,
        ));
        transaction.execute(
            "INSERT OR IGNORE INTO replica_envelope_signatures(
                 writer, sequence, envelope_hash, member_key, signature, stored_at_unix_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                store.origin,
                sequence,
                envelope_hash,
                key.public(),
                signature,
                now
            ],
        )?;
    }
    Ok(unsigned.len())
}
