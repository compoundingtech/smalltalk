//! Real background dispatch, with per-instance test probes; no gate or pass replacement.
use super::*;
use smallclaims::sqlite::work::SqliteWork;
use tokio::sync::mpsc;

async fn receive<T>(receiver: &mut mpsc::UnboundedReceiver<T>) -> T {
    // Synchronization timeout, not a reaction-latency acceptance budget.
    tokio::time::timeout(Duration::from_secs(30), receiver.recv())
        .await
        .expect("the real background dispatch did not reach its probe")
        .expect("the background probe closed")
}

#[tokio::test]
async fn default_background_starts_are_spaced_and_reach_the_probe_after_coalesced_input() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    let runtime = Arc::new(FakeRuntime::default());
    let current_input = Arc::new(AtomicUsize::new(0));
    let (snapshots, mut snapshot_rx) = mpsc::unbounded_channel();
    let (release_snapshot, held_snapshot) = std::sync::mpsc::channel();
    let snapshot_number = AtomicUsize::new(0);
    *runtime.before_snapshot.lock().unwrap() = Some(Box::new({
        let current_input = current_input.clone();
        move || {
            let _ = snapshots.send(current_input.load(Ordering::SeqCst));
            if snapshot_number.fetch_add(1, Ordering::SeqCst) < 2 {
                // Keep the preceding real pass here until the test has published the entire
                // next burst; a delayed test consumer cannot race a subsequent admission.
                held_snapshot.recv_timeout(Duration::from_secs(30)).unwrap();
            }
        }
    }));
    let notify = Arc::new(Notify::new());
    let (events, mut completed) = watch::channel(0_u64);
    let (entries, mut entry_rx) = mpsc::unbounded_channel();
    let mut reconciler =
        Reconciler::new(store, runtime, "node".into(), notify.clone()).with_event_notify(events);
    reconciler.background_entry_hook = Some(Arc::new(move |admitted, last_start, work| {
        let _ = entries.send((admitted, last_start, work));
    }));
    let reconciler = Arc::new(reconciler); // Keep the default30 setting, including repeats.
    let task = tokio::spawn(reconciler.clone().run());
    let (admitted, first, work) = receive(&mut entry_rx).await;
    assert!(admitted);
    let first = first.expect("an admitted pass must record its actual start");
    assert_eq!(work, SqliteWork::default()); // Admission precedes the first pass SQL.
    assert_eq!(receive(&mut snapshot_rx).await, 0);
    for value in 1..=100 {
        current_input.store(value, Ordering::SeqCst);
        notify.notify_one();
    }
    release_snapshot.send(()).unwrap();
    let (admitted, second, work) = receive(&mut entry_rx).await;
    assert!(admitted);
    let second = second.expect("an admitted pass must record its actual start");
    assert!(second.duration_since(first) >= Duration::from_secs(2));
    assert_eq!(work, SqliteWork::default());
    // Real reconcile_pass reaches the FakeRuntime snapshot probe after burst publication.
    // The returned PTY roster remains empty: this is no runtime-state/decision parity oracle.
    assert_eq!(receive(&mut snapshot_rx).await, 100);
    // A further notification burst must also use the same cap, whether the intervening
    // convergence pass was quiet or selected another changed-repeat.
    for value in 101..=200 {
        current_input.store(value, Ordering::SeqCst);
        notify.notify_one();
    }
    release_snapshot.send(()).unwrap();
    let (admitted, third, work) = receive(&mut entry_rx).await;
    assert!(admitted);
    let third = third.expect("an admitted pass must record its actual start");
    assert!(third.duration_since(second) >= Duration::from_secs(2));
    assert_eq!(work, SqliteWork::default());
    assert_eq!(receive(&mut snapshot_rx).await, 200);
    tokio::time::timeout(Duration::from_secs(30), async {
        while *completed.borrow_and_update() < 3 {
            completed.changed().await.unwrap();
        }
    })
    .await
    .expect("the admitted real passes did not complete");
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
}

