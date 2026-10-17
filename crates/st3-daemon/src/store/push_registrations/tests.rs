use super::*;

const PERSON: &str = "person/example";
const ACTOR: &str = "person/example/session/phone";
const GRANT: &str = "custom/client/push-phone";
const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const CREDENTIAL_HASH: &str = "cleanup-credential-hash";

fn claim(store: &Store, subject: &str, kind: &str, fields: Value) {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: Some(PERSON.into()),
            fields: serde_json::from_value(fields).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

fn grant(store: &Store, subject: &str, person: &str, actor: &str, device: &str, expiry: u64) {
    claim(
        store,
        subject,
        "custom.client.pairing-completed",
        json!({
            "person_id":person,"session_actor":actor,"device_id":device,
            "expires_at_unix_ms":expiry,"scopes":[],"credential_hash":CREDENTIAL_HASH
        }),
    );
}

fn registration(token: &str, environment: PushEnvironment) -> PushRegistration {
    PushRegistration {
        token: token.into(),
        environment,
        bundle_id: "org.example.push".into(),
        app_version: "1.0".into(),
    }
}

fn count(store: &Store) -> usize {
    store
        .readers
        .get()
        .query_row("SELECT COUNT(*) FROM local_push_registrations", [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn cleanup_proof(store: &Store, subject: &str) -> PushCleanupProof {
    let pairing_index = store.readers.get().query_row(
        "SELECT MAX(store_index) FROM claims WHERE subject=?1
         AND kind='custom.client.pairing-completed'",
        [subject],
        |row| row.get(0),
    ).unwrap();
    PushCleanupProof {
        grant: subject.into(),
        pairing_index,
        person: PERSON.into(),
        device: "device/phone".into(),
        actor: ACTOR.into(),
        credential_hash: CREDENTIAL_HASH.into(),
    }
}

#[test]
fn push_terminal_cleanup_is_idempotent_for_current_expired_and_revoked_pairings() {
    for (expiry, revoked) in [(9_000_000_000_000, false), (2000, false), (2000, true)] {
        let store = Store::open_memory("push-owner").unwrap();
        grant(&store, GRANT, PERSON, ACTOR, "device/phone", expiry);
        let proof = cleanup_proof(&store, GRANT);
        let input = registration(TOKEN, PushEnvironment::Development);
        store.update_push_registration(GRANT, PERSON, ACTOR, Some(&input), || 1000)
            .unwrap().unwrap();
        if revoked {
            claim(&store, GRANT, "custom.client.pairing-revoked", json!({}));
        }
        for _ in 0..2 {
            assert_eq!(
                store.cleanup_push_registration(&proof).unwrap(),
                Some(PushRegistrationStatus::unregistered()),
            );
            assert_eq!(count(&store), 0);
        }
    }
}

#[test]
fn push_terminal_cleanup_rechecks_every_proof_field_without_removing_other_rows() {
    let store = Store::open_memory("push-owner").unwrap();
    grant(&store, GRANT, PERSON, ACTOR, "device/phone", 2000);
    let proof = cleanup_proof(&store, GRANT);
    let input = registration(TOKEN, PushEnvironment::Development);
    store.update_push_registration(GRANT, PERSON, ACTOR, Some(&input), || 1000)
        .unwrap().unwrap();
    for field in 0..6 {
        for replacement in ["", "wrong-identity"] {
            let mut bad = proof.clone();
            match field {
                0 => bad.grant = replacement.into(),
                1 => bad.person = replacement.into(),
                2 => bad.device = replacement.into(),
                3 => bad.actor = replacement.into(),
                4 => bad.credential_hash = replacement.into(),
                _ => bad.pairing_index = if replacement.is_empty() { 0 } else { u64::MAX },
            }
            assert!(store.cleanup_push_registration(&bad).unwrap().is_none());
            assert_eq!(count(&store), 1);
        }
    }
    let mut wrong_index = proof.clone();
    wrong_index.pairing_index += 1;
    assert!(store.cleanup_push_registration(&wrong_index).unwrap().is_none());
    assert_eq!(count(&store), 1);
}

#[test]
fn push_terminal_cleanup_rejects_malformed_claims_and_foreign_issuers() {
    for field in ["person_id", "device_id", "session_actor", "credential_hash"] {
        let store = Store::open_memory("push-owner").unwrap();
        let mut fields = json!({
            "person_id":PERSON,"device_id":"device/phone","session_actor":ACTOR,
            "credential_hash":CREDENTIAL_HASH,"expires_at_unix_ms":2000
        });
        fields[field] = json!(17);
        claim(&store, GRANT, "custom.client.pairing-completed", fields);
        let proof = cleanup_proof(&store, GRANT);
        assert!(store.cleanup_push_registration(&proof).unwrap().is_none());
    }
    let issuer = Store::open_memory("push-foreign").unwrap();
    let store = Store::open_memory("push-owner").unwrap();
    grant(&issuer, GRANT, PERSON, ACTOR, "device/phone", 2000);
    replicate(&issuer, &store);
    let proof = cleanup_proof(&store, GRANT);
    assert!(store.cleanup_push_registration(&proof).unwrap().is_none());
}

#[test]
fn push_terminal_cleanup_queued_before_revocation_still_succeeds() {
    use std::sync::mpsc;
    use std::time::Duration;

    let store = Arc::new(Store::open_memory("push-owner").unwrap());
    grant(&store, GRANT, PERSON, ACTOR, "device/phone", 2000);
    let proof = cleanup_proof(&store, GRANT);
    let input = registration(TOKEN, PushEnvironment::Development);
    store.update_push_registration(GRANT, PERSON, ACTOR, Some(&input), || 1000)
        .unwrap().unwrap();
    let (proxy, incoming) = mpsc::channel();
    let original = store.connection.jobs.lock().unwrap().replace(proxy).unwrap();
    let worker_store = store.clone();
    let worker = std::thread::spawn(move || worker_store.cleanup_push_registration(&proof));
    let job = incoming.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(matches!(&job, smallclaims::sqlite::WriterJob::Batched { .. }));
    *store.connection.jobs.lock().unwrap() = Some(original.clone());
    claim(&store, GRANT, "custom.client.pairing-revoked", json!({}));
    original.send(job).unwrap();
    assert_eq!(worker.join().unwrap().unwrap(), Some(PushRegistrationStatus::unregistered()));
    assert_eq!(count(&store), 0);
}

#[test]
fn push_terminal_cleanup_queued_before_repairing_preserves_the_new_registration() {
    use std::sync::mpsc;
    use std::time::Duration;

    for replacement_grant in [GRANT, "custom/client/new-phone"] {
        let store = Arc::new(Store::open_memory("push-owner").unwrap());
        grant(&store, GRANT, PERSON, ACTOR, "device/phone", 2000);
        let proof = cleanup_proof(&store, GRANT);
        let input = registration(TOKEN, PushEnvironment::Development);
        store.update_push_registration(GRANT, PERSON, ACTOR, Some(&input), || 1000)
            .unwrap().unwrap();
        // Intercept actual queue admission so the replacement commits after proof
        // authentication but before the cleanup transaction can inspect the claim.
        let (proxy, incoming) = mpsc::channel();
        let original = store.connection.jobs.lock().unwrap().replace(proxy).unwrap();
        let worker_store = store.clone();
        let worker_proof = proof.clone();
        let worker = std::thread::spawn(move || worker_store.cleanup_push_registration(&worker_proof));
        let job = incoming.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(matches!(&job, smallclaims::sqlite::WriterJob::Batched { .. }));
        *store.connection.jobs.lock().unwrap() = Some(original.clone());
        grant(&store, replacement_grant, PERSON, ACTOR, "device/phone", 3000);
        store.update_push_registration(replacement_grant, PERSON, ACTOR, Some(&input), || 1000)
            .unwrap().unwrap();
        original.send(job).unwrap();
        let result = worker.join().unwrap().unwrap();
        if replacement_grant == GRANT {
            assert_eq!(result, None, "a newer claim for the same subject invalidates the proof");
        } else {
            assert_eq!(result, Some(PushRegistrationStatus::unregistered()));
        }
        assert_eq!(count(&store), 1);
        let connection = store.readers.get();
        let (row_grant, row_index): (String, u64) = connection.query_row(
            "SELECT pairing_grant,pairing_index FROM local_push_registrations",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(row_grant, replacement_grant);
        assert!(row_index > proof.pairing_index);
    }
}

#[test]
fn push_terminal_cleanup_database_errors_are_redacted() {
    let store = Store::open_memory("push-owner").unwrap();
    grant(&store, GRANT, PERSON, ACTOR, "device/phone", 2000);
    let proof = cleanup_proof(&store, GRANT);
    store.connection.write().execute_batch("DROP TABLE local_push_registrations").unwrap();
    let error = store.cleanup_push_registration(&proof).unwrap_err();
    assert_eq!(error.to_string(), "local push registration cleanup failed");
    let diagnostic = format!("{error:?}");
    assert!(!diagnostic.contains(CREDENTIAL_HASH));
    assert!(!diagnostic.contains(TOKEN));
}

#[test]
fn push_registration_replaces_one_key_isolates_people_and_devices_and_unregisters_idempotently() {
    let store = Store::open_memory("push-owner").unwrap();
    grant(&store, GRANT, PERSON, ACTOR, "device/phone", 2000);
    grant(
        &store,
        "custom/client/second",
        PERSON,
        "client/second",
        "device/second",
        2000,
    );
    grant(
        &store,
        "custom/client/other-person",
        "person/other",
        "client/other",
        "device/phone",
        2000,
    );
    let before = store.index().unwrap();
    let initial = registration(TOKEN, PushEnvironment::Development);
    assert_eq!(
        store
            .update_push_registration(GRANT, PERSON, ACTOR, Some(&initial), || 1000)
            .unwrap()
            .unwrap(),
        PushRegistrationStatus {
            registered: true,
            environment: Some(PushEnvironment::Development)
        }
    );
    let replacement = registration("aabbccdd", PushEnvironment::Production);
    store
        .update_push_registration(GRANT, PERSON, ACTOR, Some(&replacement), || 1000)
        .unwrap()
        .unwrap();
    assert_eq!(count(&store), 1);
    assert_eq!(
        store
            .readers
            .get()
            .query_row("SELECT token FROM local_push_registrations", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
        "aabbccdd"
    );
    store
        .update_push_registration(
            "custom/client/second",
            PERSON,
            "client/second",
            Some(&initial),
            || 1000,
        )
        .unwrap()
        .unwrap();
    store
        .update_push_registration(
            "custom/client/other-person",
            "person/other",
            "client/other",
            Some(&initial),
            || 1000,
        )
        .unwrap()
        .unwrap();
    assert_eq!(count(&store), 3);
    assert_eq!(
        store
            .push_registration_statuses(PERSON, 1000)
            .unwrap()
            .len(),
        2
    );
    for _ in 0..2 {
        assert_eq!(
            store
                .update_push_registration(GRANT, PERSON, ACTOR, None, || 1000)
                .unwrap()
                .unwrap(),
            PushRegistrationStatus::unregistered()
        );
    }
    assert_eq!(count(&store), 2);
    assert_eq!(
        store.index().unwrap(),
        before,
        "secret mutations never write a claim"
    );
}

#[test]
fn push_exact_session_identity_and_expiry_are_rechecked_inside_writer_transaction() {
    let store = Store::open_memory("push-owner").unwrap();
    grant(&store, GRANT, PERSON, ACTOR, "device/phone", 2000);
    let input = registration(TOKEN, PushEnvironment::Development);
    for (subject, person, actor) in [
        (GRANT, "person/other", ACTOR),
        (GRANT, PERSON, "client/other"),
        ("custom/client/absent", PERSON, ACTOR),
    ] {
        assert!(
            store
                .update_push_registration(subject, person, actor, Some(&input), || 1000)
                .unwrap()
                .is_none()
        );
    }
    store
        .update_push_registration(GRANT, PERSON, ACTOR, Some(&input), || 1000)
        .unwrap()
        .unwrap();
    assert!(
        store
            .update_push_registration(GRANT, PERSON, ACTOR, Some(&input), || 2000)
            .unwrap()
            .is_none()
    );
    assert_eq!(count(&store), 0);
    assert!(
        store
            .update_push_registration(GRANT, PERSON, ACTOR, None, || 2001)
            .unwrap()
            .is_none()
    );
}

#[test]
// This regression intentionally exercises the live-session update helper's DELETE
// branch. HTTP DELETE instead uses terminal cleanup and does not require a live grant.
fn push_post_and_delete_reject_grant_expiry_while_queued_for_writer() {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    for register in [true, false] {
        let store = Arc::new(Store::open_memory("push-owner").unwrap());
        grant(&store, GRANT, PERSON, ACTOR, "device/phone", 2000);
        if !register {
            let input = registration(TOKEN, PushEnvironment::Development);
            store
                .update_push_registration(GRANT, PERSON, ACTOR, Some(&input), || 1000)
                .unwrap()
                .unwrap();
        }
        let clock = Arc::new(AtomicU64::new(1000));
        let samples = Arc::new(AtomicUsize::new(0));
        let held = store.connection.write();
        // Observe actual queue admission, not merely the worker thread starting.
        // This is the writer_queue tests' public jobs-channel interception convention.
        let (proxy, incoming) = mpsc::channel();
        let original = store.connection.jobs.lock().unwrap().replace(proxy).unwrap();
        let worker_store = store.clone();
        let worker_clock = clock.clone();
        let worker_samples = samples.clone();
        let worker = std::thread::spawn(move || {
            let input = registration(TOKEN, PushEnvironment::Development);
            worker_store.update_push_registration(
                GRANT,
                PERSON,
                ACTOR,
                register.then_some(&input),
                || {
                    worker_samples.fetch_add(1, Ordering::SeqCst);
                    worker_clock.load(Ordering::SeqCst)
                },
            )
        });
        let job = incoming
            .recv_timeout(Duration::from_secs(10))
            .expect("push mutation must enqueue before the clock advances");
        assert!(matches!(&job, smallclaims::sqlite::WriterJob::Batched { .. }));
        original.send(job).unwrap();
        *store.connection.jobs.lock().unwrap() = Some(original);
        assert_eq!(clock.load(Ordering::SeqCst), 1000);
        assert_eq!(samples.load(Ordering::SeqCst), 0, "clock must wait for writer admission");
        clock.store(2001, Ordering::SeqCst);
        drop(held);
        assert!(
            worker.join().unwrap().unwrap().is_none(),
            "both POST and DELETE must reject an expired grant"
        );
        assert_eq!(samples.load(Ordering::SeqCst), 1);
        assert_eq!(count(&store), 0, "rejection must leave no registration row");
    }
}

#[test]
fn push_revoke_and_grant_replacement_delete_secrets_without_attention_reads() {
    let store = Store::open_memory("push-owner").unwrap();
    grant(&store, GRANT, PERSON, ACTOR, "device/phone", 2000);
    let input = registration(TOKEN, PushEnvironment::Development);
    store
        .update_push_registration(GRANT, PERSON, ACTOR, Some(&input), || 1000)
        .unwrap()
        .unwrap();
    claim(&store, GRANT, "custom.client.pairing-revoked", json!({}));
    assert_eq!(
        count(&store),
        0,
        "claim admission removes the secret synchronously"
    );
    assert!(
        store
            .update_push_registration(GRANT, PERSON, ACTOR, Some(&input), || 1000)
            .unwrap()
            .is_none()
    );
    grant(&store, GRANT, PERSON, ACTOR, "device/phone", 3000);
    store
        .update_push_registration(GRANT, PERSON, ACTOR, Some(&input), || 1000)
        .unwrap()
        .unwrap();
    assert_eq!(
        count(&store),
        1,
        "an older revocation does not revoke a new grant"
    );
    grant(&store, GRANT, PERSON, ACTOR, "device/phone", 4000);
    assert_eq!(
        count(&store),
        0,
        "a new grant must not inherit an old token"
    );
}

fn replicate(from: &Store, to: &Store) -> String {
    let exchange = from
        .export_replication_exchange("push-test-fleet", &to.replication_inventory().unwrap())
        .unwrap();
    let wire = serde_json::to_string(&exchange).unwrap();
    to.receive_replication_exchange(from.origin(), "push-test-fleet", &exchange)
        .unwrap();
    to.validate_replication_backlog().unwrap();
    to.apply_replication_repairs().unwrap();
    assert!(to.project_replication_backlog().unwrap());
    wire
}

#[test]
fn push_replicated_revocation_removes_registration_without_a_read_and_exports_no_secret() {
    let owner = Store::open_memory("push-owner").unwrap();
    let peer = Store::open_memory("push-peer").unwrap();
    grant(&owner, GRANT, PERSON, ACTOR, "device/phone", 2000);
    let input = registration(TOKEN, PushEnvironment::Development);
    owner
        .update_push_registration(GRANT, PERSON, ACTOR, Some(&input), || 1000)
        .unwrap()
        .unwrap();
    let wire = replicate(&owner, &peer);
    assert!(!wire.contains(TOKEN));
    assert_eq!(count(&peer), 0, "local registration is never replicated");
    assert!(
        !serde_json::to_string(&owner.claims_for(GRANT, None).unwrap())
            .unwrap()
            .contains(TOKEN)
    );
    assert!(
        peer.update_push_registration(GRANT, PERSON, ACTOR, Some(&input), || 1000)
            .unwrap()
            .is_none(),
        "only the pairing issuer stores a registration"
    );
    claim(&peer, GRANT, "custom.client.pairing-revoked", json!({}));
    assert!(!replicate(&peer, &owner).contains(TOKEN));
    assert_eq!(count(&owner), 0);
}

#[test]
fn push_local_schema_upgrades_old_store_and_registration_survives_restart_then_expires() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("graph.db");
    {
        let store = Store::open(&path, "push-owner").unwrap();
        // A prior build has the ordinary graph schema but no push-local table or trigger.
        store
            .connection
            .lock()
            .expect("fixture writer")
            .execute_batch(
                "DROP TRIGGER local_push_pairing_changed; DROP TABLE local_push_registrations;",
            )
            .unwrap();
    }
    {
        let store = Store::open(&path, "push-owner").unwrap();
        grant(&store, GRANT, PERSON, ACTOR, "device/phone", 2000);
        let input = registration(TOKEN, PushEnvironment::Development);
        store
            .update_push_registration(GRANT, PERSON, ACTOR, Some(&input), || 1000)
            .unwrap()
            .unwrap();
    }
    let store = Store::open(&path, "push-owner").unwrap();
    assert_eq!(count(&store), 1);
    assert_eq!(
        store.push_registration_statuses(PERSON, 1999).unwrap()["device/phone"].environment,
        Some(PushEnvironment::Development)
    );
    assert!(
        store
            .push_registration_statuses(PERSON, 2000)
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.sweep_push_registrations(1999).unwrap(), 0);
    assert_eq!(store.sweep_push_registrations(2000).unwrap(), 1);
    assert_eq!(store.sweep_push_registrations(2001).unwrap(), 0);
    assert_eq!(count(&store), 0);
}
