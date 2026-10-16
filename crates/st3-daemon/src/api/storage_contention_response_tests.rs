#[test]
fn admitted_contention_is_pending_not_a_peer_error_and_other_faults_stay_errors() {
    for code in ["database-busy", "database-locked"] {
        assert_eq!(super::admitted_projection_result(Err(anyhow::Error::new(
            super::St3Error::new(code, "fixture contention")))).unwrap(), None);
        assert_eq!(super::client_error_code(Some(code)), code);
    }
    assert!(super::admitted_projection_result(Err(anyhow::anyhow!("other fault"))).is_err());
    assert_eq!(super::admitted_projection_result(Ok(Some(true))).unwrap(), Some(true));
}
#[test]
fn accepted_replication_survives_a_contended_begin_and_projects_after_release() {
    use super::*;
    use std::collections::BTreeMap;
    const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    let source = Store::open_memory("source").unwrap();
    source.append_client_claim(&smallclaims::ClaimInput {
        subject: "resource/receive-busy".into(), kind: "resource.observed".into(), actor: None,
        fields: BTreeMap::from([("kind".into(), json!("custom.test.replication"))]),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
    source.bind_fleet(FLEET).unwrap();
    let store = Store::open(&path, "target").unwrap();
    store.bind_fleet(FLEET).unwrap();
    store.project_replication_backlog().unwrap();
    let frontier = || store.readers.get().query_row(
        "SELECT last_good_store_index FROM projection_health WHERE aggregate='graph'",
        [], |row| row.get::<_, u64>(0)).unwrap();
    let before = frontier();
    let inventory = store.export_replication_summary(FLEET).unwrap().inventory;
    let exchange = source.export_replication_exchange(FLEET, &inventory).unwrap();
    store.receive_replication_exchange("source", FLEET, &exchange).unwrap();
    assert!(store.validate_replication_backlog().unwrap().changed);
    let target = store.index().unwrap();
    assert!(target > before);
    store.connection.write().busy_timeout(std::time::Duration::ZERO).unwrap();
    let holder = rusqlite::Connection::open(&path).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let pending = admitted_projection_result(store.project_replication_backlog().map(Some)).unwrap();
    assert_eq!(pending, None);
    assert!(store.replication_projection_deferred());
    assert_eq!(frontier(), before);
    assert_eq!(store.index().unwrap(), target);
    assert!(!store.claims_for("resource/receive-busy", Some("resource.observed")).unwrap().is_empty());
    holder.execute_batch("ROLLBACK").unwrap();
    assert!(store.project_replication_backlog().unwrap());
    assert_eq!(frontier(), target);
    assert!(!store.replication_projection_deferred());
    let visible: i64 = store.readers.get().query_row(
        "SELECT COUNT(*) FROM events WHERE subject='resource/receive-busy' AND kind='resource.observed'",
        [], |row| row.get(0)).unwrap();
    assert_eq!(visible, 1);
}
#[test]
fn private_projection_context_stays_out_of_client_details() {
    let error = smallclaims::error::internal(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY), None))
        .with_detail("projection_stage", "begin-immediate")
        .with_detail("projection_frontier_unknown", true);
    let response = super::ApiError::bad(error);
    assert_eq!(response.status, super::StatusCode::SERVICE_UNAVAILABLE);
    assert!(!response.details.contains_key("projection_stage"));
    assert!(!response.details.contains_key("projection_frontier_unknown"));
    assert_eq!(
        response.details["sqlite_extended_code"],
        rusqlite::ffi::SQLITE_BUSY
    );
}
#[test]
fn expired_idempotency_response_is_a_terminal_client_error() {
    let error = super::ApiError::bad(super::St3Error::new(
        "idempotency-key-expired",
        "already committed",
    ));
    assert_eq!(error.status, super::StatusCode::CONFLICT);
    assert_eq!(
        super::client_error_code(Some(&error.code)),
        "idempotency-key-expired"
    );
    assert!(!super::client_error_retryable(
        error.status,
        Some(&error.code)
    ));
}

#[test]
fn typed_sqlite_contention_is_a_service_failure_not_input_validation() {
    for code in ["database-busy", "database-locked"] {
        let response = super::ApiError::bad(super::St3Error::new(code, "storage contention"));
        assert_eq!(response.status, super::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.code, code);
    }
    assert_eq!(super::ApiError::bad(super::St3Error::new("internal", "other fault")).status, super::StatusCode::INTERNAL_SERVER_ERROR);
}