#[test]
fn cancelled_real_blocking_queue_cannot_begin_pass_sql_or_reserve_start_history() {
    // One actual blocking worker lets the fixture hold the queue before run() enqueues its pass.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let before = store.index().unwrap();
        let (release, held) = std::sync::mpsc::channel();
        let holder = Arc::new(Mutex::new(None));
        let (dispatched, mut dispatch_rx) = mpsc::unbounded_channel();
        let (entries, mut entry_rx) = mpsc::unbounded_channel();
        let (events, mut completed) = watch::channel(0_u64);
        let mut reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        )
        .with_event_notify(events);
        *reconciler.background_dispatch_hook.lock().unwrap() = Some(Box::new({
            let holder = holder.clone();
            move || {
                let (started, running) = std::sync::mpsc::channel();
                let job = tokio::task::spawn_blocking(move || {
                    started.send(()).unwrap();
                    held.recv_timeout(Duration::from_secs(30)).unwrap();
                });
                // The preceding deadline read has returned. Occupy the sole worker before the
                // real pass is submitted, without replacing its spawn_blocking closure.
                running.recv_timeout(Duration::from_secs(30)).unwrap();
                *holder.lock().unwrap() = Some(job);
                let _ = dispatched.send(());
            }
        }));
        reconciler.background_entry_hook = Some(Arc::new(move |admitted, last_start, work| {
            let _ = entries.send((admitted, last_start, work));
        }));
        let reconciler = Arc::new(reconciler);
        let task = tokio::spawn(reconciler.clone().run());
        receive(&mut dispatch_rx).await;
        assert!(entry_rx.try_recv().is_err());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled()); // Drops the actual queued admission.
        release.send(()).unwrap();
        let holder_job = holder.lock().unwrap().take().unwrap();
        holder_job.await.unwrap();
        // FIFO sentinel ensures the cancelled real pass closure has reached blocking entry.
        tokio::task::spawn_blocking(|| ()).await.unwrap();
        let (admitted, last_start, work) = receive(&mut entry_rx).await;
        assert!(!admitted);
        assert!(last_start.is_none());
        assert_eq!(work, SqliteWork::default());
        assert_eq!(store.index().unwrap(), before);
        assert!(reconciler.start_spacing.last_start().is_none());
        assert_eq!(*completed.borrow(), 0);

        // Restart the same Reconciler: a cancelled queue attempt did not consume its first start.
        let task = tokio::spawn(reconciler.clone().run());
        let (admitted, last_start, work) = receive(&mut entry_rx).await;
        assert!(admitted);
        assert!(last_start.is_some());
        assert_eq!(work, SqliteWork::default());
        tokio::time::timeout(Duration::from_secs(30), completed.changed())
            .await
            .unwrap()
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QuiescenceTraffic {
    None,
    Unrelated,
    Deduplication,
    StartCompletion,
    OwnEffects,
}

