//! Signed admission retains one committed message/signature and no temporary expectation.
use super::*;
use smallclaims::principal::{ClaimSignature, FIELDS_FORMAT, Judged, fields_signing_bytes};

fn request() -> ClaimInput {
    ClaimInput {
        subject: "message/local-signed-send".into(),
        kind: "message.sent".into(),
        actor: Some("person/eval".into()),
        fields: BTreeMap::from([
            ("from".into(), json!("person/eval")),
            ("to".into(), json!("agent/eval.recipient")),
            ("content".into(), json!("One durable local send")),
            ("status".into(), json!("sent")),
        ]),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: Some("local-signed-send-request".into()),
    }
}

fn signed(input: &ClaimInput) -> ClaimSignature {
    let (key, _) = smallclaims::fleet::MemberKey::generate().unwrap();
    let mut signature = ClaimSignature::sign(
        &key,
        "",
        input.actor.as_deref().unwrap(),
        None,
        vec![],
        now_ms() as u64,
    );
    signature.format = Some(FIELDS_FORMAT.into());
    signature.signed_fields = vec!["content".into(), "from".into(), "to".into()];
    let fields = serde_json::to_value(&input.fields).unwrap();
    signature.signature = key.sign(&fields_signing_bytes(
        &input.subject,
        &input.kind,
        input.actor.as_deref(),
        &fields,
        &signature.signed_fields,
        &signature.signer,
        None,
        &signature.key,
        &signature.chain,
        &signature.nonce,
        signature.signed_at_unix_ms,
    ));
    signature
}

#[test]
fn signed_send_commits_once_and_retries_keep_the_original_signature() {
    let store = Store::open_memory("node").unwrap();
    let input = request();
    let signature = signed(&input);
    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let observed = snapshots.clone();
    let _observer = store.connection.observe_commits(move |connection| {
        let counts: (u64,u64) = connection.query_row(
            "SELECT (SELECT COUNT(*) FROM claims WHERE subject='message/local-signed-send'),(SELECT COUNT(*) FROM expected_claim_signatures)",
            [], |row| Ok((row.get(0)?,row.get(1)?)),
        ).unwrap();
        observed.lock().unwrap().push(counts);
    });
    let (record, appended) = store.append_signed_message(&input, &signature).unwrap();
    assert!(appended);
    assert_eq!(
        *snapshots.lock().unwrap(),
        [(1, 0)],
        "ACK follows one committed message and consumed signature"
    );
    let held = store.claim_signature(&record.id).unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&held).unwrap(),
        serde_json::to_value(&signature).unwrap()
    );
    assert!(held.verifies(&Judged {
        id: &record.id,
        subject: &record.subject,
        kind: &record.kind,
        actor: record.actor.as_deref(),
        content: String::new(),
        fields: &record.body["fields"]
    }));
    let repeat_signature = signed(&input);
    let (repeat, appended) = store
        .append_signed_message(&input, &repeat_signature)
        .unwrap();
    assert!(!appended);
    assert_eq!(repeat.id, record.id);
    assert_eq!(
        store.claim_signature(&record.id).unwrap().unwrap().nonce,
        signature.nonce
    );
    assert!(
        !store
            .signature_nonce_used(&repeat_signature.key, &repeat_signature.nonce)
            .unwrap()
    );
    assert!(
        snapshots
            .lock()
            .unwrap()
            .iter()
            .all(|counts| *counts == (1, 0))
    );
    let mut conflict = input;
    conflict
        .fields
        .insert("content".into(), json!("A different send"));
    assert_eq!(
        store
            .append_signed_message(&conflict, &signed(&conflict))
            .unwrap_err()
            .code,
        "idempotency-conflict"
    );
    assert_eq!(
        store
            .readers
            .get()
            .query_row(
                "SELECT COUNT(*) FROM expected_claim_signatures",
                [],
                |row| row.get::<_, u64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn failed_signed_send_rolls_back_its_message_and_temporary_signature() {
    let store = Store::open_memory("node").unwrap();
    store.connection.write().execute_batch(
        "CREATE TRIGGER reject_fixture_send BEFORE INSERT ON claims
         WHEN NEW.subject='message/local-signed-send' BEGIN SELECT RAISE(ABORT,'injected send failure'); END;",
    ).unwrap();
    let input = request();
    let signature = signed(&input);
    assert!(store.append_signed_message(&input, &signature).is_err());
    assert!(store.message(&input.subject).unwrap().is_none());
    assert!(
        !store
            .signature_nonce_used(&signature.key, &signature.nonce)
            .unwrap()
    );
    assert_eq!(
        store
            .readers
            .get()
            .query_row(
                "SELECT COUNT(*) FROM expected_claim_signatures",
                [],
                |row| row.get::<_, u64>(0)
            )
            .unwrap(),
        0
    );
    store
        .connection
        .write()
        .execute_batch("DROP TRIGGER reject_fixture_send")
        .unwrap();
    assert!(store.append_signed_message(&input, &signature).unwrap().1);
    assert_eq!(
        store.message(&input.subject).unwrap().unwrap().status,
        "sent"
    );
}
