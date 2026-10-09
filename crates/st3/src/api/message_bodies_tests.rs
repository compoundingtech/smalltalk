//! A message past the inline limit keeps a preview in its claim and the whole text in a file on
//! the member that took it. Each test owns its stores; no daemon, fleet or seat is touched.
use super::client_v0::ClientSession;
use super::*;
use crate::message_body::{INLINE_MAX_BYTES, MAX_BYTES};

fn text(bytes: usize) -> String {
    // Multi-byte characters, so a cut at the wrong byte would show.
    let mut text = String::new();
    let mut number = 0;
    while text.len() < bytes {
        text.push_str(&format!("line {number} é — pasted notes\n"));
        number += 1;
    }
    while text.len() > bytes {
        text.pop();
    }
    text
}

fn send(state: &AppState, key: &str, content: &str) -> Result<MessageSendReceipt, ApiError> {
    accept_message_receipt(
        state,
        MessageSendRequest {
            idempotency_key: key.into(),
            from: "person/alex".into(),
            to: "agent/worker".into(),
            content: content.into(),
            title: Some("notes".into()),
            in_reply_to: None,
            tags: Vec::new(),
            attachments: Vec::new(),
        },
        None,
        None,
    )
}

async fn read_body(state: &AppState, subject: &str) -> Value {
    let Json(value) = message_bodies::body(State(state.clone()), AxumPath(subject.into()))
        .await
        .unwrap();
    assert_eq!(value["message"], subject);
    value
}

async fn full_text(state: &AppState, subject: &str) -> String {
    let value = read_body(state, subject).await;
    assert_eq!(value["complete"], true);
    value["text"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn bodies_of_4_kib_100_kib_and_256_kib_send_and_read_whole() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    for (size, long) in [
        (INLINE_MAX_BYTES, false),
        (100 * 1024, true),
        (MAX_BYTES, true),
    ] {
        let body = text(size);
        assert_eq!(body.len(), size);
        let receipt = send(&state, &format!("body-{size}"), &body).unwrap();
        let message = state.store.message(&receipt.message.subject).unwrap().unwrap();
        // The claim, the receipt and every list read hold the preview; the text is read on demand.
        assert_eq!(message.body_ref.is_some(), long, "{size}");
        assert_eq!(receipt.message.body_ref, message.body_ref);
        if long {
            assert_eq!(message.body_bytes, Some(size as u64));
            assert!(message.content.len() <= crate::message_body::PREVIEW_BYTES + 3);
            assert!(body.starts_with(message.content.trim_end_matches('…')));
        } else {
            assert_eq!(message.content, body);
            assert_eq!(message.body_bytes, None);
        }
        assert_eq!(full_text(&state, &message.subject).await, body);
        if long {
            // The text is a file on this member, and the database holds none of it.
            let hash = crate::message_body::parse_reference(message.body_ref.as_deref().unwrap()).unwrap();
            assert_eq!(
                crate::message_body::bodies(root.path()).read("person/alex", &message.subject).unwrap().unwrap(),
                body.as_bytes()
            );
            assert!(state.store.get_blob(hash).unwrap().is_none());
            let claim = state.store.latest_claim(&message.subject, Some("message.sent")).unwrap().unwrap();
            assert!(serde_json::to_string(&claim.body).unwrap().len() < 2048);
        }
        // The client resource names the body only when there is one.
        let resources = client_message_resources(&state.store, None, true, None).unwrap();
        let resource = resources
            .iter()
            .find(|resource| resource["id"] == message.subject)
            .unwrap();
        assert_eq!(resource.get("body_ref").is_some(), long);
        assert_eq!(resource.get("body_bytes").is_some(), long);
    }
}

#[tokio::test]
async fn a_body_past_the_limit_is_refused_in_words_that_name_it() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    for size in [MAX_BYTES + 1, 1024 * 1024] {
        let error = send(&state, &format!("too-long-{size}"), &text(size)).unwrap_err();
        assert_eq!(error.code, "message-too-large");
        assert!(error.message.contains("256 KiB"), "{}", error.message);
        assert!(!error.message.contains("document"), "{}", error.message);
    }
    // Nothing was written for the refusals.
    assert!(state.store.messages(Some("agent/worker"), true).unwrap().is_empty());
    // Documents keep their own reference form, which is not a long body.
    let error = send(&state, "doc-form", &format!("doc/notes@{}", "0".repeat(64))).unwrap_err();
    assert_eq!(error.code, "missing-document");
}

