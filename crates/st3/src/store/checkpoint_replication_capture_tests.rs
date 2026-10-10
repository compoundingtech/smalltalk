//! Ordinary replicated admission and projection must not invalidate an older capture.
use super::*;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};
use std::time::Duration;

const FLEET: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
const CUT: u128 = 150;
const ROUNDS: usize = 4;

fn epoch(store: &Store) -> i64 {
    smallclaims::store::checkpoint_capture_epoch(&store.readers.get()).unwrap()
}

fn publish_desired(store: &Store, name: &str, version: usize) -> String {
    let source = format!(
        "version 2\nagent \"capture/{name}\" {{ name \"Capture version {version}\"; workspace \"/tmp\"; harness \"claude\" {{ }} }}\n"
    );
    let intent = crate::parse_intent(&source, store.origin()).unwrap();
    assert_eq!(intent.subjects.len(), 1);
    let subject = intent.subjects.keys().next().unwrap().clone();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(
            &intent,
            &preview.subject_tokens,
            &format!("capture-{name}-desired-{version}"),
        )
        .unwrap();
    subject
}

fn publish_document(store: &Store, name: &str, version: usize) -> String {
    let name = format!("doc/capture/{name}");
    store
        .put_document(
            &name,
            format!("Invented capture document version {version}.").as_bytes(),
            &store.latest_document_token(&name).unwrap(),
            &format!("capture-{name}-document-{version}"),
        )
        .unwrap()
        .hash
}

fn exchange(source: &Store, target: &Store) -> ReplicationExchange {
    source
        .export_replication_exchange_answering(
            FLEET,
            &target.replication_inventory().unwrap(),
            &target.replication_signature_requests().unwrap(),
        )
        .unwrap()
}

fn admit_and_project(source: &Store, target: &Store, exchange: &ReplicationExchange) {
    let receipt = target
        .receive_replication_exchange(source.origin(), FLEET, exchange)
        .unwrap();
    assert_eq!(receipt.received, exchange.envelopes.len());
    assert_eq!(receipt.duplicate, 0);
    let admitted = target.validate_replication_backlog().unwrap();
    assert!(admitted.changed);
    assert!(admitted.valid > 0);
    assert_eq!((admitted.invalid, admitted.unknown, admitted.held), (0, 0, 0));
    assert!(target.project_replication_backlog().unwrap());
}

struct PagePause {
    envelope_page: AtomicBool,
    pauses: AtomicUsize,
    reached: mpsc::SyncSender<usize>,
    resume: Mutex<mpsc::Receiver<()>>,
}

unsafe extern "C" fn pause_after_envelope_page(
    event: std::ffi::c_uint,
    context: *mut std::ffi::c_void,
    statement: *mut std::ffi::c_void,
    _: *mut std::ffi::c_void,
) -> std::ffi::c_int {
    let pause = unsafe { &*context.cast::<PagePause>() };
    let sql = unsafe { rusqlite::ffi::sqlite3_sql(statement.cast()) };
    if sql.is_null() {
        return 0;
    }
    let sql = unsafe { std::ffi::CStr::from_ptr(sql) }.to_bytes();
    const ENVELOPE_PAGE: &[u8] = b"FROM replica_envelopes envelopes";
    if event == rusqlite::ffi::SQLITE_TRACE_STMT as u32
        && sql.windows(ENVELOPE_PAGE.len()).any(|part| part == ENVELOPE_PAGE)
    {
        pause.envelope_page.store(true, Ordering::Release);
    }
    if event == rusqlite::ffi::SQLITE_TRACE_PROFILE as u32
        && sql == b"COMMIT"
        && pause.envelope_page.swap(false, Ordering::AcqRel)
        && let Ok(round) = pause.pauses.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |count| (count < ROUNDS).then_some(count + 1),
        )
        && pause.reached.send(round).is_ok()
        && let Ok(resume) = pause.resume.lock()
    {
        let _ = resume.recv_timeout(Duration::from_secs(30));
    }
    0
}

/// Uninstall even when an assertion unwinds; the callback context outlives this guard.
struct TraceGuard<'a>(&'a Connection);

impl Drop for TraceGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            rusqlite::ffi::sqlite3_trace_v2(self.0.handle(), 0, None, std::ptr::null_mut());
        }
    }
}

