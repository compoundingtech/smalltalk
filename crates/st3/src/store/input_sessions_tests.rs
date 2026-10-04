use super::checkpoint::{delete_dropped_rows_tx, record_checkpoint_tombstones_tx};
use super::*;
use st3_schema::input_sessions::{InputSessionCloseReason, InputSessionEvent, InputSessionRecord};

const EPOCH: &str = "31caee5f-560b-42f3-9f03-d9f74b03ebad";
const NEXT_EPOCH: &str = "98167767-37aa-4350-8d25-45f2932fd446";
const TERMINAL: &str = "agent/example/worker";

fn opened(owner: &str, session: u128, person: &str, at: u64) -> InputSessionRecord {
    InputSessionRecord {
        version: 1,
        ordinal: 0,
        event: InputSessionEvent::Opened,
        session_id: Uuid::from_u128(session).to_string(),
        owner: owner.into(),
        owner_epoch: EPOCH.into(),
        terminal: TERMINAL.into(),
        incarnation: "incarnation-one".into(),
        attachment: "custom/client/terminal-attachment-example".into(),
        attachment_claim: "viewer-claim".into(),
        device_id: Some("device-one".into()),
        device_actor: "client/device-one".into(),
        authority_actor: person.into(),
        person: Some(person.into()),
        pairing_claim: Some("pairing-claim".into()),
        opened_at_unix_ms: at,
        observed_at_unix_ms: at,
        successful_send_bytes: 0,
        successful_batches: 0,
        uncertain_handoff: false,
        reason: None,
    }
}

fn progress(record: &InputSessionRecord, bytes: u64, batches: u64) -> InputSessionRecord {
    let mut next = record.clone();
    next.ordinal += 1;
    next.event = InputSessionEvent::Checkpoint;
    next.observed_at_unix_ms += 1;
    next.successful_send_bytes = bytes;
    next.successful_batches = batches;
    next
}

fn close(record: &InputSessionRecord) -> InputSessionRecord {
    let mut next = record.clone();
    next.ordinal += 1;
    next.event = InputSessionEvent::Closed;
    next.observed_at_unix_ms += 1;
    next.reason = Some(InputSessionCloseReason::ClientClose);
    next
}

fn records(page: &Value) -> Vec<InputSessionRecord> {
    serde_json::from_value(page["items"].clone()).unwrap()
}

#[test]
fn publication_is_immutable_monotonic_and_idempotent_even_after_close() {
    let store = Store::open_memory("amber").unwrap();
    let max = st3_schema::input_sessions::MAX_AUDIT_INTEGER;
    let opening = opened("amber", 1, "person/avery", 100);
    let first = store.append_input_session(&opening).unwrap();
    assert_eq!(store.append_input_session(&opening).unwrap().id, first.id);
    let checkpoint = progress(&opening, max - 1, 3);
    let durable = store.append_input_session(&checkpoint).unwrap();
    assert_eq!(durable.predecessors, vec![first.id]);
    let mut next = progress(&checkpoint, max, 4);
    next.successful_send_bytes = 1;
    next.successful_batches = 1;
    assert_eq!(
        store.append_input_session(&next).unwrap_err().code,
        "input-session-regression"
    );
    next = progress(&checkpoint, max, 4);
    next.attachment_claim = "different-viewer".into();
    assert_eq!(
        store.append_input_session(&next).unwrap_err().code,
        "input-session-identity-changed"
    );
    next = progress(&checkpoint, max, 4);
    next.ordinal += 1;
    assert_eq!(
        store.append_input_session(&next).unwrap_err().code,
        "stale-input-session"
    );
    let closed = close(&checkpoint);
    let final_claim = store.append_input_session(&closed).unwrap();
    assert_eq!(
        store.append_input_session(&checkpoint).unwrap().id,
        durable.id
    );
    assert_eq!(
        store.append_input_session(&closed).unwrap().id,
        final_claim.id
    );
    let mut conflict = closed.clone();
    conflict.successful_send_bytes = max;
    assert_eq!(
        store.append_input_session(&conflict).unwrap_err().code,
        "input-session-conflict"
    );
    next = progress(&closed, max, 4);
    next.reason = None;
    assert_eq!(
        store.append_input_session(&next).unwrap_err().code,
        "input-session-ended"
    );
    assert_eq!(
        records(
            &store
                .input_session_history(TERMINAL, None, None, 200, 103)
                .unwrap()
        ),
        vec![closed]
    );
}