#[tokio::test]
async fn a_repeated_send_of_a_long_body_sends_once() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    let body = text(50 * 1024);
    let first = send(&state, "once", &body).unwrap();
    let again = send(&state, "once", &body).unwrap();
    assert!(!first.already_sent);
    assert!(again.already_sent);
    assert_eq!(first.message.subject, again.message.subject);
    assert_eq!(again.message.body_ref, first.message.body_ref);
    assert_eq!(state.store.messages(Some("agent/worker"), true).unwrap().len(), 1);
}

#[tokio::test]
async fn a_client_reads_a_body_only_in_a_conversation_it_may_read() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    let body = text(40 * 1024);
    let subject = send(&state, "private", &body).unwrap().message.subject;
    let ask = |actor: &str, scopes: &[&str]| {
        let session = ClientSession::for_tests_scoped(actor, actor, scopes);
        message_bodies::client_body(State(state.clone()), Extension(session), AxumPath(subject.clone()))
    };
    // A participant, and an agent seat, read it.
    let Json(value) = ask("person/alex", &["read.projections"]).await.unwrap();
    assert_eq!(value["text"], body.as_str());
    assert_eq!(value["bytes"], body.len());
    let _ = ask("agent/worker", &["read.projections"]).await.unwrap();
    // A session without the read scope does not.
    assert_eq!(ask("person/alex", &[]).await.unwrap_err().code, "forbidden");
    // A message of one agent to another is readable by anyone in the fleet (free mode), so
    // a person's private conversation is the case that must be refused.
    let private = send_between(&state, "person/blair", "person/avery", "p2p", &body);
    let error = ask_for(&state, "person/alex", &private).await.unwrap_err();
    assert_eq!(error.code, "forbidden");
    let _ = ask_for(&state, "person/blair", &private).await.unwrap();
    let _ = ask_for(&state, "person/avery", &private).await.unwrap();
}

fn send_between(state: &AppState, from: &str, to: &str, key: &str, content: &str) -> String {
    accept_message_receipt(
        state,
        MessageSendRequest {
            idempotency_key: key.into(),
            from: from.into(),
            to: to.into(),
            content: content.into(),
            title: None,
            in_reply_to: None,
            tags: Vec::new(),
            attachments: Vec::new(),
        },
        None,
        None,
    )
    .unwrap()
    .message
    .subject
}

async fn ask_for(
    state: &AppState,
    actor: &str,
    subject: &str,
) -> Result<Json<Value>, ApiError> {
    let session = ClientSession::for_tests_scoped(actor, actor, &["read.projections"]);
    message_bodies::client_body(State(state.clone()), Extension(session), AxumPath(subject.into()))
        .await
}

