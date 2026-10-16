//! Signed claims and their verdicts, on the plain runtime: free mode, members verifying each
//! other, forged, replayed, swapped and revoked signatures, a tampered cache, a restart, and the
//! same verdicts whatever order claims arrive in.

use std::collections::BTreeMap;
use std::sync::Arc;

use rusqlite::OptionalExtension as _;
use serde_json::{Value, json};
use smallclaims::claim::{ClaimInput, ClaimRecord};
use smallclaims::fleet::MemberKey;
use smallclaims::principal::{ClaimSignature, KEY_GRANTED, KEY_REVOKED, Verdict, content_digest};
use smallclaims::store::Store;
use smallclaims::store::principals::store_claim_signature_tx;
use smallclaims::store::runtime::Plain;

const FLEET: &str = "6f1e8a52-3c4d-4b7e-9a10-2d5c8e7f9b31";

#[test]
fn signature_upgrade_uses_the_version_assigned_by_queue_order() {
    // The landed runtime-operation migration owns 19; signature migration takes 20.
    // Update this assertion only with the reviewed queue disposition.
    assert_eq!(st_storage_version::STORAGE_VERSION, 20);
}

fn key() -> Arc<MemberKey> {
    Arc::new(MemberKey::generate().unwrap().0)
}

fn node(name: &str, member: Option<&Arc<MemberKey>>, anchor: Option<&Arc<MemberKey>>) -> Store {
    let store = Store::open_memory(name, Arc::new(Plain)).unwrap();
    store.bind_fleet(FLEET).unwrap();
    if let Some(anchor) = anchor {
        store.pin_fleet_anchor(anchor.public()).unwrap();
    }
    if let Some(member) = member {
        store.set_member_key(Some(member.clone())).unwrap();
    }
    store
}

fn append(
    store: &Store,
    kind: &str,
    subject: &str,
    actor: Option<&str>,
    fields: Value,
) -> ClaimRecord {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: actor.map(str::to_owned),
            fields: serde_json::from_value(fields).unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
}

fn note(store: &Store, actor: Option<&str>, text: &str) -> ClaimRecord {
    append(
        store,
        "example.note",
        "note/plans",
        actor,
        json!({"text": text}),
    )
}

fn admit(by: &Store, name: &str, member: &MemberKey, via: &str) {
    let mut fields =
        json!({"fleet_id": FLEET, "member_key": member.public(), "via": via, "mode": "listening"});
    if via != "anchor" {
        fields["sponsor"] = json!(format!("host/{}", by.origin));
    }
    append(
        by,
        "fleet.member-admitted",
        &format!("host/{name}"),
        None,
        fields,
    );
}

/// The anchor `a` founds the fleet and admits each named member.
fn fleet(names: &[&str]) -> Vec<Store> {
    let anchor = key();
    let a = node("a", Some(&anchor), Some(&anchor));
    admit(&a, "a", &anchor, "anchor");
    let mut stores = vec![];
    for name in names {
        let member = key();
        admit(&a, name, &member, "invite");
        stores.push(node(name, Some(&member), Some(&anchor)));
    }
    for store in &stores {
        sync(&a, store);
    }
    let mut all = vec![a];
    all.extend(stores);
    all
}

fn sync(from: &Store, to: &Store) {
    let exchange = from
        .export_replication_exchange_answering(
            FLEET,
            &to.replication_inventory().unwrap(),
            &to.replication_signature_requests().unwrap(),
        )
        .unwrap();
    to.receive_replication_exchange(&from.origin, FLEET, &exchange)
        .unwrap();
    to.validate_replication_backlog().unwrap();
    to.project_replication_backlog().unwrap();
}

fn seal(store: &Store) {
    store.replication_snapshot().unwrap();
}

fn verdict(store: &Store, claim: &ClaimRecord) -> Verdict {
    store.claim_verdict(&claim.id).unwrap()
}

