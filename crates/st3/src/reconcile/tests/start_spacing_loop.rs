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