#[tokio::test]
async fn an_old_message_reads_as_it_always_did() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    // A claim written before long bodies: no blob, no body fields.
    state
        .store
        .append_claim(&ClaimInput {
            subject: "message/old-format".into(),
            kind: "message.sent".into(),
            actor: Some("person/alex".into()),
            fields: BTreeMap::from([
                ("from".into(), json!("person/alex")),
                ("to".into(), json!("agent/worker")),
                ("content".into(), json!("An older note")),
                ("status".into(), json!("sent")),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("old".into()),
        })
        .unwrap();
    let message = state.store.message("message/old-format").unwrap().unwrap();
    assert_eq!(message.content, "An older note");
    assert_eq!((&message.body_ref, &message.body_bytes), (&None, &None));
    assert_eq!(full_text(&state, "message/old-format").await, "An older note");
    let view = serde_json::to_value(&message).unwrap();
    assert!(view.get("body_ref").is_none() && view.get("body_bytes").is_none());
    // A view an older daemon wrote reads back without the fields.
    let older: MessageView = serde_json::from_value(json!({
        "subject": "message/x", "from": "person/alex", "to": "agent/worker",
        "content": "hi", "status": "sent", "created_index": 1
    }))
    .unwrap();
    assert_eq!(older.body_ref, None);
}

const FLEET: &str = "5b4b1a8e-7c3d-4e2f-9a1b-0c6d5e4f3a2b";

fn node_state(root: &Path, name: &str) -> AppState {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    let store = Store::open(&dir.join("graph.db"), name).unwrap();
    store.bind_fleet(FLEET).unwrap();
    AppState {
        store: Arc::new(store),
        node: name.into(),
        state_dir: dir,
        ..super::tests::state(root)
    }
}

/// Everything `from` holds, delivered to `to` and admitted the way a peer exchange does it.
/// Returns the bytes of the envelopes that moved.
fn replicate(from: &Store, to: &Store) -> usize {
    let relay = from.origin().to_owned();
    let exchange = from
        .export_replication_exchange(FLEET, &to.replication_inventory().unwrap())
        .unwrap();
    let moved = exchange
        .envelopes
        .iter()
        .map(|envelope| envelope.payload.bytes().unwrap().len())
        .sum();
    to.receive_replication_exchange(&relay, FLEET, &exchange).unwrap();
    to.validate_replication_backlog().unwrap();
    to.apply_replication_repairs().unwrap();
    assert!(to.project_replication_backlog().unwrap());
    moved
}

#[tokio::test]
async fn a_long_message_replicates_as_a_preview_and_the_text_stays_with_its_owner() {
    let root = tempfile::tempdir().unwrap();
    let writer = node_state(root.path(), "writer");
    let reader = node_state(root.path(), "reader");
    let mut sent = Vec::new();
    for size in [INLINE_MAX_BYTES, 100 * 1024, MAX_BYTES] {
        let body = text(size);
        let subject = send(&writer, &format!("replicated-{size}"), &body)
            .unwrap()
            .message
            .subject;
        sent.push((subject, body));
    }
    // The exchange carries claims, not texts: 360 KiB of text moves as a few KiB of claims.
    let moved = replicate(&writer.store, &reader.store);
    assert!(moved < 16 * 1024, "{moved} bytes moved");
    for (subject, body) in &sent {
        let there = reader.store.message(subject).unwrap().unwrap();
        let here = writer.store.message(subject).unwrap().unwrap();
        assert_eq!(there.content, here.content);
        assert_eq!(there.body_ref, here.body_ref);
        assert_eq!(there.body_bytes, here.body_bytes);
        assert_eq!(there.body_origin.as_deref(), there.body_ref.is_some().then_some("host/writer"));
        // The reader has no copy of any text, in a table or a file.
        if let Some(reference) = &there.body_ref {
            let hash = crate::message_body::parse_reference(reference).unwrap();
            assert!(reader.store.get_blob(hash).unwrap().is_none());
            assert!(!crate::message_body::bodies(&reader.state_dir).exists("person/alex", subject));
            // With no route to the owner, it is handed the preview and told where the text is.
            let unreachable = read_body(&reader, subject).await;
            assert_eq!(unreachable["complete"], false);
            let text = unreachable["text"].as_str().unwrap();
            assert!(text.starts_with(&there.content) && text.contains("host/writer"), "{text}");
        } else {
            assert_eq!(there.content, *body);
        }
        assert_eq!(full_text(&writer, subject).await, *body);
    }
    // Both members hold the same graph.
    let digest = |state: &AppState| {
        state.store.replication_status(true, Some(FLEET), &[]).unwrap().graph_digest
    };
    assert_eq!(digest(&writer), digest(&reader));
}

#[tokio::test]
async fn a_claim_naming_a_body_is_accepted_by_a_build_that_predates_bodies() {
    // The body is an `attachments` entry of a type no image has, so no claim field is new: a
    // build that keeps only image entries reads the message as its preview.
    let root = tempfile::tempdir().unwrap();
    let writer = node_state(root.path(), "writer");
    let subject = send(&writer, "old-reader", &text(50 * 1024)).unwrap().message.subject;
    let claim = writer.store.latest_claim(&subject, Some("message.sent")).unwrap().unwrap();
    let fields = claim.body["fields"].as_object().unwrap();
    let registry = st3_schema::registry();
    registry
        .validate_claim(
            &subject,
            "message.sent",
            &fields.clone().into_iter().collect(),
        )
        .unwrap();
    let old_view_images: Vec<_> = serde_json::from_value::<Vec<crate::model::MessageAttachment>>(
        fields["attachments"].clone(),
    )
    .unwrap()
    .into_iter()
    .filter(|attachment| crate::blobs::MEDIA_TYPES.contains(&attachment.media_type.as_str()))
    .collect();
    assert!(old_view_images.is_empty());
    assert!(fields["content"].as_str().unwrap().ends_with('…'));
}

/// What one message costs to keep and to move, and how long its text takes to read. Printed for
/// the pull request; asserted only as bounds.
#[tokio::test]
async fn what_a_message_costs_per_size() {
    let root = tempfile::tempdir().unwrap();
    let writer = node_state(root.path(), "writer");
    let reader = node_state(root.path(), "reader");
    for size in [4 * 1024, 8 * 1024, 64 * 1024, 256 * 1024] {
        let body = text(size);
        let subject = send(&writer, &format!("cost-{size}"), &body).unwrap().message.subject;
        let claim = writer.store.latest_claim(&subject, Some("message.sent")).unwrap().unwrap();
        let claim_row = serde_json::to_string(&claim.body).unwrap().len();
        let moved = replicate(&writer.store, &reader.store);
        let started = std::time::Instant::now();
        let read = full_text(&writer, &subject).await;
        let latency = started.elapsed();
        assert_eq!(read.len(), size);
        let file = writer
            .store
            .message(&subject)
            .unwrap()
            .unwrap()
            .body_ref
            .map(|_reference| {
                crate::message_body::bodies(&writer.state_dir)
                    .read_range("person/alex", &subject, 0, 1)
                    .unwrap()
                    .map_or(0, |(size, _)| size)
            })
            .unwrap_or_default();
        eprintln!(
            "cost: body {size:>7} B | claim row {claim_row:>5} B | blob row 0 B | owner file {file:>7} B | exchange {moved:>6} B | local read {latency:?}"
        );
        assert!(claim_row < if size <= INLINE_MAX_BYTES { size + 1024 } else { 2 * 1024 }, "{claim_row}");
        assert!(moved < claim_row + 2048, "{moved}");
    }
}

fn files_in(state: &AppState) -> usize {
    crate::message_body::bodies(&state.state_dir).files().len()
}

#[tokio::test]
async fn a_refused_or_failed_send_leaves_no_file_and_a_repeat_heals_a_lost_one() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    let body = text(50 * 1024);
    // Over the limit, and a different text under a key already used: nothing is written.
    assert!(send(&state, "refused", &text(MAX_BYTES + 1)).is_err());
    assert_eq!(files_in(&state), 0);
    let first = send(&state, "lifecycle", &body).unwrap().message.subject;
    assert_eq!(files_in(&state), 1);
    let conflict = send(&state, "lifecycle", &text(60 * 1024)).unwrap_err();
    assert_eq!(conflict.code, "idempotency-mismatch");
    assert_eq!(files_in(&state), 1, "the conflicting text left no file");
    // A signature that does not cover a long message is refused before anything is written.
    let legacy = {
        let (key, _) = smallclaims::fleet::MemberKey::generate().unwrap();
        let mut signature = smallclaims::principal::ClaimSignature::sign(
            &key,
            "",
            "person/alex",
            None,
            vec![],
            client_now_ms() as u64,
        );
        signature.format = Some(smallclaims::principal::FIELDS_FORMAT.into());
        signature.signed_fields = SIGNED_MESSAGE_FIELDS.iter().map(|name| (*name).to_owned()).collect();
        signature
    };
    let error = accept_message_receipt(
        &state,
        MessageSendRequest {
            idempotency_key: "signed-whole".into(),
            from: "person/alex".into(),
            to: "agent/worker".into(),
            content: body.clone(),
            title: None,
            in_reply_to: None,
            tags: Vec::new(),
            attachments: Vec::new(),
        },
        None,
        Some(legacy),
    )
    .unwrap_err();
    assert_eq!(error.code, "long-message-signature-unsupported");
    assert!(error.message.contains("8 KiB"), "{}", error.message);
    assert_eq!(files_in(&state), 1);
    assert_eq!(state.store.messages(Some("agent/worker"), true).unwrap().len(), 1);
    // A write that was lost after its claim landed is written again by a repeat of the send.
    message_body::bodies(&state.state_dir).remove("person/alex", &first);
    assert_eq!(read_body(&state, &first).await["complete"], false);
    let again = send(&state, "lifecycle", &body).unwrap();
    assert!(again.already_sent);
    assert_eq!(full_text(&state, &first).await, body);
}