#[test]
fn replicated_admission_duplicate_reoffers_and_projection_leave_older_capture_unchanged() {
    let scratch = tempfile::tempdir().unwrap();
    let source = Store::open_memory("sender").unwrap();
    source.bind_fleet(FLEET).unwrap();
    let target = Arc::new(Store::open(&scratch.path().join("claims.sqlite3"), "receiver").unwrap());
    target.bind_fleet(FLEET).unwrap();
    source.set_write_clock_at(100).unwrap();
    for index in 0..ROUNDS {
        let claim = source
            .append_claim(&ClaimInput {
                subject: "daemon/sender".into(),
                kind: "daemon.diagnostic".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("severity".into(), json!("warning")),
                    ("code".into(), json!("capture-replication")),
                    ("reason".into(), json!(format!("Invented old diagnostic {index}."))),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some(format!("capture-old-{index}")),
            })
            .unwrap();
        assert_eq!(claim.accepted_at_unix_ms, 100);
        source.seal_local_batches().unwrap();
    }
    let old_exchange = exchange(&source, &target);
    assert_eq!(old_exchange.envelopes.len(), ROUNDS);
    admit_and_project(&source, &target, &old_exchange);

    // These projection identities are already inside the frontier but above the cut.
    // Updating them tests OLD as well as NEW references without touching an old claim.
    source.set_write_clock_at(200).unwrap();
    let remote_subject = publish_desired(&source, "remote", 0);
    publish_document(&source, "remote", 0);
    admit_and_project(&source, &target, &exchange(&source, &target));
    target.set_write_clock_at(200).unwrap();
    let local_subject = publish_desired(&target, "local", 0);
    publish_document(&target, "local", 0);
    let expected = target.checkpoint_sealed_set_paged(CUT, None, 1, 1).unwrap();
    assert_eq!(expected.claims.len(), ROUNDS);
    assert_eq!(expected.envelopes.len(), ROUNDS);
    assert!(expected.claims.iter().all(|claim| claim.claim.accepted_at_unix_ms == 100));
    let identities = checkpoint::SealedIdentities::of(&expected);
    let expected_epoch = epoch(&target);

    let (reached, at_page_boundary) = mpsc::sync_channel(1);
    let (resume, continue_capture) = mpsc::sync_channel(1);
    let pause = Arc::new(PagePause {
        envelope_page: AtomicBool::new(false),
        pauses: AtomicUsize::new(0),
        reached,
        resume: Mutex::new(continue_capture),
    });
    let reader = target.clone();
    let reader_pause = pause.clone();
    let capture = std::thread::spawn(move || {
        // Only this worker's request reader has a callback. Other replication reads cannot
        // accidentally pause, and each capture page still starts and ends its own snapshot.
        reader.readers.request_read(|| {
            let connection = reader.readers.get();
            unsafe {
                rusqlite::ffi::sqlite3_trace_v2(
                    connection.handle(),
                    (rusqlite::ffi::SQLITE_TRACE_STMT | rusqlite::ffi::SQLITE_TRACE_PROFILE) as u32,
                    Some(pause_after_envelope_page),
                    Arc::as_ptr(&reader_pause).cast_mut().cast(),
                );
            }
            let _trace = TraceGuard(&connection);
            reader.checkpoint_sealed_set_paged(CUT, None, 1, 1)
        })
    });

    for round in 0..ROUNDS {
        assert_eq!(at_page_boundary.recv_timeout(Duration::from_secs(30)).unwrap(), round);
        // PROFILE fires after COMMIT finishes: no capture snapshot remains while these real
        // receive/admission/projection transactions commit, before the next page begins.
        source.set_write_clock_at(201 + round as u128).unwrap();
        publish_desired(&source, "remote", round + 1);
        let remote_hash = publish_document(&source, "remote", round + 1);
        let incoming = exchange(&source, &target);
        assert!(!incoming.envelopes.is_empty());
        assert!(incoming.envelopes.iter().all(|envelope| envelope.accepted_at_unix_ms >= 200));
        let receipt = target.receive_replication_exchange(source.origin(), FLEET, &incoming).unwrap();
        assert_eq!(receipt.received, incoming.envelopes.len());
        assert_eq!(epoch(&target), expected_epoch, "ordinary receive in round {round}");
        let admitted = target.validate_replication_backlog().unwrap();
        assert!(admitted.changed && admitted.valid > 0);
        assert_eq!((admitted.invalid, admitted.unknown, admitted.held), (0, 0, 0));
        assert_eq!(epoch(&target), expected_epoch, "ordinary admission in round {round}");
        assert!(target.project_replication_backlog().unwrap());
        assert_eq!(epoch(&target), expected_epoch, "ordinary projection in round {round}");
        assert_eq!(target.selected_desired_token(&remote_subject).unwrap(), source.selected_desired_token(&remote_subject).unwrap());
        assert_eq!(target.latest_document_hash("doc/capture/remote").unwrap(), Some(remote_hash));

        // A below-cut envelope is re-offered exactly as a real peer does it. OR IGNORE must
        // cause neither an AFTER event nor a content-difference BEFORE invalidation.
        for _ in 0..3 {
            for duplicate in [&old_exchange, &incoming] {
                let receipt = target.receive_replication_exchange(source.origin(), FLEET, duplicate).unwrap();
                assert_eq!(receipt.received, 0);
                assert_eq!(receipt.duplicate, duplicate.envelopes.len());
                let admitted = target.validate_replication_backlog().unwrap();
                assert!(!admitted.changed);
                assert_eq!((admitted.valid, admitted.invalid, admitted.unknown, admitted.held), (0, 0, 0, 0));
                target.project_replication_backlog().unwrap();
                assert_eq!(epoch(&target), expected_epoch, "duplicate re-offer in round {round}");
            }
        }
        target.set_write_clock_at(300 + round as u128).unwrap();
        publish_desired(&target, "local", round + 1);
        let local_hash = publish_document(&target, "local", round + 1);
        target.seal_local_batches().unwrap();
        assert_eq!(epoch(&target), expected_epoch, "local desired/document writes in round {round}");
        assert_eq!(target.latest_document_hash("doc/capture/local").unwrap(), Some(local_hash));
        let token = target.selected_desired_token(&local_subject).unwrap().unwrap();
        assert!(target.claim_by_id(&token).unwrap().unwrap().accepted_at_unix_ms >= 200);
        resume.send(()).unwrap();
    }
    let captured = capture.join().unwrap().unwrap().expect("ordinary replication must not exhaust capture retries");
    assert_eq!(pause.pauses.load(Ordering::Acquire), ROUNDS);
    assert_eq!(epoch(&target), expected_epoch);
    assert_eq!(checkpoint::SealedIdentities::of(&captured), identities);
    assert_eq!(format!("{:?}", captured.claims), format!("{:?}", expected.claims));
    assert!(captured.envelopes.iter().all(|envelope| envelope.accepted_at_unix_ms == 100));
}