fn verdicts(store: &Store) -> BTreeMap<String, String> {
    let connection = store.readers.get();
    connection
        .prepare("SELECT claim_id, verdict FROM claim_verdicts")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Attach `signature` to a claim this node has not sealed yet, as a device would.
fn attach(store: &Store, claim: &ClaimRecord, signature: &ClaimSignature) {
    let connection = store.connection.write();
    store_claim_signature_tx(&connection, &claim.id, signature).unwrap();
}

#[test]
fn free_mode_signs_and_verifies_every_claim_with_no_fleet_and_no_prompt() {
    let store = Store::open_memory("studio", Arc::new(Plain)).unwrap();
    let before = note(
        &store,
        Some("person/ada"),
        "written before the node had a key",
    );
    seal(&store);
    store.set_node_key(key()).unwrap();

    let person = note(&store, Some("person/ada"), "from the CLI");
    let again = note(&store, Some("person/ada"), "again");
    let agent = note(&store, Some("agent/example/reviewer"), "from a seat");
    let daemon = note(&store, None, "from the daemon");
    let behalf = note(&store, Some("daemon/runtime"), "for a step");
    seal(&store);

    assert_eq!(verdict(&store, &before), Verdict::Unsigned);
    for claim in [&person, &again, &agent, &daemon, &behalf] {
        assert_eq!(verdict(&store, claim), Verdict::Verified, "{}", claim.body);
    }
    let person_signature = store.claim_signature(&person.id).unwrap().unwrap();
    assert_eq!(person_signature.signer, "person/ada");
    assert_eq!(
        person_signature.chain.len(),
        2,
        "device grant, then root grant"
    );
    assert_eq!(
        store.claim_signature(&again.id).unwrap().unwrap().key,
        person_signature.key,
        "one device key per person per machine"
    );
    let agent_signature = store.claim_signature(&agent.id).unwrap().unwrap();
    assert_eq!(agent_signature.signer, "agent/example/reviewer");
    assert_eq!(agent_signature.chain.len(), 1);
    let daemon_signature = store.claim_signature(&daemon.id).unwrap().unwrap();
    assert_eq!(daemon_signature.signer, "host/studio");
    assert_eq!(daemon_signature.on_behalf, None);
    let behalf_signature = store.claim_signature(&behalf.id).unwrap().unwrap();
    assert_eq!(behalf_signature.signer, "host/studio");
    assert_eq!(
        behalf_signature.on_behalf.as_deref(),
        Some("daemon/runtime")
    );

    // Two grants for the person (root, device) and one for the agent, all verified.
    let grants = store.claims_for("person/ada", Some(KEY_GRANTED)).unwrap();
    assert_eq!(grants.len(), 2);
    assert!(
        grants
            .iter()
            .all(|grant| verdict(&store, grant) == Verdict::Verified)
    );
    let counts = store.claim_verdict_counts().unwrap();
    assert_eq!(counts.get("invalid"), None);
    assert_eq!(counts.get("held"), None);
    assert!(store.recheck_claim_verdicts().unwrap().is_empty());
}

#[test]
fn members_verify_each_others_claims_through_membership() {
    let stores = fleet(&["b", "c"]);
    let (a, b, c) = (&stores[0], &stores[1], &stores[2]);
    let from_b = note(b, Some("person/ada"), "on b");
    let agent_b = note(b, Some("agent/example/reviewer"), "a seat on b");
    sync(b, a);
    sync(a, c);
    for store in [a, c] {
        assert_eq!(
            verdict(store, &from_b),
            Verdict::Verified,
            "on {}",
            store.origin
        );
        assert_eq!(
            verdict(store, &agent_b),
            Verdict::Verified,
            "on {}",
            store.origin
        );
    }
    // A store that does not know b's key holds b's claims until membership admits it.
    let stranger = node("stranger", Some(&key()), None);
    sync(b, &stranger);
    assert!(matches!(verdict(&stranger, &from_b), Verdict::Held(_)));
}

#[test]
fn a_forged_replayed_or_swapped_signature_is_invalid_and_the_genuine_one_stays_verified() {
    let stores = fleet(&["b"]);
    let (a, b) = (&stores[0], &stores[1]);
    let genuine = note(b, Some("person/ada"), "genuine");
    seal(b);
    let real = b.claim_signature(&genuine.id).unwrap().unwrap();

    // Forged: an unknown key claims to be ada's device under ada's real chain.
    let forged_claim = note(b, Some("person/ada"), "forged");
    let content = content_digest(
        &forged_claim.subject,
        &forged_claim.kind,
        forged_claim.actor.as_deref(),
        &forged_claim.body,
    );
    let forged = ClaimSignature::sign(&key(), &content, "person/ada", None, real.chain.clone(), 1);
    attach(b, &forged_claim, &forged);

    // Replayed: the same content and the genuine signature again.
    let replayed_claim = note(b, Some("person/ada"), "genuine");
    attach(b, &replayed_claim, &real);

    // Swapped: a genuine signature with its chain replaced.
    let swapped_claim = note(b, Some("person/ada"), "swapped");
    seal(b);
    let mut swapped = b.claim_signature(&swapped_claim.id).unwrap().unwrap();
    swapped.chain.reverse();
    let tampered = note(b, Some("person/ada"), "swapped");
    attach(b, &tampered, &swapped);

    sync(b, a);
    for store in [a, b] {
        assert_eq!(verdict(store, &genuine), Verdict::Verified);
        assert_eq!(verdict(store, &swapped_claim), Verdict::Verified);
        for claim in [&forged_claim, &replayed_claim, &tampered] {
            assert!(
                matches!(verdict(store, claim), Verdict::Invalid(_)),
                "{} on {}: {:?}",
                claim.body,
                store.origin,
                verdict(store, claim)
            );
        }
        assert!(store.recheck_claim_verdicts().unwrap().is_empty());
    }
}

struct SignatureCarriers {
    anchor: Arc<MemberKey>,
    baseline: smallclaims::replication::ReplicationExchange,
    exchange: smallclaims::replication::ReplicationExchange,
    claim: ClaimRecord,
    genuine: ClaimSignature,
}

fn signature_carriers(counterfeits: usize) -> SignatureCarriers {
    signature_candidates(counterfeits, 0)
}

fn signature_candidates(counterfeits: usize, alternatives: usize) -> SignatureCarriers {
    use smallclaims::claim::{ReplicaEnvelopePayload, ReplicaEnvelopeSignature};
    use smallclaims::replication::ReplicationInventory;

    let anchor = key();
    let source = node("alder", Some(&anchor), Some(&anchor));
    admit(&source, "alder", &anchor, "anchor");
    let baseline = source
        .export_replication_exchange(FLEET, &ReplicationInventory::default())
        .unwrap();
    let claim = note(&source, None, "One immutable claim, two carried signatures");
    let mut exchange = source
        .export_replication_exchange(FLEET, &baseline.inventory)
        .unwrap();
    assert_eq!(exchange.envelopes.len(), 1);
    let original = exchange.envelopes[0].clone();
    let mut payload: ReplicaEnvelopePayload =
        ciborium::from_reader(original.payload.bytes().unwrap()).unwrap();
    let genuine = payload.claim_signatures[&claim.id].clone();
    let mut expected = genuine.clone();
    let content = content_digest(
        &claim.subject,
        &claim.kind,
        claim.actor.as_deref(),
        &claim.body,
    );
    for index in 0..counterfeits + alternatives {
        let candidate = if index < counterfeits {
            let mut forged = genuine.clone();
            forged.signature = format!("invalid-signature-{index}");
            forged
        } else {
            let valid = ClaimSignature::sign(
                &anchor,
                &content,
                &genuine.signer,
                genuine.on_behalf.as_deref(),
                genuine.chain.clone(),
                genuine.signed_at_unix_ms,
            );
            if serde_json::to_string(&valid).unwrap() < serde_json::to_string(&expected).unwrap() {
                expected = valid.clone();
            }
            valid
        };
        payload.claim_signatures.insert(claim.id.clone(), candidate);
        let mut bytes = Vec::new();
        ciborium::into_writer(&payload, &mut bytes).unwrap();
        let mut fork = original.clone();
        fork.hash = smallclaims::hash::replica_envelope_hash(
            &fork.writer,
            fork.sequence,
            fork.previous_hash.as_deref(),
            fork.accepted_at_unix_ms,
            &bytes,
        );
        fork.payload = bytes.into();
        let fork_signature = ReplicaEnvelopeSignature {
            writer: fork.writer.clone(),
            sequence: fork.sequence,
            hash: fork.hash.clone(),
            member_key: anchor.public().into(),
            signature: anchor.sign(&smallclaims::fleet::envelope_signature_message(
                FLEET,
                &fork.writer,
                fork.sequence,
                &fork.hash,
            )),
        };
        exchange.envelopes.push(fork);
        exchange.signatures.push(fork_signature);
    }
    SignatureCarriers {
        anchor,
        baseline,
        exchange,
        claim,
        genuine: expected,
    }
}

fn push_signature_carrier(
    exchange: &mut smallclaims::replication::ReplicationExchange,
    original: &smallclaims::claim::ReplicaEnvelope,
    payload: &smallclaims::claim::ReplicaEnvelopePayload,
    anchor: &MemberKey,
) {
    use smallclaims::claim::ReplicaEnvelopeSignature;
    let mut bytes = Vec::new();
    ciborium::into_writer(payload, &mut bytes).unwrap();
    let mut fork = original.clone();
    fork.hash = smallclaims::hash::replica_envelope_hash(
        &fork.writer,
        fork.sequence,
        fork.previous_hash.as_deref(),
        fork.accepted_at_unix_ms,
        &bytes,
    );
    fork.payload = bytes.into();
    exchange.signatures.push(ReplicaEnvelopeSignature {
        writer: fork.writer.clone(),
        sequence: fork.sequence,
        hash: fork.hash.clone(),
        member_key: anchor.public().into(),
        signature: anchor.sign(&smallclaims::fleet::envelope_signature_message(
            FLEET,
            &fork.writer,
            fork.sequence,
            &fork.hash,
        )),
    });
    exchange.envelopes.push(fork);
}

fn receive_carriers(target: &Store, exchange: &smallclaims::replication::ReplicationExchange) {
    target
        .receive_replication_exchange("alder", FLEET, exchange)
        .unwrap();
    let admitted = target.validate_replication_backlog().unwrap();
    assert_eq!((admitted.invalid, admitted.held), (0, 0));
    target.project_replication_backlog().unwrap();
}

/// A writer can authenticate a carrier without holding the principal's device key.
fn untrusted_person_carriers() -> SignatureCarriers {
    use smallclaims::claim::ReplicaEnvelopePayload;
    use smallclaims::replication::ReplicationInventory;
    let anchor = key();
    let source = node("alder", Some(&anchor), Some(&anchor));
    admit(&source, "alder", &anchor, "anchor");
    source.ensure_principal_key("person/ada").unwrap();
    let baseline = source
        .export_replication_exchange(FLEET, &ReplicationInventory::default())
        .unwrap();
    let claim = note(
        &source,
        Some("person/ada"),
        "Genuine device authority survives untrusted carriers",
    );
    let mut exchange = source
        .export_replication_exchange(FLEET, &baseline.inventory)
        .unwrap();
    assert_eq!(exchange.envelopes.len(), 1);
    let original = exchange.envelopes[0].clone();
    let mut payload: ReplicaEnvelopePayload =
        ciborium::from_reader(original.payload.bytes().unwrap()).unwrap();
    let genuine = payload.claim_signatures[&claim.id].clone();
    let content = content_digest(
        &claim.subject,
        &claim.kind,
        claim.actor.as_deref(),
        &claim.body,
    );
    let attacker = ClaimSignature::sign(
        &key(),
        &content,
        "!untrusted",
        None,
        Vec::new(),
        genuine.signed_at_unix_ms,
    );
    let wrong_key = loop {
        let candidate = key();
        if candidate.public() < genuine.key.as_str() {
            break candidate;
        }
    };
    let same_principal = ClaimSignature::sign(
        &wrong_key,
        &content,
        "person/ada",
        None,
        genuine.chain.clone(),
        genuine.signed_at_unix_ms,
    );
    for candidate in [attacker, same_principal] {
        assert!(
            serde_json::to_string(&candidate).unwrap() < serde_json::to_string(&genuine).unwrap()
        );
        payload.claim_signatures.insert(claim.id.clone(), candidate);
        push_signature_carrier(&mut exchange, &original, &payload, &anchor);
    }
    SignatureCarriers {
        anchor,
        baseline,
        exchange,
        claim,
        genuine,
    }
}

#[test]
fn untrusted_carried_signature_cannot_displace_verified_person_in_any_schedule() {
    let fixture = untrusted_person_carriers();
    for reverse in [false, true] {
        for width in [1, 2, 3] {
            for copies in [1, 2, 3] {
                let target = node("birch", None, Some(&fixture.anchor));
                receive_carriers(&target, &fixture.baseline);
                let mut deliveries = fixture.exchange.envelopes.clone();
                if reverse {
                    deliveries.reverse();
                }
                let deliveries = deliveries
                    .into_iter()
                    .flat_map(|e| std::iter::repeat_n(e, copies))
                    .collect::<Vec<_>>();
                let mut saw_genuine = false;
                for chunk in deliveries.chunks(width) {
                    saw_genuine |= chunk
                        .iter()
                        .any(|e| e.hash == fixture.exchange.envelopes[0].hash);
                    let mut delivery = fixture.exchange.clone();
                    delivery.envelopes = chunk.to_vec();
                    receive_carriers(&target, &delivery);
                    if saw_genuine {
                        assert_eq!(
                            verdict(&target, &fixture.claim),
                            Verdict::Verified,
                            "reverse={reverse}, width={width}, copies={copies}"
                        );
                        assert_eq!(
                            target.claim_signature(&fixture.claim.id).unwrap(),
                            Some(fixture.genuine.clone())
                        );
                    }
                }
                assert!(target.recheck_claim_verdicts().unwrap().is_empty());
                assert_eq!(verdict(&target, &fixture.claim), Verdict::Verified);
            }
        }
    }
}

#[test]
fn late_root_rejudges_a_retained_nonwinning_candidate_in_every_schedule() {
    use smallclaims::claim::ReplicaEnvelopePayload;
    let fixture = untrusted_person_carriers();
    let ash = key();
    let original = &fixture.exchange.envelopes[0];
    let mut payload: ReplicaEnvelopePayload =
        ciborium::from_reader(original.payload.bytes().unwrap()).unwrap();
    let content = content_digest(
        &fixture.claim.subject,
        &fixture.claim.kind,
        fixture.claim.actor.as_deref(),
        &fixture.claim.body,
    );
    let late = ClaimSignature::sign(
        &ash,
        &content,
        "host/ash",
        Some("person/ada"),
        vec![],
        fixture.genuine.signed_at_unix_ms,
    );
    payload
        .claim_signatures
        .insert(fixture.claim.id.clone(), late.clone());
    let mut exchange = fixture.exchange.clone();
    push_signature_carrier(&mut exchange, original, &payload, &fixture.anchor);
    let sponsor = node("alder", Some(&fixture.anchor), Some(&fixture.anchor));
    receive_carriers(&sponsor, &fixture.baseline);
    admit(&sponsor, "ash", &ash, "invite");
    let roots = sponsor
        .export_replication_exchange(FLEET, &fixture.baseline.inventory)
        .unwrap();
    for reverse in [false, true] {
        for width in [1, 2, 4] {
            let target = node("birch", None, Some(&fixture.anchor));
            receive_carriers(&target, &fixture.baseline);
            let mut deliveries = exchange.envelopes.clone();
            if reverse {
                deliveries.reverse();
            }
            for chunk in deliveries.chunks(width) {
                let mut page = exchange.clone();
                page.envelopes = chunk.to_vec();
                receive_carriers(&target, &page);
            }
            assert_eq!(verdict(&target, &fixture.claim), Verdict::Verified);
            assert_eq!(
                target.claim_signature(&fixture.claim.id).unwrap(),
                Some(fixture.genuine.clone())
            );
            assert_eq!(
                target.signature_candidate_count(&fixture.claim.id).unwrap(),
                4
            );
            receive_carriers(&target, &roots);
            assert_eq!(verdict(&target, &fixture.claim), Verdict::Verified);
            assert_eq!(
                target.claim_signature(&fixture.claim.id).unwrap(),
                Some(late.clone())
            );
            assert!(target.recheck_claim_verdicts().unwrap().is_empty());
            receive_carriers(&target, &exchange);
            assert_eq!(
                target.claim_signature(&fixture.claim.id).unwrap(),
                Some(late.clone())
            );
        }
    }
}

#[test]
fn late_delegations_make_a_retained_person_signature_verified_after_split_delivery() {
    use smallclaims::claim::ReplicaEnvelopePayload;
    use smallclaims::replication::ReplicationInventory;
    let anchor = key();
    let source = node("alder", Some(&anchor), Some(&anchor));
    admit(&source, "alder", &anchor, "anchor");
    let baseline = source
        .export_replication_exchange(FLEET, &ReplicationInventory::default())
        .unwrap();
    let claim = note(
        &source,
        Some("person/ada"),
        "delegations may arrive after their signed claim",
    );
    let mut exchange = source
        .export_replication_exchange(FLEET, &baseline.inventory)
        .unwrap();
    let original = exchange
        .envelopes
        .iter()
        .find(|envelope| {
            let payload: ReplicaEnvelopePayload =
                ciborium::from_reader(envelope.payload.bytes().unwrap()).unwrap();
            payload.batch.claims.iter().any(|row| row.id == claim.id)
        })
        .unwrap()
        .clone();
    let mut payload: ReplicaEnvelopePayload =
        ciborium::from_reader(original.payload.bytes().unwrap()).unwrap();
    let genuine = payload.claim_signatures[&claim.id].clone();
    let attack = ClaimSignature::sign(
        &key(),
        &content_digest(
            &claim.subject,
            &claim.kind,
            claim.actor.as_deref(),
            &claim.body,
        ),
        "!untrusted",
        None,
        vec![],
        genuine.signed_at_unix_ms,
    );
    payload.claim_signatures.insert(claim.id.clone(), attack);
    push_signature_carrier(&mut exchange, &original, &payload, &anchor);
    let grants = exchange
        .envelopes
        .iter()
        .filter(|e| e.hash != original.hash && e.hash != exchange.envelopes.last().unwrap().hash)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(grants.len(), 2);
    for reverse in [false, true] {
        for width in [1, 2] {
            let target = node("birch", None, Some(&anchor));
            receive_carriers(&target, &baseline);
            let mut early = exchange.clone();
            early.envelopes = vec![original.clone(), exchange.envelopes.last().unwrap().clone()];
            if reverse {
                early.envelopes.reverse();
            }
            for chunk in early.envelopes.chunks(width) {
                let mut page = early.clone();
                page.envelopes = chunk.to_vec();
                receive_carriers(&target, &page);
            }
            assert!(
                matches!(verdict(&target,&claim),Verdict::Held(reason) if reason.contains("delegation"))
            );
            let mut late = exchange.clone();
            late.envelopes = grants.clone();
            if reverse {
                late.envelopes.reverse();
            }
            for chunk in late.envelopes.chunks(width) {
                let mut page = late.clone();
                page.envelopes = chunk.to_vec();
                receive_carriers(&target, &page);
            }
            assert_eq!(verdict(&target, &claim), Verdict::Verified);
            assert_eq!(
                target.claim_signature(&claim.id).unwrap(),
                Some(genuine.clone())
            );
            assert!(target.recheck_claim_verdicts().unwrap().is_empty());
        }
    }
}

#[test]
fn a_partial_envelope_cursor_cannot_authorize_a_claim_before_its_earlier_nonce_inventory() {
    nonce_inventory_transition(false, false, false);
}

#[test]
fn online_upgrade_serves_the_whole_legacy_model_then_atomically_activates_all_nonce_candidates() {
    nonce_inventory_transition(true, false, false);
}

#[test]
fn a_pinned_legacy_authorization_snapshot_survives_activation_and_retirement() {
    nonce_inventory_transition(true, true, false);
}

#[test]
fn removing_a_live_nonce_claim_rejudges_its_retained_dependents() {
    nonce_inventory_transition(false, false, true);
}

fn nonce_inventory_transition(online: bool, snapshot_overlap: bool, remove_earlier: bool) {
    use smallclaims::claim::ReplicaEnvelopePayload;
    use smallclaims::replication::ReplicationInventory;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    let anchor = key();
    let mut old_roots = BTreeMap::new();
    let source = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    source.bind_fleet(FLEET).unwrap();
    source.pin_fleet_anchor(anchor.public()).unwrap();
    source.set_member_key(Some(anchor.clone())).unwrap();
    source.set_write_clock_at(100).unwrap();
    admit(&source, "alder", &anchor, "anchor");
    source.set_write_clock_at(110).unwrap();
    let earlier = note(&source, None, "same signed content");
    let content = content_digest(
        &earlier.subject,
        &earlier.kind,
        earlier.actor.as_deref(),
        &earlier.body,
    );
    let a = ClaimSignature::sign(&anchor, &content, "host/alder", None, vec![], 110);
    let b = ClaimSignature::sign(&anchor, &content, "host/alder", None, vec![], 110);
    let (selected, hidden) =
        if serde_json::to_string(&a).unwrap() < serde_json::to_string(&b).unwrap() {
            (a, b)
        } else {
            (b, a)
        };
    attach(&source, &earlier, &selected);
    source.set_write_clock_at(120).unwrap();
    let later = note(&source, None, "same signed content");
    attach(&source, &later, &hidden);
    let exchange = source
        .export_replication_exchange(FLEET, &ReplicationInventory::default())
        .unwrap();
    assert_eq!(verdict(&source, &later), Verdict::Verified);
    let original = exchange
        .envelopes
        .iter()
        .find(|e| {
            let p: ReplicaEnvelopePayload =
                ciborium::from_reader(e.payload.bytes().unwrap()).unwrap();
            p.batch.claims.iter().any(|c| c.id == earlier.id)
        })
        .unwrap();
    let mut payload: ReplicaEnvelopePayload =
        ciborium::from_reader(original.payload.bytes().unwrap()).unwrap();
    payload
        .claim_signatures
        .insert(earlier.id.clone(), hidden.clone());
    let mut carriers = exchange.clone();
    carriers.envelopes.clear();
    carriers.signatures.clear();
    // Every padding fork and the final hidden candidate passes normal authenticated
    // admission with the original batch/writer/sequence intact. No synthetic accepted
    // row bypasses envelope/batch or immutable claim validation.
    for i in 0..66 {
        let mut carried = payload.clone();
        if i != 65 {
            let mut padding = selected.clone();
            padding.signature = format!("invalid-page-padding-{i}");
            carried.claim_signatures.insert(earlier.id.clone(), padding);
        }
        push_signature_carrier(&mut carriers, original, &carried, &anchor);
    }
    receive_carriers(&source, &carriers);
    assert!(
        matches!(verdict(&source,&later),Verdict::Invalid(reason) if reason.contains("earlier claim"))
    );
    {
        let connection = source.connection.write();
        // Reconstruct the reachable legacy first-arrival projection: earlier's first
        // candidate won, so its later non-winning nonce was not reserved; later was
        // cached Verified. Immutable admitted envelope and claim inputs stay intact.
        connection.execute("UPDATE claim_signatures SET signer=?1,key=?2,nonce=?3,signature=?4 WHERE claim_id=?5",rusqlite::params![selected.signer,selected.key,selected.nonce,serde_json::to_string(&selected).unwrap(),earlier.id]).unwrap();
        connection
            .execute(
                "UPDATE claim_verdicts SET verdict='verified',reason=NULL WHERE claim_id=?1",
                [&later.id],
            )
            .unwrap();
        if online {
            remove_candidate_schema(&connection);
            old_roots = principal_physical_roots(&connection, false);
        } else {
            connection.execute_batch("DELETE FROM claim_signature_candidates; DELETE FROM claim_signature_nonce_uses; DELETE FROM claim_signature_judging; DELETE FROM claim_signature_migration;").unwrap();
        }
        let cached: String = connection
            .query_row(
                "SELECT verdict FROM claim_verdicts WHERE claim_id=?1",
                [&later.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cached, "verified");
    }
    drop(source);
    let target = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    if online {
        assert_eq!(
            principal_physical_roots(&target.readers.get(), true),
            old_roots,
            "rename retains every old table/index root page"
        );
        assert_eq!(old_roots.len(), 10);
        assert_eq!(target.runtime.schema_digest(), "plain");
        assert_eq!(
            target
                .readers
                .get()
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            st_storage_version::STORAGE_VERSION
        );
    }
    while target.signature_migration().unwrap().phase == "legacy" {
        target.advance_signature_migration().unwrap();
    }
    let first = target.advance_signature_migration().unwrap();
    assert_eq!(first.phase, "envelopes");
    assert!(first.cursor > 0 && first.cursor < first.envelopes_through);
    if online {
        assert!(first.serving_legacy);
        assert_eq!(verdict(&target, &later), Verdict::Verified);
        assert_eq!(
            target.claim_signature(&earlier.id).unwrap(),
            Some(selected.clone())
        );
        // The entire old model remains live through a corrupt/stopped shadow job and restart.
        target
            .fail_signature_migration("test stopped shadow inventory")
            .unwrap();
        assert_eq!(verdict(&target, &later), Verdict::Verified);
        let cursor = target.signature_migration().unwrap().cursor;
        drop(target);
        let restarted = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
        assert!(restarted.signature_migration().unwrap().serving_legacy);
        assert_eq!(restarted.signature_migration().unwrap().cursor, cursor);
        assert_eq!(verdict(&restarted, &later), Verdict::Verified);
        restarted.resume_signature_migration().unwrap();
        if snapshot_overlap {
            use smallclaims::store::principals::SignatureModel;
            restarted.read_snapshot(|_| {
                let before=restarted.claim_authorization(&later.id).unwrap();
                assert_eq!(before.model,SignatureModel::Legacy);
                assert_eq!(before.verdict,Verdict::Verified);
                std::thread::scope(|scope| {
                    scope.spawn(|| { finish_signature_migration(&restarted); while restarted.retire_legacy_signature_page().unwrap() {} }).join().unwrap();
                });
                let after=restarted.claim_authorization(&later.id).unwrap();
                assert_eq!(after.model,SignatureModel::Legacy);
                assert_eq!(after.verdict,Verdict::Verified,"a pinned reader sees the complete old model after another thread activates and retires it");
                assert_eq!(before.signature,after.signature);
                Ok(())
            }).unwrap();
            let current = restarted.claim_authorization(&later.id).unwrap();
            assert_eq!(current.model, SignatureModel::Candidate);
            assert!(
                matches!(current.verdict,Verdict::Invalid(reason) if reason.contains("earlier claim"))
            );
            assert!(restarted.recheck_claim_verdicts().unwrap().is_empty());
            return;
        }
        finish_signature_migration(&restarted);
        assert!(!restarted.signature_migration().unwrap().serving_legacy);
        assert!(
            matches!(verdict(&restarted,&later),Verdict::Invalid(reason) if reason.contains("earlier claim"))
        );
        let old: String = restarted
            .readers
            .get()
            .query_row(
                "SELECT verdict FROM legacy_claim_verdicts WHERE claim_id=?1",
                [&later.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            old, "verified",
            "activation switches models rather than mutating the legacy cache"
        );
        restarted
            .fail_signature_migration("test corruption after activation")
            .unwrap();
        assert!(
            matches!(verdict(&restarted, &later), Verdict::Held(_)),
            "never fall back to the still-present legacy Verified cache after activation"
        );
        restarted.resume_signature_migration().unwrap();
        // A single corrupted serving bit must not reactivate the old Verified cache.
        restarted
            .connection
            .write()
            .execute(
                "UPDATE claim_signature_migration SET serving_legacy=1 WHERE id=1",
                [],
            )
            .unwrap();
        assert!(
            restarted
                .claim_verdict(&later.id)
                .unwrap_err()
                .to_string()
                .contains("completed bootstrap cannot serve legacy")
        );
        restarted
            .connection
            .write()
            .execute(
                "UPDATE claim_signature_migration SET serving_legacy=0 WHERE id=1",
                [],
            )
            .unwrap();
        let mut pages = 0;
        while restarted.retire_legacy_signature_page().unwrap() {
            pages += 1;
        }
        assert!(pages > 0);
        assert!(
            !restarted
                .readers
                .get()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name LIKE 'legacy_claim_%')",
                    [],
                    |row| row.get::<_, bool>(0)
                )
                .unwrap()
        );
        assert!(
            matches!(verdict(&restarted,&later),Verdict::Invalid(reason) if reason.contains("earlier claim"))
        );
        assert!(restarted.recheck_claim_verdicts().unwrap().is_empty());
        return;
    }
    assert!(
        matches!(verdict(&target,&later),Verdict::Held(reason) if reason.starts_with("signature-migration-incomplete:"))
    );
    assert!(!target.readers.get().query_row("SELECT EXISTS(SELECT 1 FROM claim_signature_nonce_uses WHERE claim_id=?1 AND nonce=?2)",rusqlite::params![earlier.id,hidden.nonce],|row|row.get::<_,bool>(0)).unwrap());
    finish_signature_migration(&target);
    assert_eq!(target.signature_candidate_count(&earlier.id).unwrap(), 67);
    assert!(
        matches!(verdict(&target,&later),Verdict::Invalid(reason) if reason.contains("earlier claim"))
    );
    if remove_earlier {
        use smallclaims::store::checkpoint::{
            ClaimTombstone, delete_dropped_rows_tx, record_checkpoint_tombstones_tx,
        };
        let mut connection = target.connection.write();
        let tx = connection.transaction().unwrap();
        let (writer,sequence,hash)=tx.query_row("SELECT writer,sequence,envelope_hash FROM replica_envelopes WHERE batch_id=?1 ORDER BY rowid LIMIT 1",[&earlier.batch_id],|row|Ok((row.get::<_,String>(0)?,row.get::<_,u64>(1)?,row.get::<_,String>(2)?))).unwrap();
        let tombstone = ClaimTombstone {
            id: earlier.id.clone(),
            writer,
            sequence,
            envelope_hash: hash,
            subject: earlier.subject.clone(),
            kind: earlier.kind.clone(),
            actor: earlier.actor.clone(),
            predecessors: earlier.predecessors.clone(),
            operation_id: earlier.operation_id.clone(),
            request_digest: earlier.request_digest.clone(),
            accepted_at_unix_ms: earlier.accepted_at_unix_ms,
        };
        // Exercise the existing deletion-maintenance hook, not removal-family qualification.
        record_checkpoint_tombstones_tx(
            &tx,
            "maintenance-fixture",
            &[],
            std::slice::from_ref(&tombstone),
        )
        .unwrap();
        delete_dropped_rows_tx(&tx, &[], &[tombstone]).unwrap();
        tx.commit().unwrap();
        drop(connection);
        target.judge_claims(true).unwrap();
        assert_eq!(target.signature_candidate_count(&earlier.id).unwrap(), 0);
        assert_eq!(
            verdict(&target, &later),
            Verdict::Verified,
            "removing the only earlier live nonce use must invalidate the retained cached verdict"
        );
    }
    assert!(target.recheck_claim_verdicts().unwrap().is_empty());
}

#[test]
fn first_open_untrusted_carrier_cannot_displace_verified_person() {
    let fixture = untrusted_person_carriers();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    {
        let store = Store::open(&path, "birch", Arc::new(Plain)).unwrap();
        store.bind_fleet(FLEET).unwrap();
        store.pin_fleet_anchor(fixture.anchor.public()).unwrap();
        receive_carriers(&store, &fixture.baseline);
        receive_carriers(&store, &fixture.exchange);
        let connection = store.connection.write();
        // Model an older node that had kept the genuine candidate before this upgrade.
        connection.execute("UPDATE claim_signatures SET signer=?1,key=?2,nonce=?3,signature=?4 WHERE claim_id=?5", rusqlite::params![fixture.genuine.signer, fixture.genuine.key, fixture.genuine.nonce, serde_json::to_string(&fixture.genuine).unwrap(), fixture.claim.id]).unwrap();
        connection
            .execute(
                "UPDATE claim_verdicts SET verdict='verified', reason=NULL WHERE claim_id=?1",
                [&fixture.claim.id],
            )
            .unwrap();
        connection
            .execute_batch(
                "DELETE FROM claim_signature_candidates; DELETE FROM claim_signature_migration;",
            )
            .unwrap();
        connection
            .execute(
                "DELETE FROM replica_records WHERE claim_id=?1",
                [&fixture.claim.id],
            )
            .unwrap();
    }
    let store = Store::open(&path, "birch", Arc::new(Plain)).unwrap();
    assert!(
        matches!(verdict(&store,&fixture.claim),Verdict::Held(reason) if reason.starts_with("signature-migration-incomplete:"))
    );
    finish_signature_migration(&store);
    assert_eq!(verdict(&store, &fixture.claim), Verdict::Verified);
    assert_eq!(
        store.claim_signature(&fixture.claim.id).unwrap(),
        Some(fixture.genuine)
    );
}

fn assert_candidate_inventory(target: &Store, fixture: &SignatureCarriers) {
    use smallclaims::claim::ReplicaEnvelopePayload;
    use std::collections::BTreeSet;
    let fields = fixture.claim.body.get("fields").unwrap_or(&Value::Null);
    let judged = smallclaims::principal::Judged {
        id: &fixture.claim.id,
        subject: &fixture.claim.subject,
        kind: &fixture.claim.kind,
        actor: fixture.claim.actor.as_deref(),
        content: content_digest(
            &fixture.claim.subject,
            &fixture.claim.kind,
            fixture.claim.actor.as_deref(),
            &fixture.claim.body,
        ),
        fields,
    };
    let mut expected_uses = BTreeSet::new();
    let mut expected = BTreeMap::<String, (String, String, BTreeSet<String>)>::new();
    for envelope in &fixture.exchange.envelopes {
        let payload: ReplicaEnvelopePayload =
            ciborium::from_reader(envelope.payload.bytes().unwrap()).unwrap();
        let signature = &payload.claim_signatures[&fixture.claim.id];
        if signature.verifies(&judged) {
            expected_uses.insert((signature.key.clone(), signature.nonce.clone()));
        }
        let entry = expected
            .entry(serde_json::to_string(signature).unwrap())
            .or_insert_with(|| {
                (
                    signature.key.clone(),
                    signature.nonce.clone(),
                    BTreeSet::new(),
                )
            });
        entry.2.insert(envelope.writer.clone());
    }
    let connection = target.readers.get();
    let rows=connection.prepare("SELECT signature,key,nonce FROM claim_signature_candidates WHERE claim_id=?1 ORDER BY signature").unwrap()
        .query_map([&fixture.claim.id],|row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    let mut actual = BTreeMap::new();
    for (signature, key, nonce) in rows {
        let writers = connection
            .prepare(
                "SELECT writer FROM claim_signature_sources WHERE claim_id=?1 AND signature=?2",
            )
            .unwrap()
            .query_map(rusqlite::params![fixture.claim.id, signature], |row| {
                row.get::<_, String>(0)
            })
            .unwrap()
            .collect::<rusqlite::Result<BTreeSet<_>>>()
            .unwrap();
        actual.insert(signature, (key, nonce, writers));
    }
    let actual_uses = connection
        .prepare("SELECT key,nonce FROM claim_signature_nonce_uses WHERE claim_id=?1")
        .unwrap()
        .query_map([&fixture.claim.id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<BTreeSet<_>>>()
        .unwrap();
    assert_eq!(actual_uses, expected_uses);
    assert_eq!(actual, expected);
    assert_eq!(
        target.signature_candidate_count(&fixture.claim.id).unwrap(),
        expected.len() as u64
    );
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(32))]
    #[test]
    fn untrusted_candidates_cannot_downgrade_verified_under_generated_delivery(
        ranks in proptest::collection::vec(proptest::prelude::any::<u16>(),3),
        width in 1usize..7,copies in 1usize..4,
    ) {
        let fixture=untrusted_person_carriers();
        let target=node("birch",None,Some(&fixture.anchor));
        receive_carriers(&target,&fixture.baseline);
        let mut order=(0..fixture.exchange.envelopes.len()).collect::<Vec<_>>();
        order.sort_by_key(|index|(ranks[*index],*index));
        let deliveries=order.into_iter().flat_map(|index|std::iter::repeat_n(fixture.exchange.envelopes[index].clone(),copies)).collect::<Vec<_>>();
        let mut saw_genuine=false;
        for chunk in deliveries.chunks(width) {
            saw_genuine|=chunk.iter().any(|e|e.hash==fixture.exchange.envelopes[0].hash);
            let mut page=fixture.exchange.clone();page.envelopes=chunk.to_vec();receive_carriers(&target,&page);
            if saw_genuine { proptest::prop_assert_eq!(verdict(&target,&fixture.claim),Verdict::Verified); }
        }
        assert_candidate_inventory(&target,&fixture);
        receive_carriers(&target,&fixture.exchange);
        assert_candidate_inventory(&target,&fixture);
        proptest::prop_assert_eq!(target.claim_signature(&fixture.claim.id).unwrap(),Some(fixture.genuine));
        proptest::prop_assert!(target.recheck_claim_verdicts().unwrap().is_empty());
    }
}

fn finish_signature_migration(store: &Store) {
    loop {
        let report = store.advance_signature_migration().unwrap();
        assert!(report.fault.is_none(), "{report:?}");
        if report.phase == "judging" {
            store.advance_signature_judgment().unwrap();
        }
        if store.signature_migration().unwrap().done() {
            break;
        }
    }
}

#[test]
fn competing_carried_signatures_have_the_same_projection_in_every_delivery_schedule() {
    let fixture = signature_carriers(1);
    for reverse in [false, true] {
        for width in [1, 2, 3] {
            for copies in [1, 2, 3] {
                let target = node("birch", None, Some(&fixture.anchor));
                receive_carriers(&target, &fixture.baseline);
                let original = &fixture.exchange.envelopes[0];
                let fork = &fixture.exchange.envelopes[1];
                let order = if reverse {
                    [fork, original]
                } else {
                    [original, fork]
                };
                let deliveries = order
                    .into_iter()
                    .flat_map(|e| std::iter::repeat_n(e.clone(), copies))
                    .collect::<Vec<_>>();
                for chunk in deliveries.chunks(width) {
                    let mut delivery = fixture.exchange.clone();
                    delivery.envelopes = chunk.to_vec();
                    receive_carriers(&target, &delivery);
                }
                let projected = (
                    target.claim_signature(&fixture.claim.id).unwrap(),
                    verdict(&target, &fixture.claim),
                );
                assert_eq!(
                    projected,
                    (Some(fixture.genuine.clone()), Verdict::Verified),
                    "reverse={reverse}, width={width}, copies={copies}"
                );
                assert!(target.recheck_claim_verdicts().unwrap().is_empty());
                assert_eq!(
                    (
                        target.claim_signature(&fixture.claim.id).unwrap(),
                        verdict(&target, &fixture.claim)
                    ),
                    projected
                );
            }
        }
    }
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(32))]
    #[test]
    fn signature_projection_is_pure_with_shuffles_splits_and_duplicates(
        count in 1usize..4, alternatives in 0usize..4,
        ranks in proptest::collection::vec(proptest::prelude::any::<u16>(), 8),
        width in 1usize..9, copies in 1usize..4,
    ) {
        let fixture = signature_candidates(count, alternatives);
        let target = node("birch", None, Some(&fixture.anchor));
        receive_carriers(&target, &fixture.baseline);
        let mut order = (0..fixture.exchange.envelopes.len()).collect::<Vec<_>>();
        order.sort_by_key(|index| (ranks[*index], *index));
        let deliveries = order.into_iter().flat_map(|index| std::iter::repeat_n(fixture.exchange.envelopes[index].clone(), copies)).collect::<Vec<_>>();
        for chunk in deliveries.chunks(width) {
            let mut delivery = fixture.exchange.clone();
            delivery.envelopes = chunk.to_vec();
            receive_carriers(&target, &delivery);
        }
        let expected = (Some(fixture.genuine.clone()), Verdict::Verified);
        assert_candidate_inventory(&target,&fixture);
        proptest::prop_assert_eq!((target.claim_signature(&fixture.claim.id).unwrap(), verdict(&target, &fixture.claim)), expected.clone());
        receive_carriers(&target, &fixture.exchange);
        proptest::prop_assert_eq!((target.claim_signature(&fixture.claim.id).unwrap(), verdict(&target, &fixture.claim)), expected);
        proptest::prop_assert!(target.recheck_claim_verdicts().unwrap().is_empty());
    }
}