#[test]
fn uncertainty_and_source_time_never_regress() {
    let store = Store::open_memory("amber").unwrap();
    let opening = opened("amber", 2, "person/avery", 100);
    store.append_input_session(&opening).unwrap();
    let mut checkpoint = progress(&opening, 2, 1);
    checkpoint.uncertain_handoff = true;
    store.append_input_session(&checkpoint).unwrap();
    let mut next = progress(&checkpoint, 3, 2);
    next.uncertain_handoff = false;
    assert_eq!(
        store.append_input_session(&next).unwrap_err().code,
        "input-session-regression"
    );
    next.uncertain_handoff = true;
    next.observed_at_unix_ms = opening.observed_at_unix_ms;
    assert_eq!(
        store.append_input_session(&next).unwrap_err().code,
        "input-session-regression"
    );
    next.observed_at_unix_ms = checkpoint.observed_at_unix_ms + 1;
    next.event = InputSessionEvent::Interrupted;
    next.reason = Some(InputSessionCloseReason::OwnerRestarted);
    assert_eq!(
        store.append_input_session(&next).unwrap_err().code,
        "input-session-interrupted-counts"
    );
}

#[test]
fn reopen_recovery_keeps_prior_epoch_and_last_durable_counts_once() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("audit.db");
    let opening = opened("amber", 3, "person/avery", 100);
    let checkpoint = progress(&opening, 21, 2);
    let mut current = opened("amber", 4, "person/avery", 102);
    current.owner_epoch = NEXT_EPOCH.into();
    {
        let store = Store::open(&path, "amber").unwrap();
        store.append_input_session(&opening).unwrap();
        store.append_input_session(&checkpoint).unwrap();
        store.append_input_session(&current).unwrap();
    }
    let store = Store::open(&path, "amber").unwrap();
    assert_eq!(store.recover_input_sessions(NEXT_EPOCH, 110).unwrap(), 1);
    assert_eq!(store.recover_input_sessions(NEXT_EPOCH, 111).unwrap(), 0);
    let page = store
        .input_session_history(TERMINAL, Some("person/avery"), None, 200, 111)
        .unwrap();
    let recovered = records(&page)
        .into_iter()
        .find(|record| record.session_id == opening.session_id)
        .unwrap();
    assert_eq!(recovered.event, InputSessionEvent::Interrupted);
    assert_eq!(
        recovered.reason,
        Some(InputSessionCloseReason::OwnerRestarted)
    );
    assert_eq!(recovered.owner_epoch, EPOCH);
    assert_eq!(
        (
            recovered.successful_send_bytes,
            recovered.successful_batches
        ),
        (21, 2)
    );
    assert_eq!(recovered.observed_at_unix_ms, 110);
    assert!(recovered.uncertain_handoff);
}

#[test]
fn history_filters_people_hides_expired_closed_sessions_and_keeps_live_ones() {
    let store = Store::open_memory("amber").unwrap();
    let now = input_sessions::RETENTION_MS + 200;
    let expired = opened("amber", 5, "person/avery", 100);
    let boundary = opened("amber", 6, "person/avery", 199);
    let live = opened("amber", 7, "person/avery", 1);
    let other = opened("amber", 8, "person/blair", 201);
    for record in [&expired, &boundary, &live, &other] {
        store.append_input_session(record).unwrap();
    }
    store.append_input_session(&close(&expired)).unwrap();
    let closed_boundary = close(&boundary);
    store.append_input_session(&closed_boundary).unwrap();
    let page = store
        .input_session_history(TERMINAL, Some("person/avery"), None, 200, now)
        .unwrap();
    let sessions = records(&page)
        .into_iter()
        .map(|record| record.session_id)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        sessions,
        BTreeSet::from([boundary.session_id, live.session_id])
    );
    assert_eq!(page["retained_from"], 200);
    assert_eq!(page["complete"], false);
    assert_eq!(page["owner_coverage"], json!(["amber"]));
    let empty = store
        .input_session_history("agent/absent", None, None, 200, now)
        .unwrap();
    assert_eq!(empty["items"], json!([]));
    assert_eq!(empty["complete"], false);
    assert_eq!(empty["owner_coverage"], json!([]));
}

fn sync(from: &Store, to: &Store, fleet: &str) {
    let exchange = from
        .export_replication_exchange_answering(
            fleet,
            &to.replication_inventory().unwrap(),
            &to.replication_signature_requests().unwrap(),
        )
        .unwrap();
    to.receive_replication_exchange(&from.origin, fleet, &exchange)
        .unwrap();
    to.validate_replication_backlog().unwrap();
    to.project_replication_backlog().unwrap();
}

