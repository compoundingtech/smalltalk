use super::*;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

struct FinishProcessOnDrop(PathBuf);

impl Drop for FinishProcessOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, "finish");
    }
}

#[test]
fn first_readiness_and_first_incarnation_beat_writer_backlog_under_mailbox_check_flood() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    let root = tempfile::tempdir().unwrap();
    let finish = root.path().join("finish");
    let _finish_on_drop = FinishProcessOnDrop(finish.clone());
    let runtime = Arc::new(NativeRuntime::new(root.path(), None, Path::new("unused-pty")));
    let reconciler = Reconciler::new(
        store.clone(),
        runtime.clone(),
        "node".into(),
        Arc::new(Notify::new()),
    );
    // Warm the incremental section before the new declaration exists. Its first
    // launch must not depend on the periodic full-pass backstop.
    reconciler.reconcile_once().unwrap();
    apply_source(
        &store,
        &format!(r#"
version 2
agent "promotion-seat" {{
    workspace {:?}
    command {:?}
    restart "never"
}}
mission "promotion-proof" state="ready" {{
    goal "Admit work while unrelated mailbox readers and writers are busy."
    step "first" {{ assigned-to "person/operator" }}
}}
"#, root.path().display().to_string(),
            format!("while [ ! -f {:?} ]; do sleep 0.01; done", finish)),
        "promotion-proof-source",
    );
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "promotion-proof".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/operator".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "promotion-proof-run".into(),
        })
        .unwrap();
    assert_eq!(run.steps[0].status, "pending");
    let stop = AtomicBool::new(false);
    let checks = AtomicU64::new(0);
    let (held, is_held) = std::sync::mpsc::sync_channel(1);
    let (release, released) = std::sync::mpsc::sync_channel(1);
    let result = std::thread::scope(|scope| {
        let held_store = store.clone();
        let holder = scope.spawn(move || {
            held_store.hold_writer_for_test(|| {
                held.send(()).unwrap();
                released.recv().unwrap();
            });
        });
        is_held.recv().unwrap();
        let mut readers = Vec::new();
        for reader in 0..8 {
            let store = &store;
            let stop = &stop;
            let checks = &checks;
            readers.push(scope.spawn(move || {
                let fence = crate::mailbox::Fence::new(
                    &format!("agent/mailbox-pressure-{reader}"),
                    "pressure-incarnation",
                    "pressure-component",
                );
                let mark = store.mailbox_watermark(&fence).unwrap();
                while !stop.load(Ordering::Acquire) {
                    assert!(!store.mailbox_changed_since(&fence, &mark, &[]).unwrap());
                    checks.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        let mut ordinary = Vec::new();
        for _ in 0..24 {
            let store = &store;
            ordinary.push(scope.spawn(move || {
                store.hold_writer_for_test(|| {
                    std::thread::sleep(Duration::from_millis(250));
                });
            }));
        }
        let queued_until = Instant::now() + Duration::from_secs(10);
        while store.pending_writer_jobs_for_test().1 != 24 {
            if Instant::now() >= queued_until {
                release.send(()).unwrap();
                stop.store(true, Ordering::Release);
                panic!("ordinary writer backlog did not enqueue");
            }
            std::thread::yield_now();
        }
        let promotion = scope.spawn(|| reconciler.reconcile_once().unwrap());
        let control_until = Instant::now() + Duration::from_secs(10);
        while store.pending_writer_jobs_for_test().0 == 0 {
            if Instant::now() >= control_until {
                release.send(()).unwrap();
                stop.store(true, Ordering::Release);
                panic!("mission transition did not reach the control writer queue");
            }
            std::thread::yield_now();
        }
        let started = Instant::now();
        release.send(()).unwrap();
        let bound = Duration::from_secs(3);
        let mut ready = None;
        let mut launched = None;
        while started.elapsed() < bound {
            let view = store.mission_run_for_reconcile(&run.id).unwrap();
            if ready.is_none() && view.steps[0].status == "ready" {
                ready = Some((started.elapsed(), view.steps[0].readiness_epoch));
            }
            if launched.is_none()
                && let Some(actual) = store.latest_actual_value("agent/promotion-seat").unwrap()
                && actual_field(&actual, "status").and_then(Value::as_str) == Some("running")
                && let Some(incarnation) = actual_field(&actual, "incarnation_id").and_then(Value::as_str)
            {
                launched = Some((started.elapsed(), incarnation.to_owned()));
            }
            if ready.is_some() && launched.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        std::fs::write(&finish, "finish").unwrap();
        stop.store(true, Ordering::Release);
        for reader in readers {
            reader.join().unwrap();
        }
        holder.join().unwrap();
        for writer in ordinary {
            writer.join().unwrap();
        }
        promotion.join().unwrap();
        (ready, launched)
    });
    let (elapsed, epoch) = result.0.expect("first readiness missed its 3s bound behind a 6s backlog");
    let (launch_elapsed, incarnation) = result.1.expect("first incarnation missed its 3s bound");
    let observed = runtime.observe_exec("promotion-seat").unwrap().unwrap();
    assert_eq!(observed.incarnation_id.as_deref(), Some(incarnation.as_str()));
    assert_eq!(epoch, 1, "promotion is one durable readiness transition");
    eprintln!(
        "first readiness: {} ms; first incarnation: {} ms; mailbox checks: {}; ordinary backlog: 24 x 250ms; bound: 3000ms",
        elapsed.as_millis(),
        launch_elapsed.as_millis(),
        checks.load(Ordering::Relaxed),
    );
}