#[test]
fn competing_carriers_cannot_revive_a_checkpointed_signature() {
    use smallclaims::store::checkpoint::{
        ClaimTombstone, delete_dropped_rows_tx, record_checkpoint_tombstones_tx,
    };
    let fixture = signature_candidates(1, 1);
    let original = &fixture.exchange.envelopes[0];
    let tombstone = ClaimTombstone {
        id: fixture.claim.id.clone(),
        writer: original.writer.clone(),
        sequence: original.sequence,
        envelope_hash: original.hash.clone(),
        subject: fixture.claim.subject.clone(),
        kind: fixture.claim.kind.clone(),
        actor: fixture.claim.actor.clone(),
        predecessors: fixture.claim.predecessors.clone(),
        operation_id: None,
        request_digest: None,
        accepted_at_unix_ms: fixture.claim.accepted_at_unix_ms,
    };
    for reverse in [false, true] {
        for width in [1, 2] {
            for copies in [1, 2] {
                let target = node("birch", None, Some(&fixture.anchor));
                receive_carriers(&target, &fixture.baseline);
                let mut first = fixture.exchange.clone();
                first.envelopes = vec![original.clone()];
                receive_carriers(&target, &first);
                {
                    let mut connection = target.connection.write();
                    let tx = connection.transaction().unwrap();
                    record_checkpoint_tombstones_tx(
                        &tx,
                        "checkpoint/fixture",
                        &[],
                        std::slice::from_ref(&tombstone),
                    )
                    .unwrap();
                    delete_dropped_rows_tx(&tx, &[], std::slice::from_ref(&tombstone)).unwrap();
                    tx.commit().unwrap();
                }
                let order = if reverse { [2, 1] } else { [1, 2] };
                let deliveries = order
                    .into_iter()
                    .flat_map(|i| {
                        std::iter::repeat_n(fixture.exchange.envelopes[i].clone(), copies)
                    })
                    .collect::<Vec<_>>();
                for chunk in deliveries.chunks(width) {
                    let mut delivery = fixture.exchange.clone();
                    delivery.envelopes = chunk.to_vec();
                    receive_carriers(&target, &delivery);
                }
                assert_eq!(
                    target.claim_signature(&fixture.claim.id).unwrap(),
                    None,
                    "reverse={reverse}, width={width}, copies={copies}"
                );
                assert_eq!(verdict(&target, &fixture.claim), Verdict::Unsigned);
                assert!(target.recheck_claim_verdicts().unwrap().is_empty());
            }
        }
    }
}

