//! Restricted application-shaped compatibility fixtures, not production numeric/authority ports.
#[allow(dead_code)]
#[path = "support/history.rs"]
mod history;
#[allow(dead_code)]
#[path = "support/real_views.rs"]
mod real_views;
use anyhow::Result;
use serde_json::{Value, json};
use smallclaims::{
    ClaimInput, ClaimRecord, Runtime, Store,
    ivm::{
        Contribution, Definition, Readiness, View, Views,
        after_write::{self, Target, WaitOptions, WaitOutcome, WriteOutcome},
        asynchronous::{self as async_views, Limits, PageStatus, Worker},
        events::{self, Publisher},
        runtime::ViewRuntime,
        source_cut,
    },
    replication::ReplicationInventory,
    store::{canonical, runtime::Plain},
};
use std::{collections::BTreeMap, sync::Arc};
use tokio::{
    sync::watch,
    time::{Duration, Instant},
};

fn definitions() -> Views {
    Views::new(vec![
        Box::new(real_views::Family::Card),
        Box::new(real_views::Family::Mailbox),
        Box::new(real_views::Family::Desired),
    ])
    .unwrap()
}
fn runtime(limits: Limits) -> Arc<ViewRuntime> {
    Arc::new(ViewRuntime::asynchronous(definitions(), limits).unwrap())
}
fn fixture(limits: Limits) -> (Arc<Store>, Arc<ViewRuntime>) {
    let runtime = runtime(limits);
    let store = Arc::new(Store::open_memory("alder", runtime.clone()).unwrap());
    store
        .connection
        .batched(|tx| events::install(tx, 128))
        .unwrap()
        .unwrap();
    (store, runtime)
}
fn input(subject: &str, kind: &str, actor: Option<&str>, fields: Value) -> ClaimInput {
    ClaimInput {
        subject: subject.into(),
        kind: kind.into(),
        actor: actor.map(str::to_owned),
        fields: serde_json::from_value(fields).unwrap(),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    }
}
fn append(
    store: &Store,
    subject: &str,
    kind: &str,
    actor: Option<&str>,
    fields: Value,
) -> ClaimRecord {
    store
        .append_claim(&input(subject, kind, actor, fields))
        .unwrap()
}
fn drain(store: &Store, rt: &ViewRuntime) -> Vec<async_views::PageReport> {
    let mut reports = vec![];
    loop {
        let report = async_views::step(store, rt).unwrap();
        let more = report.status == PageStatus::More;
        assert!(!matches!(report.status, PageStatus::Stopped(_)));
        reports.push(report);
        if !more {
            return reports;
        }
    }
}
fn rows(c: &rusqlite::Connection, view: &str) -> BTreeMap<String, Value> {
    c.prepare("SELECT id,payload FROM app_rows WHERE view=?1 ORDER BY id")
        .unwrap()
        .query_map([view], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .unwrap()
        .map(|r| {
            let (id, value) = r.unwrap();
            (id, serde_json::from_str(&value).unwrap())
        })
        .collect()
}
fn oracle(store: &Store) {
    let c = store.readers.get();
    let claims = history::raw(&c).unwrap();
    assert_eq!(rows(&c, "agent-card"), history::card(&claims));
    assert_eq!(rows(&c, "mailbox"), history::mailbox(&claims).0);
    assert_eq!(rows(&c, "desired-by-host"), history::desired(&claims).0);
}

#[test]
fn raw_admission_and_delta_are_atomic_and_views_wait_for_explicit_pages() {
    let (store, rt) = fixture(Limits {
        page_rows: 1,
        ..Limits::default()
    });
    let first = append(
        &store,
        "message/a",
        "message.sent",
        None,
        json!({"to":"person/ada","from":"person/sender","body":"hello"}),
    );
    append(
        &store,
        "message/a",
        "message.read",
        None,
        json!({"reader":"person/ada"}),
    );
    assert_eq!(
        async_views::state(&store.readers.get())
            .unwrap()
            .retained_rows,
        2
    );
    assert_eq!(
        source_cut(&store.readers.get()).unwrap().unwrap().projected,
        0
    );
    assert_eq!(
        rt.views
            .readiness(&store.readers.get(), "mailbox", 1)
            .unwrap(),
        Readiness::SourcePending
    );
    assert!(rows(&store.readers.get(), "mailbox").is_empty());
    let page = async_views::step(&store, &rt).unwrap();
    assert_eq!(page.rows, 1);
    assert_eq!(page.source_cut.projected, first.store_index);
    assert_eq!(page.status, PageStatus::More);
    assert_eq!(
        rt.views
            .readiness(&store.readers.get(), "mailbox", 1)
            .unwrap(),
        Readiness::SourcePending
    );
    let page = async_views::step(&store, &rt).unwrap();
    assert_eq!(page.status, PageStatus::CaughtUp);
    assert_eq!(page.retained_rows, 0);
    assert_eq!(page.retained_bytes, 0);
    assert!(matches!(
        rt.views
            .readiness(&store.readers.get(), "mailbox", 1)
            .unwrap(),
        Readiness::Ready(_)
    ));
    oracle(&store);
}

#[test]
fn failed_outer_commit_and_rollback_leave_no_captured_delta() {
    let (store, rt) = fixture(Limits::default());
    let publisher = Publisher::attach(&store, 8).unwrap();
    let mut notices = publisher.subscribe();
    let mut writer = store.connection.write();
    writer.execute_batch("CREATE TABLE parent(id INTEGER PRIMARY KEY);CREATE TABLE child(id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED)").unwrap();
    let tx = writer.transaction().unwrap();
    rt.append_claim_tx(
        &tx,
        &store.origin,
        "message/a",
        "message.sent",
        None,
        &json!({"fields":{"to":"person/ada","from":"person/sender","body":"hi"}}),
        &[],
        None,
    )
    .unwrap();
    tx.execute("INSERT INTO child VALUES(1)", []).unwrap();
    assert!(tx.commit().is_err());
    drop(writer);
    assert_eq!(
        async_views::state(&store.readers.get())
            .unwrap()
            .retained_rows,
        0
    );
    assert_eq!(
        source_cut(&store.readers.get()).unwrap().unwrap().admitted,
        0
    );
    assert!(notices.try_recv().is_err());
}

#[test]
fn quota_overflow_preserves_admission_and_stops_without_false_readiness() {
    let (store, rt) = fixture(Limits {
        queue_rows: 1,
        page_rows: 1,
        ..Limits::default()
    });
    append(
        &store,
        "message/a",
        "message.sent",
        None,
        json!({"to":"person/ada","from":"person/sender","body":"hello"}),
    );
    let second = append(
        &store,
        "message/b",
        "message.sent",
        None,
        json!({"to":"person/ada","from":"person/sender","body":"second"}),
    );
    let state = async_views::state(&store.readers.get()).unwrap();
    assert!(!state.available);
    assert_eq!(state.retained_rows, 1);
    assert_eq!(
        store
            .readers
            .get()
            .query_row(
                "SELECT COUNT(*) FROM claims WHERE id=?1",
                [&second.id],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
        1
    );
    assert!(matches!(
        async_views::step(&store, &rt).unwrap().status,
        PageStatus::Stopped(_)
    ));
    assert_eq!(
        rt.views
            .readiness(&store.readers.get(), "mailbox", 1)
            .unwrap(),
        Readiness::SourcePending
    );
    assert!(
        rt.views
            .availability(&store.readers.get(), "mailbox", 1)
            .unwrap()
            .error
            .unwrap()
            .contains("quota")
    );
}

#[test]
fn oversized_page_input_and_queue_corruption_are_explicit_unavailable() {
    let (store, rt) = fixture(Limits {
        page_bytes: 128,
        ..Limits::default()
    });
    append(
        &store,
        "message/a",
        "message.sent",
        None,
        json!({"to":"person/ada","from":"person/sender","body":"hello"}),
    );
    assert!(matches!(
        async_views::step(&store, &rt).unwrap().status,
        PageStatus::Stopped(_)
    ));
    let (store, rt) = fixture(Limits::default());
    append(&store, "noise", "future.kind", None, json!({"x":1}));
    store
        .connection
        .batched(|tx| tx.execute("DELETE FROM ivm_async_queue", []))
        .unwrap()
        .unwrap();
    assert!(!async_views::state(&store.readers.get()).unwrap().available);
    assert!(matches!(
        async_views::step(&store, &rt).unwrap().status,
        PageStatus::Stopped(_)
    ));
    assert_eq!(
        source_cut(&store.readers.get()).unwrap().unwrap().projected,
        0
    );
}

#[test]
fn crash_reopen_resumes_the_same_queue_without_open_time_fold() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claims.db");
    let rt = runtime(Limits {
        page_rows: 1,
        ..Limits::default()
    });
    let store = Store::open(&path, "alder", rt.clone()).unwrap();
    store
        .connection
        .batched(|tx| events::install(tx, 128))
        .unwrap()
        .unwrap();
    append(
        &store,
        "message/a",
        "message.sent",
        None,
        json!({"to":"person/ada","from":"person/sender","body":"hello"}),
    );
    append(
        &store,
        "message/b",
        "message.sent",
        None,
        json!({"to":"person/ada","from":"person/sender","body":"world"}),
    );
    async_views::step(&store, &rt).unwrap();
    let before = source_cut(&store.readers.get()).unwrap();
    drop(store);
    let store = Store::open(&path, "alder", rt.clone()).unwrap();
    assert_eq!(source_cut(&store.readers.get()).unwrap(), before);
    assert_eq!(
        async_views::state(&store.readers.get())
            .unwrap()
            .retained_rows,
        1
    );
    drain(&store, &rt);
    oracle(&store);
}

#[test]
fn unsupported_existing_store_and_synchronous_owner_are_refused_before_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claims.db");
    let plain = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    append(&plain, "noise", "future.kind", None, json!({"x":1}));
    drop(plain);
    assert!(Store::open(&path, "alder", runtime(Limits::default())).is_err());
    let async_path = dir.path().join("async.db");
    let rt = runtime(Limits::default());
    let store = Store::open(&async_path, "alder", rt).unwrap();
    drop(store);
    assert!(
        Store::open(
            &async_path,
            "alder",
            Arc::new(ViewRuntime::new(definitions()).unwrap())
        )
        .is_err()
    );
}