#[test]
fn what_one_sender_and_one_machine_keep_is_bounded_and_orphans_are_reconciled() {
    let root = tempfile::tempdir().unwrap();
    let bodies = message_body::bodies(root.path());
    // A sender at its share is refused for a new message, and still may repeat an old one.
    let share = (message_body::ACTOR_STORE_DEFAULT_MB * 1024 * 1024) as usize;
    let first = vec![b'a'; MAX_BYTES];
    let per_sender = share / MAX_BYTES;
    for number in 0..per_sender {
        let subject = format!("message/held-{number}");
        bodies.ensure_room("person/alex", &subject, first.len()).unwrap();
        bodies.put("person/alex", &subject, &first).unwrap();
    }
    let error = bodies.ensure_room("person/alex", "message/one-more", 1).unwrap_err();
    assert_eq!(error.code, "message-store-full");
    assert!(error.message.contains("person/alex"), "{}", error.message);
    bodies.ensure_room("person/alex", "message/held-0", first.len()).unwrap();
    // Another sender is not held back by the first.
    bodies.ensure_room("person/blair", "message/fresh", first.len()).unwrap();
    // A file no claim names is removed once it is old; a young one is left, as is a named one.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
    let age = |actor: &str, subject: &str| {
        std::fs::OpenOptions::new()
            .write(true)
            .open(bodies_path(root.path(), actor, subject))
            .unwrap()
            .set_modified(old)
            .unwrap();
    };
    bodies.put("person/blair", "message/orphan", b"x").unwrap();
    bodies.put("person/blair", "message/young-orphan", b"x").unwrap();
    bodies.put("person/blair", "message/kept", b"x").unwrap();
    age("person/blair", "message/orphan");
    age("person/blair", "message/kept");
    let removed = bodies.reconcile(|file| file.id == "kept");
    assert_eq!(removed, 1);
    assert!(!bodies.exists("person/blair", "message/orphan"));
    assert!(bodies.exists("person/blair", "message/young-orphan"));
    assert!(bodies.exists("person/blair", "message/kept"));
}