#[test]
fn adding_a_signature_preserves_both_valid_nonce_uses_atomically() {
    let anchor = key();
    let store = node("alder", Some(&anchor), Some(&anchor));
    store.set_write_clock_at(100).unwrap();
    admit(&store, "alder", &anchor, "anchor");
    store.set_write_clock_at(110).unwrap();
    let first = note(&store, None, "same signed content");
    let content = content_digest(
        &first.subject,
        &first.kind,
        first.actor.as_deref(),
        &first.body,
    );
    let a = ClaimSignature::sign(&anchor, &content, "host/alder", None, vec![], 110);
    let b = ClaimSignature::sign(&anchor, &content, "host/alder", None, vec![], 110);
    let (new, old) = if serde_json::to_string(&a).unwrap() < serde_json::to_string(&b).unwrap() {
        (a, b)
    } else {
        (b, a)
    };
    attach(&store, &first, &old);
    store.set_write_clock_at(120).unwrap();
    let old_nonce = note(&store, None, "same signed content");
    attach(&store, &old_nonce, &old);
    store.set_write_clock_at(130).unwrap();
    let new_nonce = note(&store, None, "same signed content");
    attach(&store, &new_nonce, &new);
    store.judge_claims(true).unwrap();
    assert_eq!(verdict(&store, &first), Verdict::Verified);
    assert!(matches!(verdict(&store, &old_nonce), Verdict::Invalid(_)));
    assert_eq!(verdict(&store, &new_nonce), Verdict::Verified);
    {
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        store_claim_signature_tx(&tx, &first.id, &new).unwrap();
        tx.rollback().unwrap();
    }
    assert_eq!(store.claim_signature(&first.id).unwrap(), Some(old));
    assert_eq!(store.judge_claims(true).unwrap(), 0);
    assert!(matches!(verdict(&store, &old_nonce), Verdict::Invalid(_)));
    assert_eq!(verdict(&store, &new_nonce), Verdict::Verified);
    attach(&store, &first, &new);
    store.judge_claims(true).unwrap();
    assert_eq!(verdict(&store, &first), Verdict::Verified);
    // A non-winning valid candidate is still signing use; neither nonce was superseded.
    assert!(matches!(verdict(&store, &old_nonce), Verdict::Invalid(_)));
    assert!(matches!(verdict(&store, &new_nonce), Verdict::Invalid(_)));
    assert!(store.recheck_claim_verdicts().unwrap().is_empty());
}

#[test]
fn opening_an_old_store_recovers_signatures_resumes_and_exposes_corruption() {
    let fixture = signature_carriers(3);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    let mut forged = fixture.genuine.clone();
    forged.signature = "invalid-signature-2".into();
    let forged_text = serde_json::to_string(&forged).unwrap();
    {
        let store = Store::open(&path, "birch", Arc::new(Plain)).unwrap();
        store.bind_fleet(FLEET).unwrap();
        store.pin_fleet_anchor(fixture.anchor.public()).unwrap();
        receive_carriers(&store, &fixture.baseline);
        receive_carriers(&store, &fixture.exchange);
        let connection = store.connection.write();
        store_claim_signature_tx(&connection, "old-gone-claim", &forged).unwrap();
        connection
            .execute(
                "UPDATE claim_signatures SET signature=?1 WHERE claim_id=?2",
                rusqlite::params![forged_text, fixture.claim.id],
            )
            .unwrap();
        connection.execute("UPDATE claim_verdicts SET verdict='invalid',reason='invalid signature' WHERE claim_id=?1",[&fixture.claim.id]).unwrap();
        connection
            .execute(
                "DELETE FROM replica_records WHERE claim_id=?1",
                [&fixture.claim.id],
            )
            .unwrap();
        connection
            .execute_batch(
                "DELETE FROM claim_signature_candidates; DELETE FROM claim_signature_migration;",
            )
            .unwrap();
        // Both old base64 TEXT and new BLOB payloads must be decoded through the same codec.
        let first = fixture.exchange.envelopes[0].payload.base64();
        connection
            .execute(
                "UPDATE replica_envelopes SET payload=?1 WHERE envelope_hash=?2",
                rusqlite::params![first, fixture.exchange.envelopes[0].hash],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE replica_envelopes SET receipt_state='degraded' WHERE envelope_hash=?1",
                [&fixture.exchange.envelopes[0].hash],
            )
            .unwrap();
        // Rejected payloads with no admitted batch are outside the candidate inventory.
        connection.execute("INSERT INTO replica_envelopes(writer,sequence,envelope_hash,accepted_at_unix_ms,payload,relay,receipt_state,received_at_unix_ms) VALUES ('rejected-fixture',1,'rejected-fixture','1',x'ff','fixture','degraded','1')",[]).unwrap();
        connection.execute("INSERT INTO replica_envelopes(writer,sequence,envelope_hash,accepted_at_unix_ms,payload,relay,receipt_state,received_at_unix_ms) VALUES ('corrupt-fixture',1,'corrupt-fixture','1',x'ff','fixture','validated','1')",[]).unwrap();
    }
    let store = Store::open(&path, "birch", Arc::new(Plain)).unwrap();
    assert!(
        matches!(verdict(&store,&fixture.claim),Verdict::Held(reason) if reason.starts_with("signature-migration-incomplete:"))
    );
    assert_eq!(store.judge_claims(true).unwrap(), 0);
    assert!(
        store
            .claim_signature(&fixture.claim.id)
            .unwrap_err()
            .to_string()
            .starts_with("signature-migration-incomplete:")
    );
    assert!(
        store
            .signature_nonce_used(&fixture.genuine.key, &fixture.genuine.nonce)
            .unwrap_err()
            .to_string()
            .starts_with("signature-migration-incomplete:")
    );
    assert!(
        store.claim_by_id(&fixture.claim.id).unwrap().is_some(),
        "ordinary reads still serve"
    );
    let page = store.advance_signature_migration().unwrap();
    assert_eq!(page.phase, "legacy");
    assert!(page.cursor > 0);
    let cursor = page.cursor;
    drop(store);
    let store = Store::open(&path, "birch", Arc::new(Plain)).unwrap();
    assert_eq!(store.signature_migration().unwrap().cursor, cursor);
    loop {
        let page = store.advance_signature_migration().unwrap();
        if page.fault.is_some() {
            break;
        }
        assert!(!page.done());
    }
    let fault = store.signature_migration().unwrap();
    assert!(fault.fault.as_deref().unwrap().contains("corrupt-fixture"));
    assert!(
        matches!(verdict(&store,&fixture.claim),Verdict::Held(reason) if reason.starts_with("signature-state-corrupt:"))
    );
    assert_eq!(store.judge_claims(true).unwrap(), 0);
    assert!(store.claim_by_id(&fixture.claim.id).unwrap().is_some());
    assert_eq!(
        store.advance_signature_migration().unwrap().cursor,
        fault.cursor,
        "no local-corruption retry"
    );
    drop(store);
    let store = Store::open(&path, "birch", Arc::new(Plain)).unwrap();
    assert_eq!(store.signature_migration().unwrap().fault, fault.fault);
    store
        .connection
        .write()
        .execute(
            "DELETE FROM replica_envelopes WHERE writer='corrupt-fixture'",
            [],
        )
        .unwrap();
    store.resume_signature_migration().unwrap();
    finish_signature_migration(&store);
    assert_eq!(
        store.claim_signature(&fixture.claim.id).unwrap(),
        Some(fixture.genuine.clone())
    );
    assert_eq!(verdict(&store, &fixture.claim), Verdict::Verified);
    assert_eq!(store.claim_signature("old-gone-claim").unwrap(), None);
    assert!(store.recheck_claim_verdicts().unwrap().is_empty());
    drop(store);
    let store = Store::open(&path, "birch", Arc::new(Plain)).unwrap();
    assert!(store.signature_migration().unwrap().done());
    assert_eq!(store.judge_claims(true).unwrap(), 0);
    assert_eq!(
        store.claim_signature(&fixture.claim.id).unwrap(),
        Some(fixture.genuine)
    );
    assert_eq!(verdict(&store, &fixture.claim), Verdict::Verified);
}

