//! Signed claims and their verdicts, on the plain runtime: free mode, members verifying each
//! other, forged, replayed, swapped and revoked signatures, a tampered cache, a restart, and the
//! same verdicts whatever order claims arrive in.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};
use smallclaims::claim::{ClaimInput, ClaimRecord};
use smallclaims::fleet::MemberKey;
use smallclaims::principal::{ClaimSignature, KEY_GRANTED, KEY_REVOKED, Verdict, content_digest};
use smallclaims::store::Store;
use smallclaims::store::principals::store_claim_signature_tx;
use smallclaims::store::runtime::Plain;

const FLEET: &str = "6f1e8a52-3c4d-4b7e-9a10-2d5c8e7f9b31";

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