#[test]
fn unrelated_inputs_advance_the_complete_prefix_without_view_writes_or_generations() {
    let (store, rt) = fixture(Limits::default());
    let before = rt.views.token(&store.readers.get(), "mailbox", 1).unwrap();
    store.connection.write().execute_batch("CREATE TABLE touches(n INTEGER);INSERT INTO touches VALUES(0);CREATE TRIGGER watch_async_prefix AFTER UPDATE ON ivm_async_views BEGIN UPDATE touches SET n=n+1; END;CREATE TRIGGER watch_view AFTER UPDATE ON ivm_views BEGIN UPDATE touches SET n=n+1; END;").unwrap();
    for n in 0..10 {
        append(
            &store,
            &format!("noise/{n}"),
            "future.kind",
            None,
            json!({"x":n}),
        );
    }
    drain(&store, &rt);
    assert_eq!(
        rt.views.token(&store.readers.get(), "mailbox", 1).unwrap(),
        before
    );
    assert_eq!(
        store
            .readers
            .get()
            .query_row("SELECT n FROM touches", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        async_views::applied_prefix(&store.readers.get(), &rt.views, "mailbox", 1).unwrap(),
        Some(10)
    );
}

#[test]
fn raw_source_remap_and_deletion_never_relabel_a_ready_prefix() {
    for change in [
        "UPDATE claims SET store_index=store_index+100",
        "DELETE FROM claims",
    ] {
        let (store, rt) = fixture(Limits::default());
        append(&store, "noise", "future.kind", None, json!({"x":1}));
        drain(&store, &rt);
        store.connection.write().execute_batch(change).unwrap();
        assert!(!async_views::state(&store.readers.get()).unwrap().available);
        assert!(matches!(
            rt.views
                .readiness(&store.readers.get(), "mailbox", 1)
                .unwrap(),
            Readiness::SourcePending | Readiness::Fenced
        ));
    }
}

#[test]
fn card_mailbox_and_desired_views_match_independent_history_after_shuffled_replication() {
    let limits = Limits {
        page_rows: 2,
        ..Limits::default()
    };
    let (source, rt) = fixture(limits);
    for (subject, kind, actor, fields) in [
        (
            "agent/ada",
            "runtime.observed",
            None,
            json!({"incarnation_id":"i1","status":"running"}),
        ),
        (
            "agent/ada",
            "harness.observed",
            None,
            json!({"incarnation_id":"i1","provider_auth":true,"state":"ready"}),
        ),
        // Legacy historical compatibility slice only, not numeric latest-source migration.
        (
            "agent/ada",
            "harness.usage",
            None,
            json!({"incarnation_id":"i1","semantics":"session_cumulative","total_tokens":12}),
        ),
        (
            "step/build",
            "work.claimed",
            Some("agent/ada"),
            json!({"claim_incarnation":"i1","status":"working"}),
        ),
        (
            "step/build",
            "work.progress",
            Some("agent/ada"),
            json!({"claim_incarnation":"i1","status":"verifying"}),
        ),
        (
            "message/a",
            "message.read",
            None,
            json!({"reader":"person/ada"}),
        ),
        (
            "message/a",
            "message.sent",
            None,
            json!({"to":"person/ada","from":"person/sender","body":"hello"}),
        ),
        ("agent/ada", "future.kind", None, json!({"bad":"ignored"})),
    ] {
        append(&source, subject, kind, actor, fields);
    }
    for host in ["amber", "cobalt"] {
        let body = desired_body("agent/fixture/desired", host);
        source
            .connection
            .batched(|tx| {
                rt.append_claim_tx(
                    tx,
                    &source.origin,
                    "agent/fixture/desired",
                    "intent.desired",
                    Some("person/fixture"),
                    &body,
                    &[],
                    None,
                )
            })
            .unwrap()
            .unwrap();
    }
    drain(&source, &rt);
    oracle(&source);
    let exchange = source
        .export_replication_exchange("sample-fleet", &ReplicationInventory::default())
        .unwrap();
    for reverse in [false, true] {
        let target_rt = runtime(limits);
        let target =
            Store::open_memory(if reverse { "cedar" } else { "birch" }, target_rt.clone()).unwrap();
        target
            .connection
            .batched(|tx| events::install(tx, 128))
            .unwrap()
            .unwrap();
        let mut envelopes = exchange.envelopes.clone();
        if reverse {
            envelopes.reverse();
        }
        for envelope in envelopes {
            let mut delivery = exchange.clone();
            delivery.envelopes = vec![envelope];
            target
                .receive_replication_exchange("alder", "sample-fleet", &delivery)
                .unwrap();
            target.validate_replication_backlog().unwrap();
            target.project_replication_backlog().unwrap();
            drain(&target, &target_rt);
            oracle(&target);
            target
                .receive_replication_exchange("alder", "sample-fleet", &delivery)
                .unwrap();
            target.validate_replication_backlog().unwrap();
            target.project_replication_backlog().unwrap();
            drain(&target, &target_rt);
            oracle(&target);
        }
        assert_eq!(
            rows(&source.readers.get(), "agent-card"),
            rows(&target.readers.get(), "agent-card")
        );
    }
}

#[tokio::test]
async fn worker_catches_up_read_after_write_and_stop_retains_resumable_state() {
    let (store, rt) = fixture(Limits {
        page_rows: 1,
        ..Limits::default()
    });
    let publisher = Publisher::attach(&store, 8).unwrap();
    let receipt = match after_write::write(
        &store,
        &input(
            "message/a",
            "message.sent",
            None,
            json!({"to":"person/ada","from":"person/sender","body":"hello"}),
        ),
    )
    .unwrap()
    {
        WriteOutcome::Committed { receipt, .. } => receipt,
        other => panic!("{other:?}"),
    };
    let worker = Worker::start(store.clone(), rt.clone(), publisher.subscribe()).unwrap();
    let (_cancel, cancellation) = watch::channel(false);
    let result = after_write::wait(
        &store,
        &rt.views,
        "mailbox",
        Target::Local(&receipt),
        &publisher,
        WaitOptions {
            deadline: Instant::now() + Duration::from_secs(5),
            cancellation,
        },
        |c, _| real_views::row(c, "mailbox", "message/a"),
    )
    .await
    .unwrap();
    assert!(matches!(result, WaitOutcome::Ready { value: Some(_), .. }));
    worker.stop().await.unwrap();
    append(
        &store,
        "message/b",
        "message.sent",
        None,
        json!({"to":"person/ada","from":"person/sender","body":"later"}),
    );
    assert_eq!(
        async_views::state(&store.readers.get())
            .unwrap()
            .retained_rows,
        1
    );
    drain(&store, &rt);
    oracle(&store);
}

#[test]
fn unsupported_local_deadline_and_ordered_tree_adapter_never_claims_ready() {
    assert!(
        ViewRuntime::asynchronous(
            Views::new(vec![Box::new(real_views::Family::Tree)]).unwrap(),
            Limits::default()
        )
        .is_err()
    );
}

fn desired_body(subject: &str, host: &str) -> Value {
    json!({"subject":subject,"kind":"agent",
        "desired":{"name":"agent","arguments":["fixture/desired"],"children":[
            {"name":"host","arguments":[host]}, {"name":"workspace","arguments":["/unopened-fixture/desired"]},
            {"name":"command","arguments":["true"]}]},
        "member":{"kind":"agent","host":host,"runtime_id":"fixture.desired",
            "workspace":"/unopened-fixture/desired","workspace_create":false,"cwd":"/unopened-fixture/desired",
            "terminal":true,"launch":{"type":"shell","value":"true"},"environment":{},"tags":{"st3.subject":subject},
            "display_name":Value::Null,"lifecycle":"service","restart":"always",
            "restart_intensity":{"attempts":3,"interval_ms":60000,"delay_ms":0,"mode":"delay"},
            "shutdown_timeout_ms":5000,"driver":Value::Null}})
}
struct Fault {
    database: bool,
}
impl View for Fault {
    fn definition(&self) -> Definition {
        Definition {
            name: "fault",
            fingerprint: "fault.v1",
            kinds: &["fault.input"],
            local_kinds: &[],
            max_contributions: 1,
        }
    }
    fn contributions(&self, _: &ClaimRecord, _: &canonical::ClaimKey) -> Result<Vec<Contribution>> {
        if self.database {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
                None,
            )
            .into());
        }
        anyhow::bail!("operator cannot maintain admitted input")
    }
}
#[test]
fn operator_gap_is_durable_and_a_ready_boolean_cannot_certify_missing_work() {
    let rt = Arc::new(
        ViewRuntime::asynchronous(
            Views::new(vec![
                Box::new(Fault { database: false }),
                Box::new(real_views::Family::Mailbox),
            ])
            .unwrap(),
            Limits::default(),
        )
        .unwrap(),
    );
    let store = Store::open_memory("alder", rt.clone()).unwrap();
    append(&store, "fault/a", "fault.input", None, json!({"x":1}));
    drain(&store, &rt);
    assert_eq!(
        rt.views
            .readiness(&store.readers.get(), "fault", 1)
            .unwrap(),
        Readiness::Fenced
    );
    assert!(matches!(
        rt.views
            .readiness(&store.readers.get(), "mailbox", 1)
            .unwrap(),
        Readiness::Ready(_)
    ));
    assert_eq!(
        async_views::applied_prefix(&store.readers.get(), &rt.views, "fault", 1).unwrap(),
        None
    );
    store
        .connection
        .batched(|tx| tx.execute("UPDATE ivm_views SET ready=1 WHERE name='fault'", []))
        .unwrap()
        .unwrap();
    assert_eq!(
        rt.views
            .readiness(&store.readers.get(), "fault", 1)
            .unwrap(),
        Readiness::SourcePending
    );
}
#[test]
fn page_storage_failure_rolls_back_output_prefix_and_queue_reclamation() {
    let rt = Arc::new(
        ViewRuntime::asynchronous(
            Views::new(vec![
                Box::new(real_views::Family::Mailbox),
                Box::new(Fault { database: true }),
            ])
            .unwrap(),
            Limits::default(),
        )
        .unwrap(),
    );
    let store = Store::open_memory("alder", rt.clone()).unwrap();
    store
        .connection
        .batched(|tx| events::install(tx, 128))
        .unwrap()
        .unwrap();
    let publisher = Publisher::attach(&store, 8).unwrap();
    append(
        &store,
        "message/a",
        "message.sent",
        None,
        json!({"to":"person/ada","from":"person/sender","body":"rollback this output"}),
    );
    append(&store, "fault/a", "fault.input", None, json!({"x":1}));
    let before: u64 = store
        .readers
        .get()
        .query_row(
            "SELECT generation FROM ivm_views WHERE name='mailbox'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let mut notices = publisher.subscribe();
    assert!(async_views::step(&store, &rt).is_err());
    assert_eq!(
        async_views::state(&store.readers.get())
            .unwrap()
            .retained_rows,
        2
    );
    assert!(rows(&store.readers.get(), "mailbox").is_empty());
    assert_eq!(
        store
            .readers
            .get()
            .query_row(
                "SELECT generation FROM ivm_views WHERE name='mailbox'",
                [],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
        before
    );
    assert!(matches!(
        notices.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert_eq!(
        source_cut(&store.readers.get()).unwrap().unwrap().projected,
        0
    );
    assert!(
        store
            .readers
            .get()
            .query_row("SELECT ready FROM ivm_views WHERE name='fault'", [], |r| {
                r.get::<_, bool>(0)
            })
            .unwrap()
    );
    assert_eq!(
        rt.views
            .readiness(&store.readers.get(), "fault", 1)
            .unwrap(),
        Readiness::SourcePending
    );
}
#[test]
fn oversized_legacy_rank_fences_only_affected_views_without_counting_the_batch() {
    let (store, rt) = fixture(Limits {
        legacy_rank_rows: 2,
        ..Limits::default()
    });
    let first = append(
        &store,
        "message/a",
        "message.read",
        None,
        json!({"reader":"person/ada","n":0}),
    );
    for n in 1..4 {
        store
            .connection
            .batched(|tx| {
                rt.append_claim_tx(
                    tx,
                    &store.origin,
                    "message/a",
                    "message.read",
                    None,
                    &json!({"fields":{"reader":"person/ada","n":n}}),
                    &[],
                    Some(&first.batch_id),
                )
            })
            .unwrap()
            .unwrap();
    }
    drain(&store, &rt);
    assert_eq!(
        rt.views
            .readiness(&store.readers.get(), "mailbox", 1)
            .unwrap(),
        Readiness::Fenced
    );
    assert!(matches!(
        rt.views
            .readiness(&store.readers.get(), "agent-card", 1)
            .unwrap(),
        Readiness::Ready(_)
    ));
    assert_eq!(
        async_views::applied_prefix(&store.readers.get(), &rt.views, "mailbox", 1).unwrap(),
        None
    );
}

#[test]
fn accepted_repair_preserves_admission_but_fences_unsupported_view_recovery() {
    let (store, rt) = fixture(Limits::default());
    let original = append(
        &store,
        "message/a",
        "message.sent",
        None,
        json!({"to":"person/ada","from":"person/sender","body":"original"}),
    );
    let replacement = append(
        &store,
        "message/a",
        "message.sent",
        None,
        json!({"to":"person/ada","from":"person/sender","body":"replacement"}),
    );
    store.seal_local_batches().unwrap();
    drain(&store, &rt);
    let record: String = store
        .readers
        .get()
        .query_row(
            "SELECT record_ref FROM replica_records WHERE claim_id=?1",
            [&original.id],
            |r| r.get(0),
        )
        .unwrap();
    let repair = append(
        &store,
        "repair/a",
        "record.repaired",
        None,
        json!({"record":record,"replacement":replacement.id}),
    );
    assert_eq!(store.apply_replication_repairs().unwrap(), 1);
    assert_eq!(
        store
            .readers
            .get()
            .query_row(
                "SELECT state FROM replica_records WHERE record_ref=?1",
                [&record],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "repaired"
    );
    drain(&store, &rt);
    let reader = store.readers.get();
    assert_eq!(
        source_cut(&reader).unwrap().unwrap().projected,
        repair.store_index
    );
    assert_eq!(
        rt.views.readiness(&reader, "mailbox", 1).unwrap(),
        Readiness::Fenced
    );
    assert_eq!(
        async_views::applied_prefix(&reader, &rt.views, "mailbox", 1).unwrap(),
        None
    );
    assert!(
        rt.views
            .availability(&reader, "mailbox", 1)
            .unwrap()
            .error
            .unwrap()
            .contains("repair")
    );
    assert!(matches!(
        rt.views.readiness(&reader, "agent-card", 1).unwrap(),
        Readiness::Ready(_)
    ));
    // A later page must continue healthy views even though mailbox has no complete prefix.
    append(&store, "noise", "future.kind", None, json!({}));
    drain(&store, &rt);
}

#[cfg(feature = "test-support")]
#[test]
#[ignore = "isolated writer-cost unit; run this exact test alone with --ignored --nocapture"]
fn unchanged_answer_writer_cost_and_retained_candidate_growth() {
    use smallclaims::sqlite::work;
    use std::time::Instant as Clock;
    // Global work counters require one filtered test process, not a concurrent suite.
    for history_size in [100, 1000] {
        let (store, rt) = fixture(Limits::default());
        let publisher = Publisher::attach(&store, 16).unwrap();
        append(
            &store,
            "message/a",
            "message.sent",
            None,
            json!({"to":"person/ada","from":"person/sender","body":"hello"}),
        );
        append(
            &store,
            "message/a",
            "message.read",
            None,
            json!({"reader":"person/ada"}),
        );
        drain(&store, &rt);
        for n in 0..history_size {
            append(
                &store,
                "message/a",
                "message.read",
                None,
                json!({"reader":"person/ada","n":n}),
            );
            drain(&store, &rt);
        }
        let before = rt.views.token(&store.readers.get(), "mailbox", 1).unwrap();
        let mut admission_us = vec![];
        let mut page_us = vec![];
        let mut admission_vm = vec![];
        let mut page_vm = vec![];
        for n in 0..20 {
            let waiting = Clock::now();
            let mut writer = store.connection.write();
            let held = Clock::now();
            let wait_us = waiting.elapsed().as_micros();
            let measured = work::total();
            let tx = writer.transaction().unwrap();
            rt.append_claim_tx(
                &tx,
                &store.origin,
                "message/a",
                "message.read",
                None,
                &json!({"fields":{"reader":"person/ada","probe":n}}),
                &[],
                None,
            )
            .unwrap();
            tx.commit().unwrap();
            drop(writer); // Inclusive of commit observer and writer return.
            admission_us.push(held.elapsed().as_micros());
            admission_vm.push(work::total() - measured);
            let measured = work::total();
            let report = async_views::step(&store, &rt).unwrap();
            page_vm.push(work::total() - measured);
            page_us.push(report.writer_held_us);
            assert_eq!(report.rows, 1);
            assert_eq!(report.retained_rows, 0);
            println!(
                "sample history={history_size} wait_us={wait_us} page_wait_us={} raw_held_us={} page_held_us={} rows={} bytes={}",
                report.writer_wait_us,
                admission_us.last().unwrap(),
                report.writer_held_us,
                report.rows,
                report.bytes
            );
        }
        assert_eq!(
            rt.views.token(&store.readers.get(), "mailbox", 1).unwrap(),
            before
        );
        let retained: (u64,u64) = store.readers.get().query_row(
            "SELECT COUNT(*),COALESCE(SUM(length(key)+length(register)+length(value)+length(rank)+length(claim_id)),0) FROM ivm_contributions",
            [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
        admission_us.sort();
        page_us.sort();
        println!(
            "summary history={history_size} raw_p99_us={} raw_max_us={} page_p99_us={} page_max_us={} contribution_rows={} contribution_payload_bytes={} admission_work={admission_vm:?} page_work={page_vm:?}",
            admission_us[19], admission_us[19], page_us[19], page_us[19], retained.0, retained.1
        );
        drop(publisher);
    }
    for history_size in [100, 1000] {
        let rt = Arc::new(ViewRuntime::new(definitions()).unwrap());
        let store = Store::open_memory("synchronous-control", rt.clone()).unwrap();
        store
            .connection
            .batched(|tx| events::install(tx, 128))
            .unwrap()
            .unwrap();
        let publisher = Publisher::attach(&store, 16).unwrap();
        append(
            &store,
            "message/a",
            "message.sent",
            None,
            json!({"to":"person/ada","from":"person/sender","body":"hello"}),
        );
        append(
            &store,
            "message/a",
            "message.read",
            None,
            json!({"reader":"person/ada"}),
        );
        for n in 0..history_size {
            append(
                &store,
                "message/a",
                "message.read",
                None,
                json!({"reader":"person/ada","n":n}),
            );
        }
        let before = rt.views.token(&store.readers.get(), "mailbox", 1).unwrap();
        let mut held = vec![];
        let mut samples = vec![];
        for n in 0..20 {
            let mut writer = store.connection.write();
            let started = Clock::now();
            let measured = work::total();
            let tx = writer.transaction().unwrap();
            rt.append_claim_tx(
                &tx,
                &store.origin,
                "message/a",
                "message.read",
                None,
                &json!({"fields":{"reader":"person/ada","probe":n}}),
                &[],
                None,
            )
            .unwrap();
            tx.commit().unwrap();
            drop(writer);
            held.push(started.elapsed().as_micros());
            samples.push(work::total() - measured);
        }
        assert_eq!(
            rt.views.token(&store.readers.get(), "mailbox", 1).unwrap(),
            before
        );
        held.sort();
        println!(
            "synchronous history={history_size} writer_p99_us={} writer_max_us={} work={samples:?}",
            held[19], held[19]
        );
        drop(publisher);
    }
    for fanout in [1, 8, 32] {
        let (store, rt) = fixture(Limits {
            page_rows: fanout,
            ..Limits::default()
        });
        let publisher = Publisher::attach(&store, 16).unwrap();
        let before = rt.views.token(&store.readers.get(), "mailbox", 1).unwrap();
        let mut held = vec![];
        let mut work_samples = vec![];
        for round in 0..20 {
            for n in 0..fanout {
                append(
                    &store,
                    &format!("message/{round}/{n}"),
                    "message.sent",
                    None,
                    json!({"to":"person/ada","from":"person/sender","body":"genuine output"}),
                );
            }
            let measured = work::total();
            let page = async_views::step(&store, &rt).unwrap();
            work_samples.push(work::total() - measured);
            held.push(page.writer_held_us);
            assert_eq!(page.rows, fanout);
            assert_eq!(page.retained_rows, 0);
        }
        assert_eq!(
            rt.views
                .token(&store.readers.get(), "mailbox", 1)
                .unwrap()
                .generation
                - before.generation,
            20 * fanout as u64
        );
        held.sort();
        println!(
            "fanout={fanout} page_p99_us={} page_max_us={} page_work={work_samples:?}",
            held[19], held[19]
        );
        drop(publisher);
    }
}

#[test]
fn missing_or_replaced_prefix_metadata_cannot_certify_partial_output() {
    for mutation in [
        "DELETE FROM ivm_async_views WHERE view='mailbox'",
        "UPDATE ivm_async_views SET fingerprint='changed' WHERE view='mailbox'",
        "UPDATE ivm_source SET projected=admitted",
    ] {
        let (store, rt) = fixture(Limits::default());
        append(
            &store,
            "message/a",
            "message.sent",
            None,
            json!({"to":"person/ada","from":"person/sender","body":"pending"}),
        );
        store.connection.write().execute_batch(mutation).unwrap();
        assert!(!async_views::state(&store.readers.get()).unwrap().available);
        assert!(!matches!(
            rt.views
                .readiness(&store.readers.get(), "mailbox", 1)
                .unwrap(),
            Readiness::Ready(_)
        ));
        assert!(matches!(
            async_views::step(&store, &rt).unwrap().status,
            PageStatus::Stopped(_)
        ));
    }
}

struct Wide;
impl View for Wide {
    fn definition(&self) -> Definition {
        Definition {
            name: "wide",
            fingerprint: "wide.v1",
            kinds: &["wide.input"],
            local_kinds: &[],
            max_contributions: 257,
        }
    }
}
#[test]
fn asynchronous_registry_rejects_unbounded_declared_dependency_fanout() {
    assert!(
        ViewRuntime::asynchronous(Views::new(vec![Box::new(Wide)]).unwrap(), Limits::default())
            .is_err()
    );
    let (store, rt) = fixture(Limits {
        page_rows: 1,
        ..Limits::default()
    });
    for n in 0..4 {
        append(&store, "noise", "future.kind", None, json!({"n":n}));
    }
    let page = async_views::step(&store, &rt).unwrap();
    assert_eq!(page.status, PageStatus::More);
    assert_eq!(page.rows, 1);
    assert_eq!(page.retained_rows, 3);
    append(&store, "later", "future.kind", None, json!({})); // Ordinary admission interleaves.
    drain(&store, &rt);
    assert_eq!(
        source_cut(&store.readers.get()).unwrap().unwrap().projected,
        5
    );
}

#[test]
fn reopening_with_a_reduced_registry_cannot_skip_a_registered_view() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let rt = runtime(Limits::default());
    {
        let store = Store::open(file.path(), "alder", rt.clone()).unwrap();
        append(
            &store,
            "message/a",
            "message.read",
            None,
            json!({"reader":"person/ada"}),
        );
        drain(&store, &rt);
    }
    let reduced = Arc::new(
        ViewRuntime::asynchronous(
            Views::new(vec![Box::new(real_views::Family::Card)]).unwrap(),
            Limits::default(),
        )
        .unwrap(),
    );
    assert!(Store::open(file.path(), "alder", reduced).is_err());
    let store = Store::open(file.path(), "alder", rt.clone()).unwrap();
    assert!(matches!(
        rt.views
            .readiness(&store.readers.get(), "mailbox", 1)
            .unwrap(),
        Readiness::Ready(_)
    ));
}

#[test]
fn direct_input_insertion_behind_the_prefix_fences_without_a_frontier_advance() {
    let (store, rt) = fixture(Limits::default());
    let claim = append(
        &store,
        "message/a",
        "message.read",
        None,
        json!({"reader":"person/ada"}),
    );
    drain(&store, &rt);
    let cut = source_cut(&store.readers.get()).unwrap().unwrap();
    let availability = rt
        .views
        .availability(&store.readers.get(), "mailbox", 1)
        .unwrap()
        .token;
    // Unsupported direct SQL source mutation, not a certified admission/repair lifecycle.
    store.connection.write().execute("INSERT INTO claims(id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms) SELECT 'unverified-backward-fixture',0,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM claims WHERE id=?1",[&claim.id]).unwrap();
    let reader = store.readers.get();
    assert_eq!(source_cut(&reader).unwrap().unwrap(), cut);
    assert!(!async_views::state(&reader).unwrap().available);
    assert!(!matches!(
        rt.views.readiness(&reader, "mailbox", 1).unwrap(),
        Readiness::Ready(_)
    ));
    assert_ne!(
        rt.views.availability(&reader, "mailbox", 1).unwrap().token,
        availability
    );
    assert!(matches!(
        async_views::step(&store, &rt).unwrap().status,
        PageStatus::Stopped(_)
    ));
}

struct DifferentMailbox;
impl View for DifferentMailbox {
    fn definition(&self) -> Definition {
        Definition {
            name: "mailbox",
            fingerprint: "different.v1",
            kinds: &["message.sent"],
            local_kinds: &[],
            max_contributions: 1,
        }
    }
}
#[test]
fn a_worker_cannot_skip_registered_views_by_using_another_runtime() {
    let (store, rt) = fixture(Limits::default());
    append(
        &store,
        "message/a",
        "message.sent",
        None,
        json!({"to":"person/ada","from":"person/sender","body":"pending"}),
    );
    let subset = ViewRuntime::asynchronous(
        Views::new(vec![Box::new(real_views::Family::Mailbox)]).unwrap(),
        Limits::default(),
    )
    .unwrap();
    let different = ViewRuntime::asynchronous(
        Views::new(vec![
            Box::new(real_views::Family::Card),
            Box::new(DifferentMailbox),
            Box::new(real_views::Family::Desired),
        ])
        .unwrap(),
        Limits::default(),
    )
    .unwrap();
    for wrong in [&subset, &different] {
        assert!(async_views::step(&store, wrong).is_err());
        assert_eq!(
            source_cut(&store.readers.get()).unwrap().unwrap().projected,
            0
        );
        assert_eq!(
            async_views::state(&store.readers.get())
                .unwrap()
                .retained_rows,
            1
        );
        assert!(rows(&store.readers.get(), "mailbox").is_empty());
    }
    drain(&store, &rt);
    oracle(&store);
    assert!(matches!(
        rt.views
            .readiness(&store.readers.get(), "mailbox", 1)
            .unwrap(),
        Readiness::Ready(_)
    ));
}

#[test]
fn incompatible_capture_format_is_refused_before_schema_or_startup_work() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let rt = runtime(Limits::default());
    {
        let store = Store::open(file.path(), "alder", rt.clone()).unwrap();
        append(
            &store,
            "message/a",
            "message.read",
            None,
            json!({"reader":"person/ada"}),
        );
        drain(&store, &rt);
        store
            .connection
            .write()
            .execute(
                "UPDATE ivm_async_state SET format='smallclaims.ivm.async.v1'",
                [],
            )
            .unwrap();
    }
    let raw = rusqlite::Connection::open(file.path()).unwrap();
    let cookie: i64 = raw
        .query_row("PRAGMA schema_version", [], |r| r.get(0))
        .unwrap();
    drop(raw);
    assert!(Store::open(file.path(), "alder", rt).is_err());
    let raw = rusqlite::Connection::open(file.path()).unwrap();
    assert_eq!(
        raw.query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        cookie
    );
    assert_eq!(
        raw.query_row("SELECT COUNT(*) FROM ivm_async_queue", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
}