#[test]
fn large_candidate_judging_commits_pages_and_resumes_after_a_persisted_fault() {
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    let anchor = key();
    let store = Arc::new(Store::open(&path, "alder", Arc::new(Plain)).unwrap());
    store.set_node_key(anchor.clone()).unwrap();
    let claim = note(&store, None, "A large unsuperseded signature set");
    seal(&store);
    let genuine = store.claim_signature(&claim.id).unwrap().unwrap();
    let content = content_digest(
        &claim.subject,
        &claim.kind,
        claim.actor.as_deref(),
        &claim.body,
    );
    {
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        for _ in 1..100 {
            let candidate = ClaimSignature::sign(
                &anchor,
                &content,
                "host/alder",
                None,
                vec![],
                genuine.signed_at_unix_ms,
            );
            store_claim_signature_tx(&tx, &claim.id, &candidate).unwrap();
        }
        tx.commit().unwrap();
    }
    assert_eq!(store.signature_candidate_count(&claim.id).unwrap(), 100);
    assert!(
        matches!(verdict(&store,&claim),Verdict::Held(reason) if reason.starts_with("signature-judgment-incomplete:"))
    );
    let stopped = Arc::new(AtomicBool::new(false));
    let cursors = Arc::new(Mutex::new(Vec::new()));
    let state = stopped.clone();
    let seen = cursors.clone();
    let watched = claim.id.clone();
    let observer=store.connection.observe_commits(move |connection| {
        let partial=connection.query_row("SELECT last_signature FROM claim_signature_judging WHERE claim_id=?1 AND complete=0",[&watched],|row|row.get::<_,String>(0)).optional().unwrap();
        if let Some(cursor)=partial && !cursor.is_empty() && !state.swap(true,Ordering::SeqCst) {
            seen.lock().unwrap().push(cursor);
            connection.execute("UPDATE claim_signature_judging SET best_verdict='corrupt-fixture' WHERE claim_id=?1",[&watched]).unwrap();
        }
    });
    assert!(
        store
            .judge_claims(true)
            .unwrap_err()
            .to_string()
            .contains("unknown candidate accumulator verdict")
    );
    drop(observer);
    assert!(
        matches!(verdict(&store,&claim),Verdict::Held(reason) if reason.starts_with("signature-state-corrupt:"))
    );
    let cursor = cursors.lock().unwrap()[0].clone();
    assert!(!cursor.is_empty());
    drop(store);
    let store = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    assert!(store.signature_migration().unwrap().fault.is_some());
    let connection = store.readers.get();
    let resumed: String = connection
        .query_row(
            "SELECT last_signature FROM claim_signature_judging WHERE claim_id=?1",
            [&claim.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(resumed, cursor);
    drop(connection);
    store
        .connection
        .write()
        .execute(
            "UPDATE claim_signature_judging SET best_verdict='verified' WHERE claim_id=?1",
            [&claim.id],
        )
        .unwrap();
    store.resume_signature_migration().unwrap();
    store.judge_claims(true).unwrap();
    assert_eq!(verdict(&store, &claim), Verdict::Verified);
    assert_eq!(store.signature_candidate_count(&claim.id).unwrap(), 100);
    assert_eq!(
        store.signature_writer_candidate_count("alder").unwrap(),
        100
    );
    assert!(
        store
            .signature_state_faults()
            .unwrap()
            .iter()
            .any(
                |fault| fault.code == "signature-candidate-count" && fault.detail.contains("alder")
            )
    );
    assert!(store.recheck_claim_verdicts().unwrap().is_empty());
}

#[test]
fn a_new_candidate_before_the_judging_cursor_restarts_the_accumulator() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let anchor = key();
    let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    store.set_node_key(anchor.clone()).unwrap();
    let claim = note(
        &store,
        None,
        "changed input must restart a partial candidate fold",
    );
    seal(&store);
    let genuine = store.claim_signature(&claim.id).unwrap().unwrap();
    let content = content_digest(
        &claim.subject,
        &claim.kind,
        claim.actor.as_deref(),
        &claim.body,
    );
    {
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        for _ in 1..100 {
            let signature = ClaimSignature::sign(
                &anchor,
                &content,
                "host/alder",
                None,
                vec![],
                genuine.signed_at_unix_ms,
            );
            store_claim_signature_tx(&tx, &claim.id, &signature).unwrap();
        }
        tx.commit().unwrap();
    }
    let mut added = genuine.clone();
    added.nonce = "0".into();
    added.signature = anchor.sign(
        format!(
            "smallclaims-claim-v1\n{content}\nhost/alder\n\n{}\n\n0\n{}",
            added.key, added.signed_at_unix_ms
        )
        .as_bytes(),
    );
    let expected = added.clone();
    let watched = claim.id.clone();
    let changed = Arc::new(AtomicBool::new(false));
    let once = changed.clone();
    let observer=store.connection.observe_commits(move |connection| {
        let partial=connection.query_row("SELECT last_signature FROM claim_signature_judging WHERE claim_id=?1 AND complete=0",[&watched],|row|row.get::<_,String>(0)).optional().unwrap();
        if let Some(cursor)=partial && !cursor.is_empty() && !once.swap(true,Ordering::SeqCst) {
            assert!(serde_json::to_string(&added).unwrap()<cursor);
            store_claim_signature_tx(connection,&watched,&added).unwrap();
        }
    });
    store.judge_claims(true).unwrap();
    drop(observer);
    assert!(changed.load(Ordering::SeqCst));
    assert_eq!(store.signature_candidate_count(&claim.id).unwrap(), 101);
    assert_eq!(verdict(&store, &claim), Verdict::Verified);
    assert_eq!(store.claim_signature(&claim.id).unwrap(), Some(expected));
    assert!(store.recheck_claim_verdicts().unwrap().is_empty());
}

#[test]
fn a_pending_large_revocation_never_authorizes_its_dependent_claim() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let anchor = key();
    let store = Arc::new(Store::open_memory("studio", Arc::new(Plain)).unwrap());
    store.set_node_key(anchor.clone()).unwrap();
    store.set_write_clock_at(100).unwrap();
    let before = note(&store, Some("person/ada"), "before");
    seal(&store);
    let device = store.claim_signature(&before.id).unwrap().unwrap();
    store.set_write_clock_at(120).unwrap();
    let revoked = append(
        &store,
        KEY_REVOKED,
        "person/ada",
        None,
        json!({"key":device.key}),
    );
    let content = content_digest(
        &revoked.subject,
        &revoked.kind,
        revoked.actor.as_deref(),
        &revoked.body,
    );
    let mut bad = ClaimSignature::sign(&anchor, &content, "host/studio", None, vec![], 120);
    bad.signature = "invalid-first-carried-revocation".into();
    attach(&store, &revoked, &bad);
    store.set_write_clock_at(130).unwrap();
    let later = note(&store, Some("person/ada"), "after");
    seal(&store);
    assert_eq!(verdict(&store, &later), Verdict::Verified);
    {
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        for _ in 0..99 {
            let signature =
                ClaimSignature::sign(&anchor, &content, "host/studio", None, vec![], 120);
            store_claim_signature_tx(&tx, &revoked.id, &signature).unwrap();
        }
        tx.commit().unwrap();
    }
    let seen = Arc::new(AtomicBool::new(false));
    let signal = seen.clone();
    let weak = Arc::downgrade(&store);
    let watched = revoked.id.clone();
    let dependent = later.clone();
    let observer=store.connection.observe_commits(move |connection| {
        let pending:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM claim_signature_judging WHERE claim_id=?1 AND complete=0)",[&watched],|row|row.get(0)).unwrap();
        if pending {
            signal.store(true,Ordering::SeqCst);
            assert!(matches!(verdict(&weak.upgrade().unwrap(),&dependent),Verdict::Held(reason) if reason.starts_with("signature-judgment-incomplete:")));
        }
    });
    store.judge_claims(true).unwrap();
    drop(observer);
    assert!(seen.load(Ordering::SeqCst));
    assert!(
        matches!(verdict(&store,&later),Verdict::Invalid(reason) if reason.contains("revoked"))
    );
    assert_eq!(verdict(&store, &before), Verdict::Verified);
    assert!(store.recheck_claim_verdicts().unwrap().is_empty());
}

#[test]
fn migration_commits_claim_offsets_inside_a_large_authenticated_envelope() {
    use smallclaims::replication::ReplicationInventory;
    use smallclaims::store::append_claim_record_tx;
    let anchor = key();
    let source = node("alder", Some(&anchor), Some(&anchor));
    admit(&source, "alder", &anchor, "anchor");
    source.set_write_clock_at(100).unwrap();
    let mut claims = Vec::new();
    {
        let mut connection = source.connection.write();
        let tx = connection.transaction().unwrap();
        let mut batch = None;
        for i in 0..100 {
            let claim = append_claim_record_tx(
                &tx,
                "alder",
                &format!("note/chunk-{i}"),
                "example.note",
                None,
                &json!({"fields":{"text":format!("claim {i}")}}),
                &[],
                batch.as_deref(),
            )
            .unwrap();
            batch = Some(claim.batch_id.clone());
            claims.push(claim);
        }
        tx.commit().unwrap();
    }
    let exchange = source
        .export_replication_exchange(FLEET, &ReplicationInventory::default())
        .unwrap();
    assert_eq!(exchange.envelopes.len(), 2);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    {
        let target = Store::open(&path, "birch", Arc::new(Plain)).unwrap();
        target.bind_fleet(FLEET).unwrap();
        target.pin_fleet_anchor(anchor.public()).unwrap();
        receive_carriers(&target, &exchange);
        target.connection.write().execute_batch("DELETE FROM claim_signature_candidates; DELETE FROM claim_signature_nonce_uses; DELETE FROM claim_signature_judging; DELETE FROM claim_signature_migration;").unwrap();
    }
    let target = Store::open(&path, "birch", Arc::new(Plain)).unwrap();
    while target.signature_migration().unwrap().phase == "legacy" {
        target.advance_signature_migration().unwrap();
    }
    let partial = target.advance_signature_migration().unwrap();
    assert!(partial.claim_offset > 0 && partial.claim_offset < 100);
    assert!(partial.envelope_row > partial.cursor);
    assert_eq!(partial.processed, 64);
    let offset = partial.claim_offset;
    drop(target);
    let target = Store::open(&path, "birch", Arc::new(Plain)).unwrap();
    assert_eq!(target.signature_migration().unwrap().claim_offset, offset);
    finish_signature_migration(&target);
    for claim in &claims {
        assert_eq!(verdict(&target, claim), Verdict::Verified);
        assert_eq!(target.signature_candidate_count(&claim.id).unwrap(), 1);
    }
    assert!(target.recheck_claim_verdicts().unwrap().is_empty());
}