fn bodies_path(root: &Path, actor: &str, subject: &str) -> std::path::PathBuf {
    let dir = hex::encode(Sha256::digest(actor.as_bytes()))[..16].to_owned();
    root.join("message-bodies").join(dir).join(subject.trim_start_matches("message/"))
}

#[test]
fn a_device_signature_over_the_preview_hash_and_size_is_carried_and_judged_everywhere() {
    use smallclaims::principal::{ClaimSignature, FIELDS_FORMAT, fields_signing_bytes};
    let store = Store::open_memory("node").unwrap();
    let body = text(60 * 1024);
    let preview = message_body::preview(&body);
    let hash = hex::encode(Sha256::digest(body.as_bytes()));
    let entry = crate::model::MessageAttachment {
        sha256: hash.clone(),
        media_type: message_body::MEDIA_TYPE.into(),
        name: None,
        size: body.len() as u64,
        origin: "host/node".into(),
    };
    let input = |key: &str| ClaimInput {
        subject: format!("message/{key}"),
        kind: "message.sent".into(),
        actor: Some("person/alex".into()),
        fields: BTreeMap::from([
            ("from".into(), json!("person/alex")),
            ("to".into(), json!("agent/worker")),
            ("content".into(), json!(preview)),
            ("status".into(), json!("sent")),
            ("attachments".into(), json!([entry])),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(key.into()),
    };
    let sign = |input: &ClaimInput, hash: &str, bytes: u64| {
        let (key, _) = smallclaims::fleet::MemberKey::generate().unwrap();
        let mut signature = ClaimSignature::sign(&key, "", "person/alex", None, vec![], client_now_ms() as u64);
        signature.format = Some(FIELDS_FORMAT.into());
        signature.signed_fields = SIGNED_LONG_MESSAGE_FIELDS.iter().map(|name| (*name).to_owned()).collect();
        let mut signed = input.fields.clone();
        signed.insert("body_sha256".into(), json!(hash));
        signed.insert("body_bytes".into(), json!(bytes));
        signature.signature = key.sign(&fields_signing_bytes(
            &input.subject,
            &input.kind,
            input.actor.as_deref(),
            &serde_json::to_value(&signed).unwrap(),
            &signature.signed_fields,
            &signature.signer,
            None,
            &signature.key,
            &signature.chain,
            &signature.nonce,
            signature.signed_at_unix_ms,
        ));
        signature
    };
    let mismatch = Some("the signature does not match the claim");
    let honest = input("long-signed");
    let (record, _) = store
        .append_signed_message(&honest, &sign(&honest, &hash, body.len() as u64))
        .unwrap();
    // A signature over another text's hash or length does not match this claim.
    let other = input("long-forged");
    let (forged, _) = store
        .append_signed_message(&other, &sign(&other, &"0".repeat(64), body.len() as u64))
        .unwrap();
    let wrong_size = input("long-wrong-size");
    let (resized, _) = store
        .append_signed_message(&wrong_size, &sign(&wrong_size, &hash, body.len() as u64 + 1))
        .unwrap();
    store.recheck_claim_verdicts().unwrap();
    assert_ne!(store.claim_verdict(&record.id).unwrap().reason(), mismatch);
    assert_eq!(store.claim_verdict(&forged.id).unwrap().reason(), mismatch);
    assert_eq!(store.claim_verdict(&resized.id).unwrap().reason(), mismatch);
}

#[tokio::test]
async fn a_backup_carries_the_long_bodies_a_member_owns_and_a_restore_proves_them() {
    let root = tempfile::tempdir().unwrap();
    let writer = node_state(root.path(), "writer");
    let sent: Vec<(String, String)> = [100 * 1024, MAX_BYTES]
        .into_iter()
        .map(|size| {
            let body = text(size);
            let subject = send(&writer, &format!("backed-{size}"), &body).unwrap().message.subject;
            (subject, body)
        })
        .collect();
    // A stray file and a file whose claim names another text are not archived.
    let bodies = message_body::bodies(&writer.state_dir);
    bodies.put("person/alex", "message/stray", b"nothing names me").unwrap();
    let mut archive = Vec::new();
    writer
        .store
        .write_backup_with_bodies(&mut archive, Some(&bodies))
        .unwrap();
    let archive_path = root.path().join("claims.jsonl");
    std::fs::write(&archive_path, &archive).unwrap();
    crate::backup::verify_file(&archive_path).unwrap();
    let archived = String::from_utf8_lossy(&archive);
    assert_eq!(archived.matches("\"record\":\"message-body\"").count(), 2);
    assert!(!archived.contains("message/stray"));
    // Restored into a fresh state directory, every body is back, matched to its claim by hash.
    let restored_dir = root.path().join("restored");
    let report = crate::backup::restore(&archive_path, &restored_dir.join("claims.sqlite3")).unwrap();
    assert_eq!(report.message_bodies_restored, 2);
    assert_eq!(report.message_bodies_unmatched, 0);
    assert_eq!(report.message_bodies_referenced, 2);
    let restored = message_body::bodies(&restored_dir);
    for (subject, body) in &sent {
        assert_eq!(restored.read("person/alex", subject).unwrap().unwrap(), body.as_bytes());
    }
    assert!(!restored.exists("person/alex", "message/stray"));
    // A body that is not the text its record names fails verification before anything is restored.
    let tampered = archived.replacen("\"data\":\"", "\"data\":\"AAAA", 1);
    let tampered_path = root.path().join("tampered.jsonl");
    std::fs::write(&tampered_path, tampered.as_bytes()).unwrap();
    assert!(crate::backup::verify_file(&tampered_path).is_err());
    // An archive without the texts, as before this change, still restores; the report says how
    // many long messages it names whose text it could not return.
    let bare = {
        let mut bare = Vec::new();
        writer.store.write_backup(&mut bare).unwrap();
        bare
    };
    let bare_path = root.path().join("bare.jsonl");
    std::fs::write(&bare_path, &bare).unwrap();
    let report = crate::backup::restore(&bare_path, &root.path().join("bare").join("claims.sqlite3")).unwrap();
    assert_eq!((report.message_bodies_restored, report.message_bodies_referenced), (0, 2));
}