#[test]
fn signed_replicas_converge_across_arrival_orders_and_cursors_ignore_progress() {
    use crate::fleet::MemberKey;
    let fleet = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
    let key = Arc::new(MemberKey::generate().unwrap().0);
    let source = Store::open_memory("amber").unwrap();
    source.bind_fleet(fleet).unwrap();
    source.pin_fleet_anchor(key.public()).unwrap();
    source.set_member_key(Some(key.clone())).unwrap();
    source.append_claim(&ClaimInput {
        subject: "host/amber".into(), kind: "fleet.member-admitted".into(), actor: None,
        fields: serde_json::from_value(json!({"fleet_id":fleet,"member_key":key.public(),"via":"anchor","mode":"listening"})).unwrap(),
        evidence: vec![], expected_subject: None, idempotency_key: None,
    }).unwrap();
    let first = opened("amber", 9, "person/avery", 100);
    let second = opened("amber", 10, "person/avery", 100);
    let left = Store::open_memory("left").unwrap();
    let right = Store::open_memory("right").unwrap();
    for replica in [&left, &right] {
        replica.bind_fleet(fleet).unwrap();
        replica.pin_fleet_anchor(key.public()).unwrap();
    }
    source.append_input_session(&first).unwrap();
    sync(&source, &left, fleet);
    source.append_input_session(&second).unwrap();
    let first_progress = progress(&first, 10, 1);
    source.append_input_session(&first_progress).unwrap();
    sync(&source, &right, fleet);
    sync(&source, &left, fleet);
    let expected = source
        .input_session_history(TERMINAL, Some("person/avery"), None, 200, 102)
        .unwrap();
    assert_eq!(
        left.input_session_history(TERMINAL, Some("person/avery"), None, 200, 102)
            .unwrap(),
        expected
    );
    assert_eq!(
        right
            .input_session_history(TERMINAL, Some("person/avery"), None, 200, 102)
            .unwrap(),
        expected
    );
    assert_eq!(
        records(&expected),
        vec![second.clone(), first_progress.clone()]
    );
    let page = source
        .input_session_history(TERMINAL, Some("person/avery"), None, 1, 102)
        .unwrap();
    let cursor = page["next_cursor"].as_str().unwrap();
    source
        .append_input_session(&progress(&second, 20, 2))
        .unwrap();
    let rest = source
        .input_session_history(TERMINAL, Some("person/avery"), Some(cursor), 1, 103)
        .unwrap();
    assert_eq!(records(&rest), vec![first_progress]);
    assert!(rest["next_cursor"].is_null());
    assert!(
        source
            .input_session_history(TERMINAL, Some("person/blair"), Some(cursor), 1, 103)
            .is_err()
    );
    assert_eq!(left.recover_input_sessions(NEXT_EPOCH, 110).unwrap(), 0);
}

#[test]
fn owner_origin_is_enforced_for_generic_writes_and_replica_classification() {
    let owner = Store::open_memory("amber").unwrap();
    let other = Store::open_memory("birch").unwrap();
    let record = opened("amber", 11, "person/avery", 100);
    assert!(other.append_input_session(&record).is_err());
    let claim = owner.append_input_session(&record).unwrap();
    assert!(matches!(
        classify_replicated_claim_with_registry(&claim, st3_schema::registry()).unwrap(),
        ReplicatedClaimAdmission::Valid
    ));
    let mut forged = claim.clone();
    forged.origin = "birch".into();
    assert!(classify_replicated_claim_with_registry(&forged, st3_schema::registry()).is_err());
    forged = claim;
    forged.actor = Some("person/avery".into());
    assert!(classify_replicated_claim_with_registry(&forged, st3_schema::registry()).is_err());
}

#[test]
fn checkpoint_proves_retained_history_and_retired_sessions_cannot_resurrect() {
    let store = Store::open_memory("amber").unwrap();
    let opening = opened("amber", 12, "person/avery", 100);
    let progress = progress(&opening, 10, 1);
    let closure = close(&progress);
    let expired_claims = [&opening, &progress, &closure]
        .map(|record| store.append_input_session(record).unwrap().id);
    let live = opened("amber", 13, "person/avery", 100);
    store.append_input_session(&live).unwrap();
    let now = u64::try_from(now_ms()).unwrap() + 1000;
    let before = store
        .input_session_history(TERMINAL, Some("person/avery"), None, 200, now)
        .unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (plan, proof) = store
        .plan_checkpoint(u128::from(now), scratch.path())
        .unwrap();
    assert!(proof.passed, "{proof:?}");
    for id in &expired_claims {
        assert!(
            plan.claims.iter().any(|claim| &claim.id == id),
            "expired audit must be trim eligible"
        );
    }
    {
        let mut connection = store.connection.write();
        let transaction = connection.transaction().unwrap();
        record_checkpoint_tombstones_tx(
            &transaction,
            "checkpoint/input-audit-test",
            &plan.envelopes,
            &plan.claims,
        )
        .unwrap();
        delete_dropped_rows_tx(&transaction, &plan.envelopes, &plan.claims).unwrap();
        transaction.commit().unwrap();
    }
    assert_eq!(
        store
            .input_session_history(TERMINAL, Some("person/avery"), None, 200, now)
            .unwrap(),
        before
    );
    assert_eq!(
        store.append_input_session(&opening).unwrap_err().code,
        "input-session-history-trimmed"
    );
}