#[test]
fn copied_key_nonce_forgery_and_foreign_key_nonce_do_not_reserve_a_trusted_nonce() {
    let anchor = key();
    let store = node("alder", Some(&anchor), Some(&anchor));
    store.set_write_clock_at(100).unwrap();
    admit(&store, "alder", &anchor, "anchor");
    store.set_write_clock_at(110).unwrap();
    let forged_claim = note(&store, None, "same signed content");
    store.set_write_clock_at(120).unwrap();
    let foreign_claim = note(&store, None, "same signed content");
    store.set_write_clock_at(130).unwrap();
    let genuine_claim = note(&store, None, "same signed content");
    let content = content_digest(
        &genuine_claim.subject,
        &genuine_claim.kind,
        genuine_claim.actor.as_deref(),
        &genuine_claim.body,
    );
    let genuine = ClaimSignature::sign(&anchor, &content, "host/alder", None, vec![], 130);
    let mut forged = genuine.clone();
    forged.signature = "copied-key-and-nonce-without-private-key".into();
    attach(&store, &forged_claim, &forged);
    let foreign_key = key();
    let mut foreign =
        ClaimSignature::sign(&foreign_key, &content, "host/unknown", None, vec![], 120);
    foreign.nonce = genuine.nonce.clone();
    foreign.signature = foreign_key.sign(
        format!(
            "smallclaims-claim-v1\n{content}\nhost/unknown\n\n{}\n\n{}\n120",
            foreign.key, foreign.nonce
        )
        .as_bytes(),
    );
    let fields = genuine_claim.body.get("fields").unwrap();
    assert!(foreign.verifies(&smallclaims::principal::Judged {
        id: &foreign_claim.id,
        subject: &foreign_claim.subject,
        kind: &foreign_claim.kind,
        actor: foreign_claim.actor.as_deref(),
        content: content.clone(),
        fields
    }));
    attach(&store, &foreign_claim, &foreign);
    attach(&store, &genuine_claim, &genuine);
    store.judge_claims(true).unwrap();
    assert!(matches!(
        verdict(&store, &forged_claim),
        Verdict::Invalid(_)
    ));
    assert_eq!(verdict(&store, &genuine_claim), Verdict::Verified);
    assert!(
        store
            .signature_nonce_used(&genuine.key, &genuine.nonce)
            .unwrap()
    );
    assert!(
        store
            .signature_nonce_used(&foreign.key, &foreign.nonce)
            .unwrap()
    );
    assert!(store.recheck_claim_verdicts().unwrap().is_empty());
}

#[test]
fn revoking_a_key_invalidates_what_it_signs_afterwards_and_everything_below_a_revoked_root() {
    let stores = fleet(&["b"]);
    let (a, b) = (&stores[0], &stores[1]);
    let before = note(b, Some("person/ada"), "before");
    seal(b);
    let signature = b.claim_signature(&before.id).unwrap().unwrap();
    // The node revokes ada's device key.
    append(
        b,
        KEY_REVOKED,
        "person/ada",
        None,
        json!({"key": signature.key, "reason": "lost laptop"}),
    );
    let after = note(b, Some("person/ada"), "after");
    sync(b, a);
    for store in [a, b] {
        assert_eq!(verdict(store, &before), Verdict::Verified);
        assert!(
            matches!(verdict(store, &after), Verdict::Invalid(reason) if reason.contains("revoked"))
        );
    }

    // Revoking a root cuts its devices: robin's device signs, then their root is revoked.
    let first = note(b, Some("person/robin"), "first");
    seal(b);
    let robin = b.claim_signature(&first.id).unwrap().unwrap();
    let root_grant = b.claim_by_id(robin.chain.last().unwrap()).unwrap().unwrap();
    let root_key = root_grant.body["fields"]["key"]
        .as_str()
        .unwrap()
        .to_owned();
    append(
        b,
        KEY_REVOKED,
        "person/robin",
        None,
        json!({"key": root_key}),
    );
    let later = note(b, Some("person/robin"), "later");
    sync(b, a);
    for store in [a, b] {
        assert_eq!(verdict(store, &first), Verdict::Verified);
        assert!(
            matches!(verdict(store, &later), Verdict::Invalid(reason) if reason.contains("revoked"))
        );
        assert!(store.recheck_claim_verdicts().unwrap().is_empty());
    }
}

#[test]
fn a_device_key_cannot_grant_keys_or_sign_for_another_person() {
    let store = Store::open_memory("studio", Arc::new(Plain)).unwrap();
    store.set_node_key(key()).unwrap();
    let ada = note(&store, Some("person/ada"), "ada");
    seal(&store);
    let device = store.claim_signature(&ada.id).unwrap().unwrap();

    // ada's device signs a claim as robin, under ada's chain.
    let posing = note(&store, Some("person/robin"), "posing");
    let content = content_digest(
        &posing.subject,
        &posing.kind,
        posing.actor.as_deref(),
        &posing.body,
    );
    let held = store.keyring.by_public(&device.key).unwrap();
    let as_robin = ClaimSignature::sign(
        &held.key,
        &content,
        "person/robin",
        None,
        device.chain.clone(),
        2,
    );
    attach(&store, &posing, &as_robin);
    seal(&store);
    assert!(matches!(verdict(&store, &posing), Verdict::Invalid(_)));
}

#[test]
fn a_tampered_cache_is_caught_and_never_widens_a_verdict() {
    let store = Store::open_memory("studio", Arc::new(Plain)).unwrap();
    store.set_node_key(key()).unwrap();
    let good = note(&store, Some("person/ada"), "good");
    let bad = note(&store, Some("person/ada"), "bad");
    let content = content_digest(&bad.subject, &bad.kind, bad.actor.as_deref(), &bad.body);
    attach(
        &store,
        &bad,
        &ClaimSignature::sign(&key(), &content, "person/ada", None, vec![], 3),
    );
    seal(&store);
    assert!(matches!(verdict(&store, &bad), Verdict::Invalid(_)));
    let expected = verdicts(&store);
    {
        let connection = store.connection.write();
        connection
            .execute(
                "UPDATE claim_verdicts SET verdict='verified', reason=NULL WHERE claim_id=?1",
                [&bad.id],
            )
            .unwrap();
        connection
            .execute("DELETE FROM claim_verdicts WHERE claim_id=?1", [&good.id])
            .unwrap();
        connection
            .execute(
                "INSERT INTO claim_verdicts(claim_id, verdict, signer) VALUES ('claim/injected', 'verified', 'person/ada')",
                [],
            )
            .unwrap();
    }
    let mismatches = store.recheck_claim_verdicts().unwrap();
    let mut found = mismatches
        .iter()
        .map(|mismatch| mismatch.claim_id.as_str())
        .collect::<Vec<_>>();
    found.sort_unstable();
    let mut wanted = vec![bad.id.as_str(), good.id.as_str(), "claim/injected"];
    wanted.sort_unstable();
    assert_eq!(found, wanted);
    assert_eq!(verdicts(&store), expected);
}

#[test]
fn a_restart_keeps_the_keys_and_rebuilds_the_same_verdicts() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("claims.sqlite3");
    let keys = root.path().join("keys");
    let node_key = key();
    let first = {
        let store = Store::open(&path, "studio", Arc::new(Plain)).unwrap();
        store.use_key_directory(&keys).unwrap();
        store.set_node_key(node_key.clone()).unwrap();
        let claim = note(&store, Some("person/ada"), "before the restart");
        seal(&store);
        store.judge_claims(true).unwrap();
        claim
    };
    let store = Store::open(&path, "studio", Arc::new(Plain)).unwrap();
    store.use_key_directory(&keys).unwrap();
    store.set_node_key(node_key).unwrap();
    assert_eq!(
        store.judge_claims(true).unwrap(),
        0,
        "unchanged trust roots and cached verdicts need no re-judging after reopen"
    );
    let before = verdicts(&store);
    let second = note(&store, Some("person/ada"), "after the restart");
    seal(&store);
    assert_eq!(
        store.claim_signature(&first.id).unwrap().unwrap().key,
        store.claim_signature(&second.id).unwrap().unwrap().key,
        "the device key survives the restart"
    );
    assert_eq!(
        store
            .claims_for("person/ada", Some(KEY_GRANTED))
            .unwrap()
            .len(),
        2
    );
    assert_eq!(verdict(&store, &second), Verdict::Verified);
    assert!(store.recheck_claim_verdicts().unwrap().is_empty());
    let after = verdicts(&store);
    assert!(
        before
            .iter()
            .all(|(claim, verdict)| after.get(claim) == Some(verdict))
    );
}

#[test]
fn founding_does_not_rewrite_already_sealed_unsigned_delegations() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let path = smallclaims::fleet::join::store_path(root);
    let (existing, envelopes) = {
        let store = Store::open(&path, "studio", Arc::new(Plain)).unwrap();
        store
            .set_node_key(Arc::new(
                smallclaims::fleet::join::standalone_node_key(root).unwrap(),
            ))
            .unwrap();
        store
            .use_key_directory(&smallclaims::fleet::join::key_directory(root))
            .unwrap();
        let existing = note(
            &store,
            Some("agent/garden/worker"),
            "already affected history",
        );
        // Reproduce the old startup's irreversible unsigned sealing, with the held keys
        // still on disk. Activation may sign envelopes, but cannot rewrite their payloads.
        let mut connection = store.connection.write();
        let transaction = connection.transaction().unwrap();
        smallclaims::store::seed_replica_envelopes_tx(&transaction, "studio", None).unwrap();
        transaction.commit().unwrap();
        let envelopes = connection
            .prepare("SELECT envelope_hash,CAST(payload AS BLOB) FROM replica_envelopes ORDER BY sequence")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        (existing, envelopes)
    };
    smallclaims::fleet::join::found(root, "studio", &Default::default()).unwrap();
    let file = smallclaims::fleet::FleetFile::load(root).unwrap().unwrap();
    let store = Store::open(&path, "studio", Arc::new(Plain)).unwrap();
    smallclaims::fleet::activate(&store, root, &file).unwrap();
    let next = note(
        &store,
        Some("agent/garden/worker"),
        "after affected history",
    );
    seal(&store);
    assert_eq!(verdict(&store, &existing), Verdict::Unsigned);
    assert!(matches!(verdict(&store, &next), Verdict::Invalid(reason)
        if reason.starts_with("delegation ") && reason.ends_with(" is unsigned")));
    assert!(store.claim_verdict_counts().unwrap()["unsigned"] > 0);
    let connection = store.readers.get();
    for (hash, payload) in envelopes {
        let current: Vec<u8> = connection
            .query_row(
                "SELECT CAST(payload AS BLOB) FROM replica_envelopes WHERE envelope_hash=?1",
                [&hash],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(current, payload, "immutable affected envelope {hash}");
    }
}

