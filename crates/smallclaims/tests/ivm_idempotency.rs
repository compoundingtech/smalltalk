use serde_json::json;
use smallclaims::{
    ClaimInput, Store,
    ivm::{Views, runtime::ViewRuntime, source_cut},
    store::{claim_operation, current_index, operation_tx},
};
use std::sync::Arc;

fn runtime() -> Arc<ViewRuntime> {
    Arc::new(ViewRuntime::new(Views::new(vec![]).unwrap()).unwrap())
}

fn input() -> ClaimInput {
    ClaimInput {
        subject: "account/alder".into(),
        kind: "account.limits".into(),
        actor: None,
        fields: serde_json::from_value(json!({"remaining": 3})).unwrap(),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: Some("limits/alder/1".into()),
    }
}

#[test]
fn identical_keyed_input_returns_original_claim_without_advancing_the_cut() {
    let store = Store::open_memory("alder", runtime()).unwrap();
    let input = input();
    let (first, new) = store.append_claim_outcome(&input).unwrap();
    assert!(new);
    let before = source_cut(&store.readers.get()).unwrap();
    let (repeated, new) = store.append_claim_outcome(&input).unwrap();
    assert_eq!(repeated.id, first.id);
    assert!(!new);
    assert_eq!(store.claims_for(&input.subject, None).unwrap().len(), 1);
    assert_eq!(
        current_index(&store.readers.get()).unwrap(),
        first.store_index
    );
    assert_eq!(source_cut(&store.readers.get()).unwrap(), before);
    let (operation, digest) = claim_operation(&input).unwrap().unwrap();
    assert_eq!(first.operation_id.as_deref(), Some(operation.as_str()));
    assert_eq!(first.request_digest.as_deref(), Some(digest.as_str()));
    assert_eq!(
        operation_tx(&store.readers.get(), &operation).unwrap(),
        Some((digest, first.id, "active".into()))
    );
}

#[test]
fn changed_request_with_same_key_is_refused_without_appending() {
    let store = Store::open_memory("alder", runtime()).unwrap();
    let input = input();
    let first = store.append_claim(&input).unwrap();
    let mut changed = input.clone();
    changed.fields.insert("remaining".into(), json!(8));
    let error = store.append_claim(&changed).unwrap_err();
    assert_eq!(error.code, "idempotency-mismatch");
    assert_ne!(
        error.details["stored_digest"],
        error.details["request_digest"]
    );
    assert_eq!(store.claims_for(&input.subject, None).unwrap().len(), 1);
    assert_eq!(store.append_claim(&input).unwrap().id, first.id);
}

#[test]
fn keyless_inputs_still_append_and_invalid_keys_are_refused() {
    let store = Store::open_memory("alder", runtime()).unwrap();
    let mut input = input();
    for key in ["".to_owned(), "bad\nkey".to_owned(), "x".repeat(513)] {
        input.idempotency_key = Some(key);
        assert_eq!(
            store.append_claim(&input).unwrap_err().code,
            "invalid-idempotency-key"
        );
    }
    assert!(store.claims_for(&input.subject, None).unwrap().is_empty());
    input.idempotency_key = None;
    let (first, new) = store.append_claim_outcome(&input).unwrap();
    assert!(new);
    let (second, new) = store.append_claim_outcome(&input).unwrap();
    assert!(new);
    assert_ne!(first.id, second.id);
    assert!(first.operation_id.is_none());
}

#[test]
fn keyed_retry_survives_reopening_the_store() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.db");
    let input = input();
    let first = {
        let store = Store::open(&path, "alder", runtime()).unwrap();
        store.append_claim(&input).unwrap()
    };
    let store = Store::open(&path, "alder", runtime()).unwrap();
    let (repeated, new) = store.append_claim_outcome(&input).unwrap();
    assert_eq!(repeated.id, first.id);
    assert!(!new);
    assert_eq!(store.claims_for(&input.subject, None).unwrap().len(), 1);
}

#[test]
fn simultaneous_keyed_requests_append_once() {
    let store = Store::open_memory("alder", runtime()).unwrap();
    let input = input();
    let barrier = std::sync::Barrier::new(2);
    let outcomes = std::thread::scope(|scope| {
        let append = || {
            barrier.wait();
            store.append_claim_outcome(&input).unwrap()
        };
        let first = scope.spawn(append);
        let second = scope.spawn(append);
        [first.join().unwrap(), second.join().unwrap()]
    });
    assert_eq!(outcomes[0].0.id, outcomes[1].0.id);
    assert_ne!(outcomes[0].1, outcomes[1].1);
    assert_eq!(store.claims_for(&input.subject, None).unwrap().len(), 1);
}

#[test]
fn conflicting_operation_is_refused_without_appending() {
    let store = Store::open_memory("alder", runtime()).unwrap();
    let input = input();
    store.append_claim(&input).unwrap();
    let (operation, _) = claim_operation(&input).unwrap().unwrap();
    store
        .connection
        .batched(|tx| {
            tx.execute(
                "UPDATE operations SET state='conflict' WHERE id=?1",
                [&operation],
            )
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        store.append_claim(&input).unwrap_err().code,
        "idempotency-conflict"
    );
    assert_eq!(store.claims_for(&input.subject, None).unwrap().len(), 1);
}