async fn quiescence_pass_counts(traffic: QuiescenceTraffic) -> Vec<usize> {
    const SUBJECTS: usize = 3;
    let store = Arc::new(Store::open_memory("node").unwrap());
    let workspace = tempfile::tempdir().unwrap();
    let mut source = String::from("version 2\n");
    for subject in 0..SUBJECTS {
        source.push_str(&format!(
            "exec \"quiet-{subject}\" {{ workspace \"{}\"; command \"true\"; restart \"never\" }}\n",
            workspace.path().display()
        ));
    }
    apply_source(&store, &source, "quiescence");
    assert_eq!(store.desired_subjects().unwrap().len(), SUBJECTS);
    let runtime = Arc::new(FakeRuntime::default());
    let mut reconciler = Reconciler::new(
        store.clone(),
        runtime.clone(),
        "node".into(),
        Arc::new(Notify::new()),
    );
    reconciler.set_max_passes_per_minute(600).unwrap();
    // Settle the fixture's own effects before measuring the startup wake.
    for _ in 0..4 {
        reconciler.reconcile_once().unwrap();
    }
    let before = store.index().unwrap();
    reconciler.reconcile_once().unwrap();
    assert_eq!(store.index().unwrap(), before);
    let local_fields = BTreeMap::from([
        ("reason".into(), Value::String("already recorded".into())),
    ]);
    if traffic == QuiescenceTraffic::Deduplication {
        reconciler.record_once("exec/unrelated", "runtime.action.succeeded", local_fields.clone()).unwrap();
        let claim = store.latest_observation("exec/unrelated", "runtime.action.succeeded").unwrap().unwrap();
        assert!(crate::store::local_observation_position(&claim).is_some());
    }
    let (wakes, mut wake_rx) = mpsc::unbounded_channel();
    reconciler.background_wake_hook = Some(Arc::new(move |passes| {
        let _ = wakes.send(passes);
    }));
    let reconciler = Arc::new(reconciler);
    let snapshots = Arc::new(AtomicUsize::new(0));
    *runtime.before_snapshot.lock().unwrap() = Some(Box::new({
        let snapshots = snapshots.clone();
        let reconciler = Arc::downgrade(&reconciler);
        move || {
            let pass = snapshots.fetch_add(1, Ordering::SeqCst);
            if traffic == QuiescenceTraffic::Unrelated && pass < 64 {
                // Write on another thread, as replication does. Join makes the write land
                // strictly between the pass's before/after readings, without timing races.
                let store = store.clone();
                std::thread::spawn(move || {
                    store.append_claim(&ClaimInput {
                        subject: format!("message/unrelated-{pass}"),
                        kind: "message.sent".into(),
                        actor: Some("person/test".into()),
                        fields: BTreeMap::from([
                            ("from".into(), Value::String("person/test".into())),
                            ("to".into(), Value::String("person/other".into())),
                            ("content".into(), Value::String("unrelated".into())),
                            ("status".into(), Value::String("sent".into())),
                        ]),
                        evidence: vec![],
                        expected_subject: None,
                        idempotency_key: None,
                    }).unwrap();
                }).join().unwrap();
            }
            if traffic == QuiescenceTraffic::Deduplication {
                let before = smallclaims::touched::writes();
                reconciler.upgrade().unwrap()
                    .record_once("exec/unrelated", "runtime.action.succeeded", local_fields.clone()).unwrap();
                assert_eq!(smallclaims::touched::writes(), before);
            }
            if traffic == QuiescenceTraffic::OwnEffects && pass < 64 {
                let mut fields = local_fields.clone();
                fields.insert("reason".into(), Value::String(format!("pass {pass}")));
                reconciler.upgrade().unwrap()
                    .record_once("exec/unrelated", "runtime.action.succeeded", fields).unwrap();
            }
        }
    }));
    let task = tokio::spawn(reconciler.clone().run());
    let first = receive(&mut wake_rx).await;
    let mut counts = vec![first];
    if traffic == QuiescenceTraffic::StartCompletion {
        let reconciler = reconciler.clone();
        tokio::task::spawn_blocking(move || {
            let subject = reconciler.store.desired_subjects().unwrap().into_iter()
                .find(|subject| subject.subject == "exec/quiet-0").unwrap();
            let before = smallclaims::touched::writes();
            // Complete only after the initial pass rests. This real start path's
            // signal_changed must supply the next wake; the test never calls notify.
            reconciler.perform_start(
                &subject, subject.member.as_ref().unwrap(), "helper completed start",
            ).unwrap();
            assert!(smallclaims::touched::writes() > before);
        }).await.unwrap();
    }
    if first == 64 || traffic == QuiescenceTraffic::StartCompletion {
        // The cap or the helper's real signal_changed must supply this notification.
        counts.push(receive(&mut wake_rx).await);
    }
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    eprintln!("traffic={traffic:?}: passes_per_wake={counts:?}");
    counts
}

#[tokio::test]
async fn background_quiesces_without_concurrent_writes() {
    assert_eq!(quiescence_pass_counts(QuiescenceTraffic::None).await, vec![1]);
}

#[tokio::test]
async fn background_quiesces_under_unrelated_claim_writes() {
    let counts = quiescence_pass_counts(QuiescenceTraffic::Unrelated).await;
    assert!(counts.len() == 1 && counts[0] <= 2, "passes_per_wake={counts:?}");
}

#[tokio::test]
async fn background_quiesces_after_deduping_local_only_observations() {
    assert_eq!(quiescence_pass_counts(QuiescenceTraffic::Deduplication).await, vec![1]);
}

#[tokio::test]
async fn background_start_completion_on_a_helper_thread_wakes_a_quiet_pass() {
    // The completion wakes a pass, which records the new observation and confirms it once.
    assert_eq!(quiescence_pass_counts(QuiescenceTraffic::StartCompletion).await, vec![1, 2]);
}

#[tokio::test]
async fn background_own_local_effects_keep_the_64_pass_cap_and_self_notify() {
    assert_eq!(quiescence_pass_counts(QuiescenceTraffic::OwnEffects).await, vec![64, 1]);
}