#[test]
fn a_populated_standalone_store_founds_a_fleet_without_losing_unsealed_delegations() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let path = smallclaims::fleet::join::store_path(root);
    let keys = smallclaims::fleet::join::key_directory(root);
    let node_key = Arc::new(smallclaims::fleet::join::standalone_node_key(root).unwrap());
    let existing = {
        let store = Store::open(&path, "studio", Arc::new(Plain)).unwrap();
        store.set_node_key(node_key.clone()).unwrap();
        store.use_key_directory(&keys).unwrap();
        let person = note(&store, Some("person/ada"), "before founding");
        let agent = note(
            &store,
            Some("agent/garden/worker"),
            "live seat before founding",
        );
        // No replication exchange or checkpoint has sealed this recent work yet.
        assert!(store.claim_signature(&agent.id).unwrap().is_none());
        [person, agent]
    };
    let founded = smallclaims::fleet::join::found(
        root,
        "studio",
        &smallclaims::fleet::join::MemberSettings {
            transports: Some(Vec::new()),
            advertise_loopback: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(founded.anchor_key, node_key.public());
    let file = smallclaims::fleet::FleetFile::load(root).unwrap().unwrap();
    let store = Store::open(&path, "studio", Arc::new(Plain)).unwrap();
    // Fleet activation itself must restore the held keys before it seals old work.
    smallclaims::fleet::activate(&store, root, &file).unwrap();
    let person = note(&store, Some("person/ada"), "after founding");
    let agent = note(
        &store,
        Some("agent/garden/worker"),
        "live seat after founding",
    );
    seal(&store);
    for claim in existing.iter().chain([&person, &agent]) {
        assert_eq!(verdict(&store, claim), Verdict::Verified, "{}", claim.id);
    }
    let peer = Store::open_memory("beacon", Arc::new(Plain)).unwrap();
    peer.bind_fleet(&founded.fleet_id).unwrap();
    peer.pin_fleet_anchor(&founded.anchor_key).unwrap();
    let exchange = store
        .export_replication_exchange_answering(
            &founded.fleet_id,
            &peer.replication_inventory().unwrap(),
            &peer.replication_signature_requests().unwrap(),
        )
        .unwrap();
    peer.receive_replication_exchange("studio", &founded.fleet_id, &exchange)
        .unwrap();
    peer.validate_replication_backlog().unwrap();
    peer.project_replication_backlog().unwrap();
    for claim in existing.iter().chain([&person, &agent]) {
        assert_eq!(
            verdict(&peer, claim),
            Verdict::Verified,
            "peer {}",
            claim.id
        );
    }
    drop(store);
    let restarted = Store::open(&path, "studio", Arc::new(Plain)).unwrap();
    smallclaims::fleet::activate(&restarted, root, &file).unwrap();
    let next = note(
        &restarted,
        Some("agent/garden/worker"),
        "live seat after another restart",
    );
    seal(&restarted);
    assert_eq!(verdict(&restarted, &next), Verdict::Verified);
    assert_eq!(
        restarted.claim_signature(&agent.id).unwrap().unwrap().key,
        restarted.claim_signature(&next.id).unwrap().unwrap().key
    );
}

/// Deterministic shuffles without a dependency.
fn shuffled<T: Clone>(items: &[T], mut seed: u64) -> Vec<T> {
    let mut items = items.to_vec();
    for index in (1..items.len()).rev() {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        items.swap(index, (seed >> 33) as usize % (index + 1));
    }
    items
}

#[test]
fn verdicts_are_the_same_in_any_arrival_order_and_match_a_recompute() {
    let stores = fleet(&["b", "c", "d"]);
    let writers = &stores[1..];
    let people = ["person/ada", "person/robin", "agent/example/reviewer"];
    for round in 0..4 {
        for (index, store) in writers.iter().enumerate() {
            let actor = people[(round + index) % people.len()];
            let claim = note(
                store,
                Some(actor),
                &format!("round {round} from {}", store.origin),
            );
            if round == 2 && index == 0 {
                // b replays one of its own signatures and revokes ada's key on b.
                seal(store);
                let signature = store.claim_signature(&claim.id).unwrap().unwrap();
                let copy = note(
                    store,
                    Some(actor),
                    &format!("round {round} from {}", store.origin),
                );
                attach(store, &copy, &signature);
                append(
                    store,
                    KEY_REVOKED,
                    actor,
                    None,
                    json!({"key": signature.key}),
                );
            }
        }
    }
    let mut results = Vec::new();
    for seed in [1_u64, 7, 42] {
        let receiver = node(&format!("receiver{seed}"), Some(&key()), None);
        // Membership first, from the anchor, then the writers in a shuffled order, twice so
        // late arrivals change earlier answers.
        receiver
            .pin_fleet_anchor(&stores[0].fleet_anchor().unwrap().unwrap())
            .unwrap();
        let order = (0..writers.len()).collect::<Vec<_>>();
        for index in shuffled(&order, seed) {
            sync(&writers[index], &receiver);
        }
        sync(&stores[0], &receiver);
        for index in shuffled(&order, seed.wrapping_add(3)) {
            sync(&writers[index], &receiver);
        }
        let cached = verdicts(&receiver);
        assert!(
            receiver.recheck_claim_verdicts().unwrap().is_empty(),
            "seed {seed}"
        );
        assert!(cached.values().any(|verdict| verdict == "invalid"));
        assert!(cached.values().any(|verdict| verdict == "verified"));
        results.push(cached);
    }
    assert!(results.windows(2).all(|pair| pair[0] == pair[1]));
}

#[test]
fn a_fields_signature_for_a_long_message_reserves_its_nonce() {
    use smallclaims::principal::{FIELDS_FORMAT, fields_signing_bytes};
    let member = key();
    let store = node("alder", None, None);
    store.set_node_key(member.clone()).unwrap();
    let hash = "a".repeat(64);
    let claim = append(
        &store,
        "message.sent",
        "message/body-proof",
        None,
        json!({"body":"preview", "to":"agent/example", "attachments":[{"media_type":"text/plain", "origin":"host/alder", "sha256":hash,"size":100_000}]}),
    );
    store.replication_snapshot().unwrap();
    let mut fields = claim.body["fields"].clone();
    fields["body_sha256"] = json!(hash);
    fields["body_bytes"] = json!(100_000);
    let names = vec!["body_sha256".into(), "body_bytes".into()];
    let nonce = "long-body-field-proof";
    let bytes = fields_signing_bytes(
        &claim.subject,
        &claim.kind,
        None,
        &fields,
        &names,
        "host/alder",
        None,
        member.public(),
        &[],
        nonce,
        1,
    );
    let signature = ClaimSignature {
        signer: "host/alder".into(),
        on_behalf: None,
        key: member.public().into(),
        chain: vec![],
        nonce: nonce.into(),
        signed_at_unix_ms: 1,
        signature: member.sign(&bytes),
        format: Some(FIELDS_FORMAT.into()),
        signed_fields: names,
    };
    {
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        store_claim_signature_tx(&tx, &claim.id, &signature).unwrap();
        tx.commit().unwrap();
    }
    store.judge_claims(true).unwrap();
    assert_eq!(store.claim_verdict(&claim.id).unwrap(), Verdict::Verified);
    assert!(
        store.signature_nonce_used(member.public(), nonce).unwrap(),
        "the same normalized fields must verify for judging and nonce-use maintenance"
    );
}

#[test]
fn candidate_judging_never_rewrites_the_whole_dependency_set_in_one_commit() {
    use std::sync::Mutex;
    for count in [100, 1_000] {
        let member = key();
        let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
        store.set_node_key(member.clone()).unwrap();
        let claim = note(&store, None, "Bound dependency maintenance per writer loan");
        store.replication_snapshot().unwrap();
        let genuine = store.claim_signature(&claim.id).unwrap().unwrap();
        let content = content_digest(
            &claim.subject,
            &claim.kind,
            claim.actor.as_deref(),
            &claim.body,
        );
        {
            let mut connection = store.connection.write();
            let tx = connection.transaction().unwrap();
            for _ in 1..count {
                let signature = ClaimSignature::sign(
                    &member,
                    &content,
                    "host/alder",
                    None,
                    vec![],
                    genuine.signed_at_unix_ms,
                );
                store_claim_signature_tx(&tx, &claim.id, &signature).unwrap();
            }
            tx.commit().unwrap();
        }
        for pass in 0..2 {
            store
                .connection
                .write()
                .execute(
                    "INSERT OR IGNORE INTO claim_verdict_fresh(claim_id) VALUES (?1)",
                    [&claim.id],
                )
                .unwrap();
            let baseline = store.connection.write().total_changes();
            let observed = Arc::new(Mutex::new((baseline, Vec::<u64>::new())));
            let changes = observed.clone();
            let observer = store.connection.observe_commits(move |connection| {
                let now = connection.total_changes();
                let mut state = changes.lock().unwrap();
                let delta = now - state.0;
                state.1.push(delta);
                state.0 = now;
            });
            store.judge_claims(true).unwrap();
            drop(observer);
            let changes = observed.lock().unwrap();
            let largest = changes.1.iter().copied().max().unwrap();
            assert!(
                largest <= 128,
                "candidate_count={count}, pass={pass}, largest commit DML={largest}; finalization or restart must not bulk-rebuild candidate dependencies"
            );
            assert_eq!(
                store.signature_candidate_count(&claim.id).unwrap(),
                count as u64
            );
            assert_eq!(verdict(&store, &claim), Verdict::Verified);
        }
        assert!(store.recheck_claim_verdicts().unwrap().is_empty());
    }
}

/// Reconstruct the actual pre-upgrade schema, not only a missing migration control row.
/// Immutable claims and authenticated carriers and the first-carried cache remain intact.
fn remove_candidate_schema(connection: &rusqlite::Connection) {
    connection.execute_batch("DROP TABLE claim_signature_candidates; DROP TABLE claim_signature_sources; DROP TABLE claim_signature_counts; DROP TABLE claim_signature_writer_counts; DROP TABLE claim_signature_nonce_uses; DROP TABLE claim_signature_judging; DROP TABLE claim_signature_migration;
        DROP INDEX claim_candidate_verdicts_verdict_index; DROP INDEX claim_candidate_verdict_links_claim_index;
        CREATE INDEX claim_verdicts_verdict_index ON claim_verdicts(verdict);
        CREATE INDEX claim_verdict_links_claim_index ON claim_verdict_links(claim_id);
        CREATE INDEX claim_signatures_nonce_index ON claim_signatures(key,nonce); PRAGMA user_version=19;").unwrap();
}

#[test]
fn online_upgrade_keeps_live_signing_delegation_and_revocation_current() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    let keys = directory.path().join("keys");
    let node_key = key();
    let before = {
        let store = Store::open(&path, "studio", Arc::new(Plain)).unwrap();
        store.use_key_directory(&keys).unwrap();
        store.set_node_key(node_key.clone()).unwrap();
        store.set_write_clock_at(100).unwrap();
        let before = note(&store, Some("person/ada"), "before upgrade");
        seal(&store);
        assert_eq!(verdict(&store, &before), Verdict::Verified);
        remove_candidate_schema(&store.connection.write());
        before
    };
    let store = Store::open(&path, "studio", Arc::new(Plain)).unwrap();
    store.use_key_directory(&keys).unwrap();
    store.set_node_key(node_key).unwrap();
    assert!(store.signature_migration().unwrap().serving_legacy);
    assert!(
        store
            .signature_state_faults()
            .unwrap()
            .iter()
            .any(|fault| fault.code == "signature-legacy-p0-exposed")
    );
    {
        let connection = store.connection.write();
        connection.execute("UPDATE claim_signature_migration SET advanced_at_unix_ms=advanced_at_unix_ms-600000 WHERE id=1",[]).unwrap();
    }
    assert!(
        store
            .signature_state_faults()
            .unwrap()
            .iter()
            .any(|fault| fault.code == "signature-bootstrap-stalled")
    );
    store.advance_signature_migration().unwrap();
    assert!(store.signature_migration().unwrap().completed_work > 0);
    assert!(
        !store
            .signature_state_faults()
            .unwrap()
            .iter()
            .any(|fault| fault.code == "signature-bootstrap-stalled")
    );
    assert!(
        store
            .signature_migration()
            .unwrap()
            .progress_description()
            .contains("work units/s")
    );
    assert_eq!(verdict(&store, &before), Verdict::Verified);
    let signature = store.claim_signature(&before.id).unwrap().unwrap();
    assert!(
        store
            .signature_nonce_used(&signature.key, &signature.nonce)
            .unwrap()
    );
    assert_eq!(
        store.claim_verdict_counts().unwrap().get("verified"),
        Some(&3)
    );
    store.set_write_clock_at(110).unwrap();
    let during = note(&store, Some("person/ada"), "during upgrade");
    seal(&store);
    assert_eq!(verdict(&store, &during), Verdict::Verified);
    let during_signature = store.claim_signature(&during.id).unwrap().unwrap();
    assert_eq!(
        during_signature.key, signature.key,
        "existing held keys and grants remain usable"
    );
    assert_eq!(during_signature.chain, signature.chain);
    store.set_write_clock_at(120).unwrap();
    let revocation = append(
        &store,
        KEY_REVOKED,
        "person/ada",
        None,
        json!({"key":signature.key}),
    );
    seal(&store);
    assert_eq!(verdict(&store, &revocation), Verdict::Verified);
    store.set_write_clock_at(130).unwrap();
    let after = note(&store, Some("person/ada"), "after revocation");
    seal(&store);
    assert!(
        matches!(verdict(&store,&after),Verdict::Invalid(reason) if reason.contains("revoked"))
    );
    assert_eq!(verdict(&store, &before), Verdict::Verified);
    finish_signature_migration(&store);
    assert!(!store.signature_migration().unwrap().serving_legacy);
    assert_eq!(
        store.claim_signature(&during.id).unwrap(),
        Some(during_signature)
    );
    assert_eq!(verdict(&store, &during), Verdict::Verified);
    assert!(
        matches!(verdict(&store,&after),Verdict::Invalid(reason) if reason.contains("revoked"))
    );
    assert!(store.recheck_claim_verdicts().unwrap().is_empty());
}

#[test]
fn a_nonce_check_before_online_cutover_is_revalidated_in_the_claim_transaction() {
    use smallclaims::claim::ReplicaEnvelopePayload;
    use smallclaims::replication::ReplicationInventory;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    let anchor = key();
    let (earlier, hidden) = {
        let source = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
        source.bind_fleet(FLEET).unwrap();
        source.pin_fleet_anchor(anchor.public()).unwrap();
        source.set_member_key(Some(anchor.clone())).unwrap();
        source.set_write_clock_at(100).unwrap();
        admit(&source, "alder", &anchor, "anchor");
        source.set_write_clock_at(110).unwrap();
        let earlier = note(&source, Some("host/alder"), "same content");
        let content = content_digest(
            &earlier.subject,
            &earlier.kind,
            earlier.actor.as_deref(),
            &earlier.body,
        );
        let selected = ClaimSignature::sign(&anchor, &content, "host/alder", None, vec![], 110);
        let hidden = ClaimSignature::sign(&anchor, &content, "host/alder", None, vec![], 110);
        attach(&source, &earlier, &selected);
        let exchange = source
            .export_replication_exchange(FLEET, &ReplicationInventory::default())
            .unwrap();
        let original = exchange
            .envelopes
            .iter()
            .find(|envelope| {
                let payload: ReplicaEnvelopePayload =
                    ciborium::from_reader(envelope.payload.bytes().unwrap()).unwrap();
                payload
                    .batch
                    .claims
                    .iter()
                    .any(|claim| claim.id == earlier.id)
            })
            .unwrap();
        let mut payload: ReplicaEnvelopePayload =
            ciborium::from_reader(original.payload.bytes().unwrap()).unwrap();
        payload
            .claim_signatures
            .insert(earlier.id.clone(), hidden.clone());
        let mut carriers = exchange.clone();
        carriers.envelopes.clear();
        carriers.signatures.clear();
        push_signature_carrier(&mut carriers, original, &payload, &anchor);
        receive_carriers(&source, &carriers);
        let connection = source.connection.write();
        connection.execute("UPDATE claim_signatures SET signer=?1,key=?2,nonce=?3,signature=?4 WHERE claim_id=?5",rusqlite::params![selected.signer,selected.key,selected.nonce,serde_json::to_string(&selected).unwrap(),earlier.id]).unwrap();
        remove_candidate_schema(&connection);
        (earlier, hidden)
    };
    let store = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    assert!(store.signature_migration().unwrap().serving_legacy);
    assert!(
        !store
            .signature_nonce_used(&hidden.key, &hidden.nonce)
            .unwrap(),
        "the complete legacy first-carried model has not reserved the alternate nonce"
    );
    // Model the real API's successful check followed by its later writer savepoint.
    finish_signature_migration(&store);
    assert!(
        store
            .signature_nonce_used(&hidden.key, &hidden.nonce)
            .unwrap()
    );
    let before = store
        .readers
        .get()
        .query_row("SELECT COUNT(*) FROM claims", [], |row| {
            row.get::<_, u64>(0)
        })
        .unwrap();
    let result = store.append_signed_claim(
        &ClaimInput {
            subject: earlier.subject.clone(),
            kind: earlier.kind.clone(),
            actor: earlier.actor.clone(),
            fields: serde_json::from_value(earlier.body["fields"].clone()).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        },
        &hidden,
    );
    assert_eq!(result.unwrap_err().code, "device-signature-replayed");
    assert_eq!(
        store
            .readers
            .get()
            .query_row("SELECT COUNT(*) FROM claims", [], |row| row
                .get::<_, u64>(0))
            .unwrap(),
        before,
        "nonce refusal rolls the claim back"
    );
}

fn principal_physical_roots(
    connection: &rusqlite::Connection,
    legacy: bool,
) -> BTreeMap<String, i64> {
    let tables = if legacy {
        "'legacy_claim_signatures','legacy_claim_verdicts','legacy_claim_verdict_links','legacy_claim_verdict_queue','legacy_claim_verdict_fresh'"
    } else {
        "'claim_signatures','claim_verdicts','claim_verdict_links','claim_verdict_queue','claim_verdict_fresh'"
    };
    connection.prepare(&format!("SELECT name,rootpage FROM sqlite_schema WHERE type IN ('table','index') AND tbl_name IN ({tables})" )).unwrap().query_map([], |row|Ok((row.get::<_,String>(0)?.replace("legacy_", ""),row.get::<_,i64>(1)?))).unwrap().collect::<Result<BTreeMap<_,_>,_>>().unwrap()
}

#[test]
fn live_signed_appends_between_inventory_loans_need_no_bulk_activation_tail() {
    use smallclaims::store::principals::SignatureModel;
    use std::sync::mpsc;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    let member = key();
    {
        let source = Store::open(&path, "studio", Arc::new(Plain)).unwrap();
        source.set_node_key(member.clone()).unwrap();
        for i in 0..513 {
            note(&source, None, &format!("initial {i}"));
        }
        seal(&source);
        remove_candidate_schema(&source.connection.write());
    }
    let store = Arc::new(Store::open(&path, "studio", Arc::new(Plain)).unwrap());
    store.set_node_key(member.clone()).unwrap();
    let (start, work) = mpsc::channel();
    let (completed, received) = mpsc::channel();
    let writer = store.clone();
    let producer = std::thread::spawn(move || {
        for i in work {
            let input = ClaimInput {
                subject: format!("note/live-{i}"),
                kind: "example.note".into(),
                actor: Some("host/studio".into()),
                fields: serde_json::from_value(json!({"text":format!("live {i}")})).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            };
            let body = json!({"fields":input.fields,"evidence":input.evidence});
            let content =
                content_digest(&input.subject, &input.kind, input.actor.as_deref(), &body);
            let signature = ClaimSignature::sign(&member, &content, "host/studio", None, vec![], 1);
            let (claim, _) = writer.append_signed_claim(&input, &signature).unwrap();
            completed.send((claim, signature)).unwrap();
        }
    });
    let mut live = Vec::new();
    for i in 0..16 {
        let page = store.advance_signature_migration().unwrap();
        assert!(
            page.serving_legacy,
            "fixture must exercise live dual maintenance before activation"
        );
        start.send(i).unwrap();
        let (claim, signature) = received.recv().unwrap();
        assert_eq!(store.signature_model().unwrap(), SignatureModel::Legacy);
        assert_eq!(
            store.claim_signature(&claim.id).unwrap(),
            Some(signature.clone())
        );
        assert_eq!(store.signature_candidate_count(&claim.id).unwrap(), 1);
        live.push((claim, signature));
    }
    drop(start);
    producer.join().unwrap();
    // Sealing remains on the old serving model while the captured inventory is incomplete.
    seal(&store);
    assert!(store.signature_migration().unwrap().serving_legacy);
    // Exercise the existing checkpoint deletion hook on a sealed, live new input.
    // This is maintenance coverage, not proof this removal family qualifies for production.
    let (dropped, _) = live.remove(0);
    {
        use smallclaims::store::checkpoint::{
            ClaimTombstone, delete_dropped_rows_tx, record_checkpoint_tombstones_tx,
        };
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        let (writer,sequence,hash)=tx.query_row("SELECT writer,sequence,envelope_hash FROM replica_envelopes WHERE batch_id=?1 ORDER BY rowid LIMIT 1",[&dropped.batch_id],|row|Ok((row.get::<_,String>(0)?,row.get::<_,u64>(1)?,row.get::<_,String>(2)?))).unwrap();
        let tombstone = ClaimTombstone {
            id: dropped.id.clone(),
            writer,
            sequence,
            envelope_hash: hash,
            subject: dropped.subject.clone(),
            kind: dropped.kind.clone(),
            actor: dropped.actor.clone(),
            predecessors: dropped.predecessors.clone(),
            operation_id: dropped.operation_id.clone(),
            request_digest: dropped.request_digest.clone(),
            accepted_at_unix_ms: dropped.accepted_at_unix_ms,
        };
        record_checkpoint_tombstones_tx(
            &tx,
            "online-maintenance-fixture",
            &[],
            std::slice::from_ref(&tombstone),
        )
        .unwrap();
        delete_dropped_rows_tx(&tx, &[], &[tombstone]).unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(store.claim_signature(&dropped.id).unwrap(), None);
    assert_eq!(store.signature_candidate_count(&dropped.id).unwrap(), 0);
    // Reconstruct an unsupported-record receipt and apply its normal repair API during build.
    let record_ref: String = store
        .readers
        .get()
        .query_row(
            "SELECT record_ref FROM replica_records WHERE claim_id=?1 ORDER BY position LIMIT 1",
            [&live[0].0.id],
            |row| row.get(0),
        )
        .unwrap();
    store.connection.write().execute("UPDATE replica_records SET state='unknown',error_code='unknown-kind' WHERE record_ref=?1",[&record_ref]).unwrap();
    store
        .repair_replica_record(
            &record_ref,
            &live[1].0.id,
            "known replacement",
            "host/studio",
            "online-repair",
        )
        .unwrap();
    store.apply_replication_repairs().unwrap();
    assert_eq!(
        store
            .readers
            .get()
            .query_row(
                "SELECT state FROM replica_records WHERE record_ref=?1",
                [&record_ref],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "repaired"
    );
    assert!(store.signature_migration().unwrap().serving_legacy);
    // Replay only runtime views; it must not discard either signature model's relations.
    store.replay_replication_graph().unwrap();
    finish_signature_migration(&store);
    assert_eq!(store.signature_model().unwrap(), SignatureModel::Candidate);
    for (claim, signature) in live {
        let authorization = store.claim_authorization(&claim.id).unwrap();
        assert_eq!(authorization.model, SignatureModel::Candidate);
        assert_eq!(authorization.signature, Some(signature));
        assert_eq!(authorization.verdict, Verdict::Verified);
    }
    let connection = store.readers.get();
    assert!(!connection.query_row("SELECT EXISTS(SELECT 1 FROM claim_verdict_queue) OR EXISTS(SELECT 1 FROM claim_verdict_fresh)",[],|row|row.get::<_,bool>(0)).unwrap());
    drop(connection);
    assert!(store.recheck_claim_verdicts().unwrap().is_empty());
}

#[test]
fn a_failed_online_schema_upgrade_rolls_back_renames_and_the_version_marker() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    let roots = {
        let source = Store::open(&path, "studio", Arc::new(Plain)).unwrap();
        source.set_node_key(key()).unwrap();
        note(&source, None, "schema rollback");
        seal(&source);
        let connection = source.connection.write();
        remove_candidate_schema(&connection);
        connection
            .execute_batch(
                "PRAGMA user_version=18; CREATE VIEW legacy_claim_verdict_links AS SELECT 1;",
            )
            .unwrap();
        principal_physical_roots(&connection, false)
    };
    let error = match Store::open(&path, "studio", Arc::new(Plain)) {
        Ok(_) => panic!("injected rename collision must fail"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("already another table or index"),
        "{error:#}"
    );
    let connection =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        18
    );
    assert_eq!(principal_physical_roots(&connection, false), roots);
    assert!(!connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='claim_signature_migration' OR name='legacy_claim_signatures')",[],|row|row.get::<_,bool>(0)).unwrap());
}

#[test]
fn a_broken_legacy_nonce_lookup_cannot_authorize_from_its_cached_verified_result() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    let claim = {
        let source = Store::open(&path, "studio", Arc::new(Plain)).unwrap();
        source.set_node_key(key()).unwrap();
        let claim = note(&source, None, "legacy read failure");
        seal(&source);
        remove_candidate_schema(&source.connection.write());
        claim
    };
    let store = Store::open(&path, "studio", Arc::new(Plain)).unwrap();
    assert_eq!(verdict(&store, &claim), Verdict::Verified);
    store.connection.write().execute_batch("ALTER TABLE legacy_claim_signatures RENAME COLUMN nonce TO broken_nonce; DELETE FROM meta WHERE key='claim_verdict_roots';").unwrap();
    assert!(
        store
            .judge_claims(true)
            .unwrap_err()
            .to_string()
            .contains("legacy authorization facts")
    );
    assert!(store.signature_migration().unwrap().legacy_fault.is_some());
    assert!(
        store
            .claim_verdict(&claim.id)
            .unwrap_err()
            .to_string()
            .contains("legacy projection")
    );
    assert!(
        store
            .signature_state_faults()
            .unwrap()
            .iter()
            .any(|fault| fault.code == "signature-state-corrupt")
    );
    store
        .connection
        .write()
        .execute_batch("ALTER TABLE legacy_claim_signatures RENAME COLUMN broken_nonce TO nonce;")
        .unwrap();
    store.resume_signature_migration().unwrap();
    store.judge_claims(true).unwrap();
    assert_eq!(verdict(&store, &claim), Verdict::Verified);
    let signature = store.claim_signature(&claim.id).unwrap().unwrap();
    store
        .connection
        .write()
        .execute(
            "UPDATE legacy_claim_signatures SET signature='not-json' WHERE claim_id=?1",
            [&claim.id],
        )
        .unwrap();
    let stopped = store.advance_signature_migration().unwrap();
    assert!(
        stopped.fault.is_some() && stopped.legacy_fault.is_some(),
        "a damaged legacy source is not only a shadow-model failure"
    );
    assert!(store.claim_verdict(&claim.id).is_err());
    store
        .connection
        .write()
        .execute(
            "UPDATE legacy_claim_signatures SET signature=?1 WHERE claim_id=?2",
            rusqlite::params![serde_json::to_string(&signature).unwrap(), claim.id],
        )
        .unwrap();
    store.resume_signature_migration().unwrap();
    finish_signature_migration(&store);
    assert_eq!(verdict(&store, &claim), Verdict::Verified);
}

#[cfg(feature = "test-support")]
#[test]
fn root_changes_between_shadow_loans_are_reconciled_before_activation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    let claims = {
        let source = Store::open(&path, "studio", Arc::new(Plain)).unwrap();
        source.set_node_key(key()).unwrap();
        let claims = (0..65)
            .map(|i| note(&source, None, &format!("root change {i}")))
            .collect::<Vec<_>>();
        seal(&source);
        remove_candidate_schema(&source.connection.write());
        claims
    };
    let store = Store::open(&path, "studio", Arc::new(Plain)).unwrap();
    while store.signature_migration().unwrap().phase != "judging" {
        store.advance_signature_migration().unwrap();
    }
    store.judge_claim_page_for_test(true).unwrap();
    assert!(store.signature_migration().unwrap().serving_legacy);
    assert!(
        store
            .readers
            .get()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM claim_verdicts WHERE verdict='verified')",
                [],
                |row| row.get::<_, bool>(0)
            )
            .unwrap()
    );
    store.set_node_key(key()).unwrap();
    // Even a caller skipping the initial root pass cannot activate stale bootstrap roots.
    while !store.signature_migration().unwrap().done() {
        store.judge_claim_page_for_test(false).unwrap();
    }
    for claim in claims {
        assert!(
            matches!(verdict(&store, &claim), Verdict::Held(_)),
            "old node key is no longer trusted in the active snapshot"
        );
    }
    assert!(store.recheck_claim_verdicts().unwrap().is_empty());
}

#[test]
fn pending_judgment_diagnostics_aggregate_thousands_of_rows_into_one_bounded_sample() {
    let store = Store::open_memory("diagnostics", Arc::new(Plain)).unwrap();
    let mut connection = store.connection.write();
    let tx = connection.transaction().unwrap();
    for i in 0..4096 {
        tx.execute("INSERT INTO claim_signature_judging(claim_id,candidate_count,roots_digest,content_digest,last_signature,complete,dirty) VALUES (?1,100,'roots','content','',0,0)",[format!("pending/{i:05}")]).unwrap();
    }
    tx.commit().unwrap();
    drop(connection);
    let faults = store.signature_state_faults().unwrap();
    let pending = faults
        .iter()
        .filter(|fault| fault.code == "signature-judgment-incomplete")
        .collect::<Vec<_>>();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].subject, "host/diagnostics");
    assert!(pending[0].detail.contains("4096 claims"));
    assert!(pending[0].detail.contains("first 20 claim ids"));
    assert!(pending[0].detail.contains("pending/00019"));
    assert!(!pending[0].detail.contains("pending/00020"));
    assert!(pending[0].detail.len() < 1024);
}
