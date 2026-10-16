//! A blocked backend start must leave the real reconcile loop free to stop it.
use super::*;
use tokio::sync::mpsc;

struct BarrierRuntime {
    inner: FakeRuntime,
    store: Arc<Store>,
    entered: mpsc::UnboundedSender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    stopped: mpsc::UnboundedSender<Option<String>>,
    publish_early: bool,
    honor_guard: bool,
    operations: Mutex<HashMap<String, String>>,
    started: mpsc::UnboundedSender<String>,
    reject_gates: AtomicBool,
    verification_hidden: AtomicBool,
    provenance_error: AtomicBool,
    ending_unverified: AtomicBool,
}

impl RuntimeControl for BarrierRuntime {
    fn snapshot_ptys(&self) -> Result<Vec<RuntimeObservation>> {
        self.inner.snapshot_ptys()
    }
    fn observe_exec(&self, id: &str) -> Result<Option<RuntimeObservation>> {
        self.inner.observe_exec(id)
    }
    fn observe_start(&self, member: &MemberSpec) -> Result<Option<RuntimeObservation>> {
        if self.verification_hidden.load(Ordering::Acquire) {
            return Ok(None);
        }
        if self
            .operations
            .lock()
            .unwrap()
            .contains_key(&member.runtime_id)
        {
            Ok(self
                .inner
                .ptys
                .lock()
                .unwrap()
                .iter()
                .find(|item| item.runtime_id == member.runtime_id)
                .cloned())
        } else {
            self.inner.observe_start(member)
        }
    }
    fn start(&self, member: &MemberSpec) -> Result<()> {
        self.start_guarded(member, &|| Ok(()))
    }
    fn start_guarded(&self, member: &MemberSpec, guard: &dyn Fn() -> Result<()>) -> Result<()> {
        if member.runtime_id == "example.worker" || member.runtime_id.starts_with("example.held") {
            self.operations.lock().unwrap().insert(
                member.runtime_id.clone(),
                member.tags[start_jobs::OPERATION_TAG].clone(),
            );
            if self.publish_early {
                self.publish(member);
            }
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv()?;
            if self.honor_guard {
                guard()?;
            }
            if !self.publish_early {
                self.publish(member);
            }
        } else {
            guard()?;
        }
        if self.reject_gates.load(Ordering::Acquire)
            && member
                .driver
                .as_deref()
                .is_some_and(|driver| driver.ends_with("gate"))
        {
            self.inner
                .failed_starts
                .lock()
                .unwrap()
                .insert(member.runtime_id.clone());
        }
        self.inner.start(member)?;
        let _ = self.started.send(member.runtime_id.clone());
        Ok(())
    }
    fn start_operation(
        &self,
        id: &str,
        _terminal: bool,
        incarnation: &str,
    ) -> Result<Option<String>> {
        anyhow::ensure!(
            !self.provenance_error.load(Ordering::Acquire),
            "fixture provenance unavailable"
        );
        if incarnation != "blocked-start-incarnation" {
            return self.inner.start_operation(id, _terminal, incarnation);
        }
        Ok(self.operations.lock().unwrap().get(id).cloned())
    }
    fn start_operation_closed(&self, member: &MemberSpec) -> Result<bool> {
        // The held fixture has no detached producer or work scope after its
        // exact operation-owned runtime has exited. A running row is no proof.
        let ended = self.inner.ptys.lock().unwrap().iter().any(|observation| {
            observation.runtime_id == member.runtime_id
                && observation.incarnation_id.as_deref() == Some("blocked-start-incarnation")
                && matches!(observation.status.as_str(), "exited" | "stopped")
        }) && self.operations.lock().unwrap().get(&member.runtime_id)
            == member.tags.get(start_jobs::OPERATION_TAG);
        Ok((ended && !self.ending_unverified.load(Ordering::Acquire))
            || (self.reject_gates.load(Ordering::Acquire)
                && member
                    .driver
                    .as_deref()
                    .is_some_and(|driver| driver.ends_with("gate"))))
    }
    fn stop(&self, id: &str, terminal: bool, incarnation: Option<&str>) -> Result<()> {
        let subject = format!("agent/{}", id.replace('.', "/"));
        let operation = self.store.runtime_start_for(&subject)?.unwrap();
        assert!(
            operation.revoked_by.is_some(),
            "physical control preceded durable revocation"
        );
        assert!(
            self.store
                .observations_for(&subject, "runtime.action.requested")?
                .iter()
                .any(|claim| claim.body["fields"]["action"] == "terminate")
        );
        self.inner.stop(id, terminal, incarnation)?;
        let _ = self.stopped.send(incarnation.map(str::to_owned));
        Ok(())
    }
    fn kill(&self, id: &str, terminal: bool, incarnation: Option<&str>) -> Result<()> {
        let subject = format!("agent/{}", id.replace('.', "/"));
        assert!(
            self.store
                .runtime_start_for(&subject)?
                .unwrap()
                .revoked_by
                .is_some()
        );
        let deadline = self
            .store
            .latest_observation(&subject, "runtime.action.deadline-reached")?
            .unwrap();
        assert_eq!(
            deadline.body["fields"]["incarnation_id"].as_str(),
            incarnation
        );
        assert!(
            self.store
                .observations_for(&subject, "runtime.action.requested")?
                .iter()
                .any(|request| deadline.body["evidence"][0] == request.id)
        );
        self.inner.kill(id, terminal, incarnation)
    }
    fn remove(&self, id: &str, terminal: bool) -> Result<()> {
        self.inner.remove(id, terminal)
    }
    fn screen(&self, id: &str) -> Result<String> {
        self.inner.screen(id)
    }
    fn send_key(&self, id: &str, key: &str) -> Result<()> {
        self.inner.send_key(id, key)
    }
    fn read_exec_log(&self, id: &str) -> Result<Option<String>> {
        self.inner.read_exec_log(id)
    }
}

impl BarrierRuntime {
    fn publish(&self, member: &MemberSpec) {
        self.inner.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id: member.runtime_id.clone(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("blocked-start-incarnation".into()),
        });
    }
}

struct ReleaseOnDrop(std::sync::mpsc::Sender<()>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        for _ in 0..8 {
            let _ = self.0.send(());
        }
    }
}

async fn workers_done<R: RuntimeControl>(reconciler: &Arc<Reconciler<R>>) {
    let live = reconciler.start_jobs.live.clone();
    tokio::time::timeout(
        Duration::from_secs(30),
        tokio::task::spawn_blocking(move || {
            let mut jobs = live.0.lock().unwrap();
            while !jobs.is_empty() {
                jobs = live.1.wait(jobs).unwrap();
            }
        }),
    )
    .await
    .unwrap()
    .unwrap();
}

/// `sleep` as a program path. The Nix build sandbox has no `/bin/sleep`, so look it up on PATH
/// and fall back to the usual location.
fn sleep_program() -> String {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("sleep"))
        .find(|candidate| candidate.is_file())
        .map(|candidate| candidate.display().to_string())
        .unwrap_or_else(|| "/bin/sleep".into())
}

fn fixture(
    publish_early: bool,
    honor_guard: bool,
) -> (
    tempfile::TempDir,
    Arc<Store>,
    Arc<BarrierRuntime>,
    Arc<Reconciler<BarrierRuntime>>,
    ReleaseOnDrop,
    mpsc::UnboundedReceiver<()>,
) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&directory.path().join("claims.db"), "node").unwrap());
    let (entered, entered_rx) = mpsc::unbounded_channel();
    let (release, held) = std::sync::mpsc::channel();
    let (stopped, _) = mpsc::unbounded_channel();
    let (started, _) = mpsc::unbounded_channel();
    let runtime = Arc::new(BarrierRuntime {
        inner: FakeRuntime::default(),
        store: store.clone(),
        entered,
        release: Mutex::new(held),
        stopped,
        publish_early,
        honor_guard,
        operations: Mutex::default(),
        started,
        reject_gates: AtomicBool::new(false),
        verification_hidden: AtomicBool::new(false),
        provenance_error: AtomicBool::new(false),
        ending_unverified: AtomicBool::new(false),
    });
    let reconciler = Arc::new(Reconciler::new(
        store.clone(),
        runtime.clone(),
        "node".into(),
        Arc::new(Notify::new()),
    ));
    reconciler
        .wait_for_test_starts
        .store(false, Ordering::Release);
    (
        directory,
        store,
        runtime,
        reconciler,
        ReleaseOnDrop(release),
        entered_rx,
    )
}

fn launch(reconciler: &Reconciler<BarrierRuntime>, subject: &str) -> bool {
    let desired = reconciler
        .store
        .desired_subjects_named(&[subject.to_owned()])
        .unwrap()
        .remove(0);
    reconciler
        .perform_start_for_request(
            &desired,
            desired.member.as_ref().unwrap(),
            "test launch",
            None,
        )
        .unwrap()
}

#[test]
fn retracted_and_restored_token_cannot_admit_a_different_prepared_declaration() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.db");
    let store = Arc::new(Store::open(&path, "node").unwrap());
    let runtime = Arc::new(FakeRuntime::default());
    let reconciler = Reconciler::new(
        store.clone(),
        runtime.clone(),
        "node".into(),
        Arc::new(Notify::new()),
    );
    let subject = "exec/example/worker";
    apply_source(
        &store,
        "version 2\nexec \"example/worker\" { workspace \"/tmp\"; command \"true\" }",
        "aba-a",
    );
    let stale = store
        .desired_subjects_named(&[subject.into()])
        .unwrap()
        .remove(0);

    // Save genuine selected rows. Retraction selects A between the separate
    // token/body reads; restoration selects the identical B token at admission.
    store
        .connection
        .batched(|tx| {
            tx.execute(
                "CREATE TEMP TABLE aba_a AS SELECT * FROM desired WHERE subject=?1",
                [subject],
            )
        })
        .unwrap()
        .unwrap();
    apply_source(
        &store,
        "version 2\nexec \"example/worker\" { workspace \"/tmp\"; command \"false\" }",
        "aba-b",
    );
    let token = store.selected_desired_token(subject).unwrap();
    store
        .connection
        .batched(|tx| {
            tx.execute(
                "CREATE TEMP TABLE aba_b AS SELECT * FROM desired WHERE subject=?1",
                [subject],
            )
        })
        .unwrap()
        .unwrap();
    store.connection.batched(|tx| tx.execute_batch("DELETE FROM desired WHERE subject='exec/example/worker'; INSERT INTO desired SELECT * FROM aba_a;")).unwrap().unwrap();
    let after_retraction = store
        .desired_subjects_named(&[subject.into()])
        .unwrap()
        .remove(0);
    assert_eq!(after_retraction.kind, stale.kind);
    assert_eq!(after_retraction.desired, stale.desired);
    store.connection.batched(|tx| tx.execute_batch("DELETE FROM desired WHERE subject='exec/example/worker'; INSERT INTO desired SELECT * FROM aba_b;")).unwrap().unwrap();
    assert_eq!(store.selected_desired_token(subject).unwrap(), token);
    let member = stale.member.as_ref().unwrap().clone();
    let prepared = start_jobs::PreparedStart {
        subject: stale,
        launch_member: member.clone(),
        member,
        reason: "retract/restore regression".into(),
        request: None,
        desired_token: "stale-a".into(),
        selected_token: token,
        placement_evidence: Vec::new(),
        start_request: String::new(),
        start_request_receipt: None,
        control_request: None,
        control_request_receipt: None,
        expected_incarnation: None,
        resume_operation: None,
    };
    assert!(!reconciler.dispatch_start(prepared).unwrap());
    assert!(
        store
            .observations_for(subject, "runtime.action.requested")
            .unwrap()
            .is_empty()
    );
    assert!(store.pending_runtime_starts().unwrap().is_empty());
    assert!(runtime.starts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_old_snapshot_cannot_replace_the_incarnation_completed_by_a_worker() {
    let (_directory, store, runtime, reconciler, release, mut entered) = fixture(false, true);
    apply_source(
        &store,
        "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"true\" }",
        "paused-old-snapshot",
    );
    let desired = store
        .desired_subjects_named(&["agent/example/worker".into()])
        .unwrap()
        .remove(0);
    let member = desired.member.as_ref().unwrap();
    let old = RuntimeObservation {
        runtime_id: member.runtime_id.clone(),
        terminal: true,
        status: "stopped".into(),
        incarnation_id: Some("prior-incarnation".into()),
        exit_code: None,
    };
    runtime.inner.ptys.lock().unwrap().push(old.clone());
    store
        .append_claim(&ClaimInput {
            subject: desired.subject.clone(),
            kind: "runtime.observed".into(),
            actor: None,
            fields: member_fields(member, "stopped", Some("prior-incarnation"), false),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    // Capture the old physical generation, then pause its publication until the
    // replacement worker and its owner completion have both finished.
    let capture_fence = store.capture_runtime_observations().unwrap();
    let captured = runtime.snapshot_ptys().unwrap().remove(0);
    runtime.inner.ptys.lock().unwrap().clear();
    assert!(launch(&reconciler, &desired.subject));
    tokio::time::timeout(Duration::from_secs(30), entered.recv())
        .await
        .unwrap()
        .unwrap();
    release.0.send(()).unwrap();
    workers_done(&reconciler).await;
    reconciler.reconcile_start_jobs().unwrap();
    let completed = store
        .latest_claim(&desired.subject, Some("runtime.observed"))
        .unwrap()
        .unwrap();
    assert_eq!(
        completed.body["fields"]["incarnation_id"],
        "blocked-start-incarnation"
    );
    let prior_target: String = store.readers.get().query_row(
        "SELECT json_extract(body,'$.expected_incarnation') FROM local_runtime_starts WHERE subject=?1",
        [&desired.subject], |row| row.get(0)).unwrap();
    assert_eq!(prior_target, "prior-incarnation");
    reconciler
        .record_member(&desired, &captured, false)
        .unwrap();
    assert_eq!(
        store
            .latest_claim(&desired.subject, Some("runtime.observed"))
            .unwrap()
            .unwrap()
            .id,
        completed.id
    );
    reconciler
        .record_once(
            &desired.subject,
            "runtime.observed",
            member_fields(member, "absent", None, false),
        )
        .unwrap();
    assert_eq!(
        store
            .latest_claim(&desired.subject, Some("runtime.observed"))
            .unwrap()
            .unwrap()
            .id,
        completed.id
    );
    assert_eq!(
        reconciler.runtime_observation_drops.lock().unwrap()[&desired.subject],
        2
    );
    let fault_subject = format!("daemon/{}", reconciler.host);
    let fault_scope = format!("runtime-observation:{}", desired.subject);
    assert!(
        !store
            .observations_for(&fault_subject, "reconcile.fault")
            .unwrap()
            .iter()
            .any(|claim| claim.body["fields"]["scope"] == fault_scope.as_str())
    );
    reconciler
        .record_member(&desired, &captured, false)
        .unwrap();
    let faults = store
        .observations_for(&fault_subject, "reconcile.fault")
        .unwrap();
    assert_eq!(
        faults
            .iter()
            .filter(
                |claim| claim.body["fields"]["scope"] == fault_scope.as_str()
                    && claim.body["fields"]["status"] == "faulted"
            )
            .count(),
        1
    );
    assert_eq!(
        store
            .latest_claim(&desired.subject, Some("runtime.observed"))
            .unwrap()
            .unwrap()
            .id,
        completed.id
    );
    let wake = *reconciler.event_notify.borrow();
    reconciler
        .record_member(&desired, &captured, false)
        .unwrap();
    assert_eq!(
        *reconciler.event_notify.borrow(),
        wake,
        "an already faulted unchanged refusal must not spin the reactive loop"
    );
    assert_eq!(
        reconciler.runtime_observation_drops.lock().unwrap()[&desired.subject],
        3
    );
    drop(capture_fence);
    let _fresh_capture = store.capture_runtime_observations().unwrap();
    let fresh = runtime.snapshot_ptys().unwrap().remove(0);
    reconciler.record_member(&desired, &fresh, false).unwrap();
    let current = store
        .latest_claim(&desired.subject, Some("runtime.observed"))
        .unwrap()
        .unwrap();
    assert_eq!(current.body["fields"]["status"], "running");
    assert_eq!(
        current.body["fields"]["incarnation_id"],
        "blocked-start-incarnation"
    );
    assert!(
        !reconciler
            .runtime_observation_drops
            .lock()
            .unwrap()
            .contains_key(&desired.subject)
    );
    assert!(
        store
            .observations_for(&fault_subject, "reconcile.fault")
            .unwrap()
            .iter()
            .any(
                |claim| claim.body["fields"]["scope"] == fault_scope.as_str()
                    && claim.body["fields"]["status"] == "recovered"
            )
    );
    // A fresh native PTY exit can lack a PID/incarnation in its registry row.
    // It may describe absence after this unchanged capture; absence itself is
    // still never an operation-ending or replacement proof for the consumer.
    let ended = RuntimeObservation {
        status: "exited".into(),
        incarnation_id: None,
        ..fresh
    };
    reconciler.record_member(&desired, &ended, false).unwrap();
    let absent = store
        .latest_claim(&desired.subject, Some("runtime.observed"))
        .unwrap()
        .unwrap();
    assert_eq!(absent.body["fields"]["status"], "exited");
    let actual = store
        .latest_actual_value(&desired.subject)
        .unwrap()
        .unwrap();
    assert!(actual_field(&actual, "incarnation_id").unwrap().is_null());
    assert!(store.runtime_start_for(&desired.subject).unwrap().is_none());
}

#[tokio::test]
async fn operator_stop_preempts_a_blocked_start_in_the_real_loop() {
    let directory = tempfile::tempdir().unwrap();
    // Use a file store, like the daemon, so publishing beside a pass needs no retries.
    let store = Arc::new(Store::open(&directory.path().join("claims.db"), "node").unwrap());
    apply_source(
        &store,
        r#"version 2
agent "example/worker" { workspace "/tmp"; command "true" }
"#,
        "barrier-start",
    );
    let (entered, mut entered_rx) = mpsc::unbounded_channel();
    let (release, held) = std::sync::mpsc::channel();
    let release = ReleaseOnDrop(release);
    let (stopped, mut stopped_rx) = mpsc::unbounded_channel();
    let (started, mut started_rx) = mpsc::unbounded_channel();
    let (probed, mut probed_rx) = mpsc::unbounded_channel();
    let runtime = Arc::new(BarrierRuntime {
        inner: FakeRuntime::default(),
        store: store.clone(),
        entered,
        release: Mutex::new(held),
        stopped,
        publish_early: true,
        honor_guard: true,
        operations: Mutex::default(),
        started,
        reject_gates: AtomicBool::new(false),
        verification_hidden: AtomicBool::new(false),
        provenance_error: AtomicBool::new(false),
        ending_unverified: AtomicBool::new(false),
    });
    let notify = Arc::new(Notify::new());
    let reconciler = Arc::new(
        Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            notify.clone(),
        )
        .with_disk_probe(
            vec![PathBuf::from("/state")],
            Arc::new(move |_| {
                let _ = probed.send(());
                Ok(crate::disk::DiskSpace {
                    filesystem: 7,
                    available: 100 << 30,
                    total: 200 << 30,
                })
            }),
        ),
    );
    let task = tokio::spawn(reconciler.clone().run());
    tokio::time::timeout(Duration::from_secs(30), entered_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(runtime.inner.stops.lock().unwrap().is_empty());
    while probed_rx.try_recv().is_ok() {}
    apply_source(
        &store,
        "version 2\nstop \"agent/example/worker\"\nagent \"example/unrelated\" { workspace \"/tmp\"; command \"true\" }",
        "barrier-stop",
    );
    let selected = store
        .selected_desired_token("agent/example/worker")
        .unwrap()
        .unwrap();
    assert_eq!(
        store.claim_by_id(&selected).unwrap().unwrap().body["kind"],
        "stop"
    );
    notify.notify_one();
    // A watchdog bounds a failing test; it neither releases nor cancels the start.
    let before_release = tokio::time::timeout(Duration::from_secs(10), stopped_rx.recv()).await;
    let unrelated_before_release =
        tokio::time::timeout(Duration::from_secs(10), started_rx.recv()).await;
    let safety_before_release =
        tokio::time::timeout(Duration::from_secs(10), probed_rx.recv()).await;
    let physical_stops_before_release = runtime.inner.stops.lock().unwrap().len();
    // Always release before asserting, including on the old broken path, so the test
    // cannot strand a blocking worker during Tokio shutdown.
    release.0.send(()).unwrap();
    let incarnation = match before_release {
        Ok(Some(incarnation)) => incarnation,
        _ => tokio::time::timeout(Duration::from_secs(30), stopped_rx.recv())
            .await
            .unwrap()
            .unwrap(),
    };
    workers_done(&reconciler).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(incarnation.as_deref(), Some("blocked-start-incarnation"));
    assert_eq!(
        unrelated_before_release.unwrap().unwrap(),
        "example.unrelated"
    );
    safety_before_release.unwrap().unwrap();
    assert_eq!(
        physical_stops_before_release, 1,
        "the Store committed a stop, but the physical stop waited for the start barrier"
    );
}

#[tokio::test]
async fn unpublished_start_is_not_a_settled_stop_and_revoked_completion_is_suppressed() {
    let (_directory, store, runtime, reconciler, release, mut entered) = fixture(false, true);
    apply_source(
        &store,
        "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"true\" }",
        "unpublished",
    );
    assert!(launch(&reconciler, "agent/example/worker"));
    tokio::time::timeout(Duration::from_secs(30), entered.recv())
        .await
        .unwrap()
        .unwrap();
    apply_source(
        &store,
        "version 2\nstop \"agent/example/worker\"",
        "unpublished-stop",
    );
    reconciler.reconcile_once().unwrap();
    let operation = store
        .runtime_start_for("agent/example/worker")
        .unwrap()
        .unwrap();
    assert!(operation.revoked_by.is_some() && !operation.finished);
    assert!(
        !reconciler
            .stop_is_settled(&store.desired_subjects().unwrap().remove(0))
            .unwrap()
    );
    assert_eq!(store.runtime_start_counts().unwrap(), (1, 1));
    release.0.send(()).unwrap();
    workers_done(&reconciler).await;
    reconciler.reconcile_once().unwrap();
    assert!(runtime.inner.starts.lock().unwrap().is_empty());
    assert!(
        store
            .observations_for("agent/example/worker", "runtime.action.succeeded")
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.runtime_start_counts().unwrap(), (0, 0));
    assert!(
        store
            .runtime_start_for("agent/example/worker")
            .unwrap()
            .is_some(),
        "backend rejection alone cannot prove detached producer closure"
    );
}

#[tokio::test]
async fn revoked_jobs_hold_capacity_and_completion_admits_a_queued_seat() {
    let (_directory, store, _runtime, reconciler, release, mut entered) = fixture(false, true);
    apply_source(
        &store,
        "version 2\nagent \"example/held1\" { workspace \"/tmp\"; command \"true\" }\nagent \"example/held2\" { workspace \"/tmp\"; command \"true\" }\nagent \"example/queued\" { workspace \"/tmp\"; command \"true\" }",
        "cap",
    );
    let started_at = std::time::Instant::now();
    let threads_before = std::fs::read_dir("/proc/self/task")
        .ok()
        .map(|entries| entries.count());
    assert!(launch(&reconciler, "agent/example/held1"));
    assert!(launch(&reconciler, "agent/example/held2"));
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(30), entered.recv())
            .await
            .unwrap()
            .unwrap();
    }
    let threads_held = std::fs::read_dir("/proc/self/task")
        .ok()
        .map(|entries| entries.count());
    eprintln!(
        "cap=2: two launch admissions reached held backends in {:?}; process threads before={threads_before:?}, held={threads_held:?}",
        started_at.elapsed()
    );
    assert!(!launch(&reconciler, "agent/example/held1"));
    assert!(launch(&reconciler, "agent/example/queued"));
    assert!(
        !store
            .runtime_start_for("agent/example/queued")
            .unwrap()
            .unwrap()
            .dispatched
    );
    reconciler.revoke_start("agent/example/held1").unwrap();
    reconciler.revoke_start("agent/example/held2").unwrap();
    assert_eq!(store.runtime_start_counts().unwrap(), (2, 2));
    reconciler.reconcile_start_jobs().unwrap();
    assert!(
        !store
            .runtime_start_for("agent/example/queued")
            .unwrap()
            .unwrap()
            .dispatched
    );
    release.0.send(()).unwrap();
    release.0.send(()).unwrap();
    workers_done(&reconciler).await;
    reconciler.reconcile_start_jobs().unwrap();
    workers_done(&reconciler).await;
    reconciler.reconcile_start_jobs().unwrap();
    assert!(
        store
            .runtime_start_for("agent/example/queued")
            .unwrap()
            .is_none(),
        "queued completion faults: {:?}",
        reconciler.raised_faults.lock().unwrap()
    );
    assert_eq!(store.runtime_start_counts().unwrap(), (0, 0));
}

#[tokio::test]
async fn a_late_revoked_runtime_is_stopped_by_provenance_and_a_replacement_survives() {
    let (_directory, store, runtime, reconciler, release, mut entered) = fixture(false, false);
    apply_source(
        &store,
        "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"true\" }",
        "late",
    );
    assert!(launch(&reconciler, "agent/example/worker"));
    tokio::time::timeout(Duration::from_secs(30), entered.recv())
        .await
        .unwrap()
        .unwrap();
    reconciler.revoke_start("agent/example/worker").unwrap();
    release.0.send(()).unwrap();
    workers_done(&reconciler).await;
    reconciler.reconcile_start_jobs().unwrap();
    assert_eq!(*runtime.inner.stops.lock().unwrap(), ["example.worker"]);
    assert!(
        store
            .observations_for("agent/example/worker", "runtime.action.succeeded")
            .unwrap()
            .is_empty()
    );
    runtime.inner.ptys.lock().unwrap()[0].incarnation_id = Some("replacement".into());
    reconciler.reconcile_start_jobs().unwrap();
    assert_eq!(runtime.inner.stops.lock().unwrap().len(), 1);
    assert!(
        store
            .runtime_start_for("agent/example/worker")
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn an_owner_restart_never_replays_a_pending_launch_and_controls_bypass_its_lock() {
    let (_directory, store, runtime, reconciler, release, mut entered) = fixture(true, true);
    apply_source(
        &store,
        "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"true\" }",
        "owner",
    );
    assert!(launch(&reconciler, "agent/example/worker"));
    tokio::time::timeout(Duration::from_secs(30), entered.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .try_runtime_start_lock("agent/example/worker")
            .unwrap()
            .is_none()
    );
    let next = Arc::new(Reconciler::new(
        store.clone(),
        runtime.clone(),
        "node".into(),
        Arc::new(Notify::new()),
    ));
    next.wait_for_test_starts.store(false, Ordering::Release);
    next.reconcile_start_jobs().unwrap();
    assert!(!launch(&next, "agent/example/worker"));
    apply_source(
        &store,
        "version 2\nstop \"agent/example/worker\"",
        "owner-stop",
    );
    next.reconcile_once().unwrap();
    assert!(!runtime.inner.stops.lock().unwrap().is_empty());
    release.0.send(()).unwrap();
    workers_done(&reconciler).await;
    next.reconcile_start_jobs().unwrap();
    assert!(
        store
            .try_runtime_start_lock("agent/example/worker")
            .unwrap()
            .is_some()
    );
    assert!(runtime.inner.starts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn account_limit_enforcement_physically_stops_a_blocked_start() {
    let (_directory, store, runtime, reconciler, release, mut entered) = fixture(true, true);
    apply_source(
        &store,
        "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"true\" }",
        "limit",
    );
    assert!(launch(&reconciler, "agent/example/worker"));
    tokio::time::timeout(Duration::from_secs(30), entered.recv())
        .await
        .unwrap()
        .unwrap();
    let now = now_ms();
    store
        .append_claim(&ClaimInput {
            subject: "agent/example/worker".into(),
            kind: "harness.limits".into(),
            actor: Some("agent/example/worker".into()),
            fields: BTreeMap::from([
                ("driver".into(), Value::from("claude")),
                ("account".into(), Value::from("claude/example")),
                (
                    "incarnation_id".into(),
                    Value::from("blocked-start-incarnation"),
                ),
                ("weekly_percent".into(), Value::from(96)),
                ("five_hour_percent".into(), Value::from(40)),
                ("measured_at_unix_ms".into(), Value::from(now as u64)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let policy = crate::store::LimitsPolicy {
        stop_at_weekly_percent: 95,
        keep: BTreeSet::new(),
        exempt_accounts: BTreeSet::new(),
        exempt_harnesses: BTreeSet::new(),
        notify: "agent/example/operations".into(),
        fresh_ms: 3_600_000,
    };
    let outcome = store.enforce_account_limits(&policy, now).unwrap();
    assert_eq!(outcome.stopped, ["agent/example/worker"]);
    reconciler.reconcile_once().unwrap();
    assert_eq!(*runtime.inner.stops.lock().unwrap(), ["example.worker"]);
    assert_eq!(store.runtime_start_counts().unwrap(), (1, 1));
    release.0.send(()).unwrap();
    workers_done(&reconciler).await;
    assert!(runtime.inner.starts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn operator_stop_reaches_its_incarnation_fenced_kill_while_start_is_blocked() {
    let (_directory, store, runtime, reconciler, release, mut entered) = fixture(true, true);
    apply_source(
        &store,
        "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"true\"; shutdown-timeout \"1ms\" }",
        "kill-start",
    );
    assert!(launch(&reconciler, "agent/example/worker"));
    tokio::time::timeout(Duration::from_secs(30), entered.recv())
        .await
        .unwrap()
        .unwrap();
    apply_source(
        &store,
        "version 2\nstop \"agent/example/worker\"",
        "kill-stop",
    );
    reconciler.reconcile_once().unwrap();
    // Let the authored shutdown grace elapse without releasing the launch barrier.
    tokio::time::sleep(Duration::from_millis(2)).await;
    reconciler.reconcile_once().unwrap();
    assert_eq!(*runtime.inner.kills.lock().unwrap(), ["example.worker"]);
    assert_eq!(store.runtime_start_counts().unwrap(), (1, 1));
    release.0.send(()).unwrap();
    workers_done(&reconciler).await;
    assert!(runtime.inner.starts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn asynchronous_gate_start_failures_are_broken_checks_without_relaunch() {
    let (_directory, _store, runtime, reconciler, _release, _entered) = fixture(false, true);
    runtime.reject_gates.store(true, Ordering::Release);
    let stage = GateContext {
        subject: "step-run/example/verify".into(),
        name: "verify".into(),
        started_at_unix_ms: now_ms(),
        attempt: 1,
        run: String::new(),
        generation: String::new(),
        eval: false,
    };
    let environment = BTreeMap::new();
    let mechanical = || {
        reconciler
            .run_mechanical(
                &stage,
                "mechanical",
                "true",
                "node",
                "/tmp",
                &environment,
                60_000,
            )
            .unwrap()
    };
    assert!(matches!(mechanical(), GateOutcome::Pending));
    workers_done(&reconciler).await;
    reconciler.reconcile_start_jobs().unwrap();
    assert!(matches!(mechanical(), GateOutcome::Broken(_)));
    assert!(matches!(mechanical(), GateOutcome::Broken(_)));
    assert_eq!(runtime.inner.starts.lock().unwrap().len(), 1);
    let llm = || {
        reconciler
            .run_llm_gate(
                &stage,
                "llm",
                "example-model",
                "node",
                "/tmp",
                &[],
                &environment,
                1_000,
                60_000,
                "Check the result.",
            )
            .unwrap()
    };
    assert!(matches!(llm(), GateOutcome::Pending));
    workers_done(&reconciler).await;
    reconciler.reconcile_start_jobs().unwrap();
    assert!(matches!(llm(), GateOutcome::Broken(_)));
    assert!(matches!(llm(), GateOutcome::Broken(_)));
    assert_eq!(runtime.inner.starts.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn accepted_but_unverified_starts_retain_admission_until_exact_verification_or_retirement() {
    for revoke in [false, true] {
        let (_directory, store, runtime, reconciler, release, mut entered) = fixture(true, false);
        apply_source(
            &store,
            "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"true\" }",
            "verification-start",
        );
        runtime.verification_hidden.store(true, Ordering::Release);
        assert!(launch(&reconciler, "agent/example/worker"));
        tokio::time::timeout(Duration::from_secs(30), entered.recv())
            .await
            .unwrap()
            .unwrap();
        release.0.send(()).unwrap();
        workers_done(&reconciler).await;
        reconciler.reconcile_start_jobs().unwrap();
        let operation = store
            .runtime_start_for("agent/example/worker")
            .unwrap()
            .unwrap();
        assert!(operation.finished && !operation.applied && !operation.receipted);
        assert_eq!(store.unresolved_runtime_start_counts().unwrap(), (1, 0));
        assert!(!launch(&reconciler, "agent/example/worker"));
        assert!(
            store
                .observations_for("agent/example/worker", "runtime.action.succeeded")
                .unwrap()
                .is_empty()
        );
        if revoke {
            reconciler.revoke_start("agent/example/worker").unwrap();
        }
        runtime.verification_hidden.store(false, Ordering::Release);
        runtime
            .operations
            .lock()
            .unwrap()
            .insert("example.worker".into(), "foreign-operation".into());
        reconciler.reconcile_start_jobs().unwrap();
        assert!(
            store
                .runtime_start_for("agent/example/worker")
                .unwrap()
                .is_some()
        );
        assert!(runtime.inner.stops.lock().unwrap().is_empty());
        runtime
            .operations
            .lock()
            .unwrap()
            .insert("example.worker".into(), operation.id.clone());
        runtime.provenance_error.store(true, Ordering::Release);
        reconciler.reconcile_start_jobs().unwrap();
        assert!(
            store
                .runtime_start_for("agent/example/worker")
                .unwrap()
                .is_some()
        );
        runtime.provenance_error.store(false, Ordering::Release);
        reconciler.reconcile_start_jobs().unwrap();
        if revoke {
            assert_eq!(*runtime.inner.stops.lock().unwrap(), ["example.worker"]);
            runtime.inner.ptys.lock().unwrap()[0].status = "exited".into();
            reconciler.reconcile_start_jobs().unwrap();
            assert!(
                store
                    .observations_for("agent/example/worker", "runtime.action.succeeded")
                    .unwrap()
                    .is_empty()
            );
        } else {
            let succeeded = store
                .observations_for("agent/example/worker", "runtime.action.succeeded")
                .unwrap();
            assert_eq!(succeeded.len(), 1);
            assert_eq!(
                succeeded[0].body["fields"]["incarnation_id"],
                "blocked-start-incarnation"
            );
            assert!(
                succeeded[0].body["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|evidence| evidence == &operation.body["start_request"])
            );
        }
        assert!(
            store
                .runtime_start_for("agent/example/worker")
                .unwrap()
                .is_none()
        );
        assert_eq!(store.unresolved_runtime_start_counts().unwrap(), (0, 0));
    }
}

#[tokio::test]
async fn ended_pty_without_complete_retirement_proof_keeps_the_seat_busy() {
    let (_directory, store, runtime, reconciler, release, mut entered) = fixture(true, false);
    apply_source(
        &store,
        "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"true\" }",
        "ended-unverified-start",
    );
    assert!(launch(&reconciler, "agent/example/worker"));
    tokio::time::timeout(Duration::from_secs(30), entered.recv())
        .await
        .unwrap()
        .unwrap();
    release.0.send(()).unwrap();
    workers_done(&reconciler).await;
    // The daemon exited, but its work scope cannot be proven empty. The runtime
    // accepts a stop signal; that asynchronous acceptance is still no proof.
    runtime.ending_unverified.store(true, Ordering::Release);
    runtime.inner.ptys.lock().unwrap()[0].status = "exited".into();
    reconciler.revoke_start("agent/example/worker").unwrap();
    reconciler.reconcile_start_jobs().unwrap();
    let operation = store
        .runtime_start_for("agent/example/worker")
        .unwrap()
        .unwrap();
    assert!(!operation.cleanup_verified);
    assert!(operation.result.as_ref().unwrap()["closure_proof"].is_null());
    assert_eq!(*runtime.inner.stops.lock().unwrap(), ["example.worker"]);
    assert!(!launch(&reconciler, "agent/example/worker"));
    // Only the fixture's later positive exact-operation closure permits release.
    runtime.ending_unverified.store(false, Ordering::Release);
    reconciler.reconcile_start_jobs().unwrap();
    assert!(
        store
            .runtime_start_for("agent/example/worker")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn missing_or_repaired_original_request_prevents_real_native_start() {
    if crate::test_support::supervise_test() {
        return;
    }
    for change in ["deleted", "changed"] {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&root.path().join("claims.db"), "node").unwrap());
        let runtime = Arc::new(NativeRuntime::new(
            root.path(),
            None,
            Path::new("unused-pty"),
        ));
        let reconciler = Arc::new(Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        ));
        let marker = root.path().join("launched");
        apply_source(
            &store,
            &format!(
                "version 2\nexec \"example/guarded\" {{ workspace {:?}; argv \"/bin/sh\" \"-c\" {:?}; }}",
                root.path().display().to_string(),
                format!("touch {:?}", marker)
            ),
            "guarded-native",
        );
        let desired = store
            .desired_subjects_named(&["exec/example/guarded".into()])
            .unwrap()
            .remove(0);
        let mut member = desired.member.as_ref().unwrap().clone();
        let id = uuid::Uuid::now_v7().to_string();
        member
            .tags
            .insert(start_jobs::OPERATION_TAG.into(), id.clone());
        member
            .environment
            .insert(start_jobs::OPERATION_ENV.into(), id.clone());
        let prepared = start_jobs::PreparedStart {
            subject: desired.clone(),
            launch_member: member.clone(),
            member,
            reason: "request retention regression".into(),
            request: None,
            desired_token: reconciler.launch_token(&desired.subject).unwrap(),
            selected_token: store.selected_desired_token(&desired.subject).unwrap(),
            placement_evidence: Vec::new(),
            start_request: String::new(),
            start_request_receipt: None,
            control_request: None,
            control_request_receipt: None,
            expected_incarnation: None,
            resume_operation: None,
        };
        let mut operation = crate::store::RuntimeStartOperation {
            id: id.clone(),
            subject: desired.subject.clone(),
            owner: "test-owner".into(),
            body: serde_json::to_value(prepared).unwrap(),
            revoked_by: None,
            finished: false,
            result: None,
            applied: false,
            cleanup_verified: false,
            dispatched: false,
            receipted: false,
        };
        store.admit_runtime_start(&mut operation, 2).unwrap();
        let request: crate::model::ClaimRecord =
            serde_json::from_value(operation.body["start_request_receipt"].clone()).unwrap();
        assert!(store.runtime_start_request_retained(&request).unwrap());
        let connection = rusqlite::Connection::open(root.path().join("claims.db")).unwrap();
        let local_id: i64 = request.id.rsplit('/').next().unwrap().parse().unwrap();
        if change == "deleted" {
            connection
                .execute("DELETE FROM local_observations WHERE id=?1", [local_id])
                .unwrap();
        } else {
            connection.execute("UPDATE local_observations SET body=json_set(body,'$.fields.runtime_id','foreign-runtime') WHERE id=?1", [local_id]).unwrap();
        }
        assert!(!store.runtime_start_request_retained(&request).unwrap());
        reconciler.start_queued_job(&operation).unwrap();
        workers_done(&reconciler).await;
        reconciler.reconcile_start_jobs().unwrap();
        assert!(!marker.exists(), "{change} request reached native creation");
        assert!(
            runtime
                .observe_exec(&desired.member.as_ref().unwrap().runtime_id)
                .unwrap()
                .is_none()
        );
        assert!(store.runtime_start_for(&desired.subject).unwrap().is_none());
        let result: String = store
            .readers
            .get()
            .query_row(
                "SELECT result FROM local_runtime_starts WHERE id=?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        let result: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["closure_proof"]["verification"], "producer-closed");
        assert_eq!(
            result["closure_proof"]["start_request_receipt"]["id"],
            request.id
        );
        assert!(
            store
                .observations_for(&desired.subject, "runtime.action.succeeded")
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn native_rejected_starts_prove_closure_and_allow_a_later_start_for_the_same_seat() {
    if crate::test_support::supervise_test() {
        return;
    }
    for failure in ["missing-program", "missing-cwd"] {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&root.path().join("graph.db"), "node").unwrap());
        let runtime = Arc::new(NativeRuntime::new(
            root.path(),
            None,
            Path::new("unused-pty"),
        ));
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        // Exercise NativeRuntime's producer boundary directly through the durable jobs.
        // The ordinary exec member preparation wraps its command in the st3 driver;
        // a missing command inside that wrapper is a later readiness failure.
        let dispatch = |desired: &DesiredSubject| {
            let member = desired.member.as_ref().unwrap().clone();
            reconciler.dispatch_start(start_jobs::PreparedStart {
                subject: desired.clone(),
                launch_member: member.clone(),
                member,
                reason: "native producer closure regression".into(),
                request: None,
                desired_token: reconciler.launch_token(&desired.subject).unwrap(),
                selected_token: store.selected_desired_token(&desired.subject).unwrap(),
                placement_evidence: Vec::new(),
                start_request: String::new(),
                start_request_receipt: None,
                control_request: None,
                control_request_receipt: None,
                expected_incarnation: None,
                resume_operation: None,
            })
        };
        let cwd = if failure == "missing-cwd" {
            root.path().join("missing-cwd")
        } else {
            root.path().to_owned()
        };
        let program = if failure == "missing-program" {
            format!("missing-native-program-{}", uuid::Uuid::new_v4())
        } else {
            sleep_program()
        };
        apply_source(
            &store,
            &format!(
                "version 2\nexec \"example/rejected\" {{ workspace {:?}; argv {:?} \"1000\"; }}",
                cwd.display().to_string(),
                program
            ),
            "rejected-native",
        );
        let desired = store
            .desired_subjects_named(&["exec/example/rejected".into()])
            .unwrap()
            .remove(0);
        let rejected = dispatch(&desired);
        assert!(rejected.is_err(), "{failure}: {rejected:?}");
        assert!(store.runtime_start_for(&desired.subject).unwrap().is_none());
        let result: String = store
            .readers
            .get()
            .query_row(
                "SELECT result FROM local_runtime_starts WHERE subject=?1 ORDER BY id DESC LIMIT 1",
                [&desired.subject],
                |row| row.get(0),
            )
            .unwrap();
        let result: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["producer_closed"], true);
        let failed = store
            .observations_for(&desired.subject, "runtime.action.failed")
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(failed.body["fields"]["operation_status"], "producer-closed");
        assert_eq!(failed.body["fields"]["operation"], result["operation_id"]);
        assert_eq!(
            failed.body["fields"]["runtime_id"],
            desired.member.as_ref().unwrap().runtime_id
        );
        assert_eq!(failed.body["fields"]["terminal"], false);
        assert!(
            failed.body["evidence"]
                .as_array()
                .unwrap()
                .contains(&result["start_request"])
        );
        apply_source(
            &store,
            &format!(
                "version 2\nexec \"example/rejected\" {{ workspace {:?}; argv {:?} \"1000\"; }}",
                root.path().display().to_string(),
                sleep_program()
            ),
            "valid-native",
        );
        let desired = store
            .desired_subjects_named(&["exec/example/rejected".into()])
            .unwrap()
            .remove(0);
        assert!(dispatch(&desired).unwrap());
        assert!(store.runtime_start_for(&desired.subject).unwrap().is_none());
        let observation = runtime
            .observe_exec(&desired.member.as_ref().unwrap().runtime_id)
            .unwrap()
            .unwrap();
        let completed = store
            .observations_for(&desired.subject, "runtime.action.succeeded")
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(completed.body["fields"]["operation_status"], "verified");
        assert_eq!(
            completed.body["fields"]["incarnation_id"].as_str(),
            observation.incarnation_id.as_deref()
        );
        assert_eq!(completed.body["fields"]["terminal"], false);
        let payload: (u64,u64,u64) = store.readers.get().query_row(
            "SELECT length(CAST(body AS BLOB)),length(CAST(result AS BLOB)),                length(CAST(json_extract(result,'$.runtime_observation') AS BLOB))              FROM local_runtime_starts WHERE id=?1",
            [completed.body["fields"]["operation"].as_str().unwrap()],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
        eprintln!(
            "native exec fixture ledger bytes: prepared={}, result={}, retained runtime observation={}; total={}",
            payload.0,
            payload.1,
            payload.2,
            payload.0 + payload.1
        );
        runtime
            .kill(
                &observation.runtime_id,
                false,
                observation.incarnation_id.as_deref(),
            )
            .unwrap();
    }
}

#[test]
fn native_rollout_guard_rejection_proves_no_producer_was_created() {
    if crate::test_support::supervise_test() {
        return;
    }
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let called = root.path().join("producer-called");
    let pty = root.path().join("pty");
    std::fs::write(
        &pty,
        format!("#!/bin/sh\nprintf called > {:?}\nexit 99\n", called),
    )
    .unwrap();
    std::fs::set_permissions(&pty, std::fs::Permissions::from_mode(0o700)).unwrap();
    let intent = parse_intent(&format!("version 2\nagent \"example/guard\" {{ workspace {:?}; argv {:?} \"1000\"; }}", root.path().display().to_string(), sleep_program()), "node").unwrap();
    let mut member = intent
        .subjects
        .values()
        .find_map(|subject| subject.member.clone())
        .unwrap();
    member.environment.insert(
        "PATH".into(),
        format!("{}:${{PATH}}", root.path().display()),
    );
    member
        .tags
        .insert(start_jobs::OPERATION_TAG.into(), "guard-operation".into());
    member
        .environment
        .insert(start_jobs::OPERATION_ENV.into(), "guard-operation".into());
    member.environment.insert(
        crate::rollout::OPERATION_ENV.into(),
        "rollout-operation".into(),
    );
    member.environment.insert(
        crate::rollout::PREDECESSOR_ENV.into(),
        "old:incarnation".into(),
    );
    let state = root.path().join("state");
    std::fs::create_dir(&state).unwrap();
    let runtime = NativeRuntime::new(&state, None, &pty);
    let guard_called = AtomicBool::new(false);
    let error = runtime
        .start_guarded(&member, &|| {
            guard_called.store(true, Ordering::Release);
            anyhow::bail!("declaration changed before launch")
        })
        .unwrap_err();
    assert!(guard_called.load(Ordering::Acquire));
    assert_eq!(
        error
            .downcast_ref::<st_runtime::StartRejected>()
            .unwrap()
            .operation
            .as_deref(),
        Some("guard-operation")
    );
    assert!(
        !called.exists(),
        "a rejected guard reached the producer binary"
    );
}

#[test]
fn native_pty_publication_guard_after_a_launcher_does_not_prove_producer_closure() {
    if crate::test_support::supervise_test() {
        return;
    }
    use std::os::unix::fs::PermissionsExt as _;
    st_runtime::initialize_isolation(&BTreeMap::new());
    let root = tempfile::tempdir().unwrap();
    let pty = root.path().join("pty");
    let called = root.path().join("called");
    std::fs::write(
        &pty,
        format!("#!/bin/sh\nprintf called > {called:?}\nprintf 'already in use' >&2\nexit 1\n"),
    )
    .unwrap();
    std::fs::set_permissions(&pty, std::fs::Permissions::from_mode(0o700)).unwrap();
    let intent = parse_intent(&format!("version 2\nagent \"example/late-guard\" {{ workspace {:?}; argv {:?} \"1000\"; }}", root.path().display().to_string(), sleep_program()), "node").unwrap();
    let mut member = intent
        .subjects
        .values()
        .find_map(|subject| subject.member.clone())
        .unwrap();
    member.environment.insert(
        "PATH".into(),
        format!("{}:${{PATH}}", root.path().display()),
    );
    member.tags.insert(
        start_jobs::OPERATION_TAG.into(),
        "late-guard-operation".into(),
    );
    member.environment.insert(
        start_jobs::OPERATION_ENV.into(),
        "late-guard-operation".into(),
    );
    let state = root.path().join("state");
    std::fs::create_dir(&state).unwrap();
    let runtime = NativeRuntime::new(&state, None, &pty);
    let guards = std::sync::atomic::AtomicUsize::new(0);
    let error = runtime
        .start_guarded(&member, &|| {
            guards.fetch_add(1, Ordering::Relaxed);
            anyhow::ensure!(
                !called.exists(),
                "declaration changed after the first launcher"
            );
            Ok(())
        })
        .unwrap_err();
    assert!(called.exists());
    assert_eq!(guards.load(Ordering::Relaxed), 2);
    assert!(error.to_string().contains("declaration changed"));
    assert!(
        error.downcast_ref::<st_runtime::StartRejected>().is_none(),
        "the earlier physical launcher remains potentially live: {error:#}"
    );
}

#[test]
fn native_pty_launcher_rejection_is_distinct_from_a_returned_producer_error() {
    if crate::test_support::supervise_test() {
        return;
    }
    use std::os::unix::fs::PermissionsExt as _;
    // Select the supported detached mode in this isolated subprocess. A systemd
    // launcher is itself a created producer, even when its inner executable fails.
    st_runtime::initialize_isolation(&BTreeMap::new());
    for (binary, rejected) in [
        ("invalid executable format", true),
        ("#!/bin/sh\nexit 1\n", false),
        (
            "#!/bin/sh\nrm -- \"$0\"\nprintf 'already in use' >&2\nexit 1\n",
            false,
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let pty = root.path().join("pty");
        // Linux refuses a file with no shebang (ENOEXEC). macOS's spawn instead hands such a file
        // to /bin/sh, which runs the text and so creates a launcher; there the file the kernel
        // itself refuses is a fat Mach-O header with nothing behind it.
        let contents: &[u8] = if cfg!(target_os = "macos") && binary == "invalid executable format"
        {
            b"\xca\xfe\xba\xbe invalid executable format"
        } else {
            binary.as_bytes()
        };
        std::fs::write(&pty, contents).unwrap();
        std::fs::set_permissions(&pty, std::fs::Permissions::from_mode(0o700)).unwrap();
        let intent = parse_intent(&format!("version 2\nagent \"example/rejected-pty\" {{ workspace {:?}; argv {:?} \"1000\"; }}", root.path().display().to_string(), sleep_program()), "node").unwrap();
        let mut member = intent
            .subjects
            .values()
            .find_map(|subject| subject.member.clone())
            .unwrap();
        member.environment.insert(
            "PATH".into(),
            format!("{}:${{PATH}}", root.path().display()),
        );
        member
            .tags
            .insert(start_jobs::OPERATION_TAG.into(), "pty-operation".into());
        member
            .environment
            .insert(start_jobs::OPERATION_ENV.into(), "pty-operation".into());
        let state = root.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let runtime = NativeRuntime::new(&state, None, &pty);
        let error = runtime.start_guarded(&member, &|| Ok(())).unwrap_err();
        assert_eq!(
            error.downcast_ref::<st_runtime::StartRejected>().is_some(),
            rejected,
            "{error:#}"
        );
        if rejected {
            assert_eq!(
                error
                    .downcast_ref::<st_runtime::StartRejected>()
                    .unwrap()
                    .operation
                    .as_deref(),
                Some("pty-operation")
            );
        }
        assert!(
            !runtime.start_operation_closed(&member).unwrap(),
            "a returned old-producer error is not closure proof"
        );
    }
}

/// A real CLI producer returns failure while its detached child is held before
/// publication. The FIFO and socket order effects; no observation polling is needed.
struct NativeLatePty {
    root: tempfile::TempDir,
    store: Arc<Store>,
    runtime: Arc<NativeRuntime>,
    reconciler: Reconciler<NativeRuntime>,
    desired: DesiredSubject,
    real_pty: std::path::PathBuf,
    gate: std::fs::File,
    ready: tokio::net::UnixListener,
}

impl NativeLatePty {
    fn new() -> Self {
        use std::os::unix::{ffi::OsStrExt as _, fs::PermissionsExt as _};
        st_runtime::initialize_isolation(&BTreeMap::new());
        let real_pty =
            st_runtime::resolve_executable("pty", &crate::environment::snapshot().unwrap())
                .unwrap();
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let pty = bin.join("pty");
        std::fs::write(pty.with_extension("real"), real_pty.as_os_str().as_bytes()).unwrap();
        let gate_path = pty.with_extension("gate");
        let c_path = std::ffi::CString::new(gate_path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let gate = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&gate_path)
            .unwrap();
        let ready = tokio::net::UnixListener::bind(pty.with_extension("event")).unwrap();
        std::fs::write(&pty, r#"#!/bin/sh
(
  IFS= read -r permitted < "$0.gate" || exit 1
  "$(cat "$0.real")" "$@"
  outcome=$?
  python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM); s.connect(sys.argv[1]); s.sendall(bytes([int(sys.argv[2])]))' "$0.event" "$outcome"
) </dev/null >"$0.output" 2>&1 &
exit 1
"#).unwrap();
        std::fs::set_permissions(&pty, std::fs::Permissions::from_mode(0o700)).unwrap();
        let state = root.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let store = Arc::new(Store::open(&root.path().join("graph.db"), "node").unwrap());
        apply_source(
            &store,
            &format!(
                "version 2\nagent \"example/late\" {{ workspace {:?}; argv {:?} \"1000\"; }}",
                root.path().display().to_string(),
                sleep_program()
            ),
            "late-native",
        );
        let desired = store
            .desired_subjects_named(&["agent/example/late".into()])
            .unwrap()
            .remove(0);
        let runtime = Arc::new(NativeRuntime::new(&state, None, &pty));
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        Self {
            root,
            store,
            runtime,
            reconciler,
            desired,
            real_pty,
            gate,
            ready,
        }
    }

    fn dispatch(&self) -> Result<bool> {
        let member = self.desired.member.as_ref().unwrap().clone();
        let mut launch_member = member.clone();
        launch_member.environment.insert(
            "PATH".into(),
            format!("{}:${{PATH}}", self.root.path().join("bin").display()),
        );
        self.reconciler.dispatch_start(start_jobs::PreparedStart {
            subject: self.desired.clone(),
            member,
            launch_member,
            reason: "native post-producer failure regression".into(),
            request: None,
            desired_token: self.reconciler.launch_token(&self.desired.subject)?,
            selected_token: self.store.selected_desired_token(&self.desired.subject)?,
            placement_evidence: Vec::new(),
            start_request: String::new(),
            start_request_receipt: None,
            control_request: None,
            control_request_receipt: None,
            expected_incarnation: None,
            resume_operation: None,
        })
    }

    fn assert_open(&self) -> crate::store::RuntimeStartOperation {
        let operation = self
            .store
            .runtime_start_for(&self.desired.subject)
            .unwrap()
            .unwrap();
        assert!(operation.finished && operation.revoked_by.is_some());
        assert!(!operation.cleanup_verified);
        assert_eq!(operation.result.as_ref().unwrap()["producer_closed"], false);
        assert_eq!(self.store.runtime_start_counts().unwrap(), (0, 0));
        assert_eq!(
            self.store.unresolved_runtime_start_counts().unwrap(),
            (1, 1)
        );
        assert!(
            !self.dispatch().unwrap(),
            "a second start escaped the unresolved producer"
        );
        self.reconciler.reconcile_start_jobs().unwrap();
        assert!(
            self.store
                .observations_for(&self.desired.subject, "harness.diagnostic")
                .unwrap()
                .iter()
                .any(|claim| claim.body["fields"]["code"] == "start-revocation-unverified")
        );
        operation
    }

    async fn publish(&mut self) {
        use std::io::Write as _;
        use tokio::io::AsyncReadExt as _;
        self.gate.write_all(b"publish\n").unwrap();
        let (mut acknowledgment, _) =
            tokio::time::timeout(Duration::from_secs(30), self.ready.accept())
                .await
                .unwrap()
                .unwrap();
        let mut outcome = [255];
        tokio::time::timeout(
            Duration::from_secs(5),
            acknowledgment.read_exact(&mut outcome),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            outcome[0],
            0,
            "{}",
            std::fs::read_to_string(self.root.path().join("bin/pty.output")).unwrap()
        );
    }
}

#[tokio::test]
async fn native_returned_pty_error_retains_unknown_and_visible_open_count() {
    if crate::test_support::supervise_test() {
        return;
    }
    use tower::ServiceExt as _;
    let fixture = NativeLatePty::new();
    assert!(fixture.dispatch().unwrap());
    let operation = fixture.assert_open();
    assert!(fixture.runtime.snapshot_ptys().unwrap().is_empty());
    let state = fixture.root.path().join("state");
    let app = crate::api::router(crate::api::AppState {
        store: fixture.store.clone(),
        notify: Arc::new(Notify::new()),
        event_notify: tokio::sync::watch::channel(0).0,
        node: "node".into(),
        pty_root: state.join("pty"),
        state_dir: state,
        pty_binary: fixture.real_pty.clone(),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: crate::model::PlannerSpec::default(),
    });
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/v1/client/agents/agent/example/late")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let card: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(card["value"]["runtime_start"]["operation_id"], operation.id);
    assert_eq!(card["value"]["runtime_start"]["unresolved_jobs"], 1);
    assert_eq!(card["value"]["runtime_start"]["revoked_unresolved_jobs"], 1);
    assert_eq!(card["value"]["runtime_start"]["worker_running"], false);
    assert_eq!(card["value"]["runtime_start"]["cleanup_verified"], false);
    let failed = fixture
        .store
        .observations_for(&fixture.desired.subject, "runtime.action.failed")
        .unwrap()
        .pop()
        .unwrap();
    assert!(failed.body["fields"]["operation_status"].is_null());
    assert_eq!(failed.body["fields"]["operation"], operation.id);
}

#[tokio::test]
async fn native_late_pty_publication_is_retired_by_exact_operation_and_incarnation() {
    if crate::test_support::supervise_test() {
        return;
    }
    let mut fixture = NativeLatePty::new();
    assert!(fixture.dispatch().unwrap());
    let operation = fixture.assert_open();
    apply_source(
        &fixture.store,
        "version 2\nstop \"agent/example/late\"",
        "stop-late-native",
    );
    fixture.publish().await;
    let observation = fixture
        .runtime
        .snapshot_ptys()
        .unwrap()
        .into_iter()
        .find(|o| o.runtime_id == fixture.desired.member.as_ref().unwrap().runtime_id)
        .unwrap();
    let incarnation = observation.incarnation_id.as_deref().unwrap();
    assert_eq!(
        fixture
            .runtime
            .start_operation(&observation.runtime_id, true, incarnation)
            .unwrap()
            .as_deref(),
        Some(operation.id.as_str())
    );
    fixture.reconciler.reconcile_start_jobs().unwrap();
    fixture.reconciler.reconcile_start_jobs().unwrap();
    assert!(
        fixture
            .store
            .runtime_start_for(&fixture.desired.subject)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fixture.store.unresolved_runtime_start_counts().unwrap(),
        (0, 0)
    );
    let result: String = fixture
        .store
        .readers
        .get()
        .query_row(
            "SELECT result FROM local_runtime_starts WHERE id=?1",
            [&operation.id],
            |row| row.get(0),
        )
        .unwrap();
    let result: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(result["verification"], "operation-retired");
    assert_eq!(result["incarnation"], incarnation);
    let retired = fixture
        .store
        .observations_for(&fixture.desired.subject, "runtime.action.succeeded")
        .unwrap()
        .into_iter()
        .find(|claim| claim.id == result["retirement_result"])
        .unwrap();
    assert_eq!(retired.body["fields"]["operation"], operation.id);
    assert_eq!(retired.body["fields"]["operation_status"], "retired");
    assert_eq!(retired.body["fields"]["incarnation_id"], incarnation);
    assert!(
        retired.body["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == &result["retirement_request"])
    );
    assert!(
        fixture
            .store
            .observations_for(&fixture.desired.subject, "runtime.action.requested")
            .unwrap()
            .iter()
            .any(|claim| claim.body["fields"]["action"] == "terminate"
                && claim.body["fields"]["incarnation_id"] == incarnation)
    );
    assert!(
        !fixture
            .store
            .observations_for(&fixture.desired.subject, "runtime.action.succeeded")
            .unwrap()
            .iter()
            .any(|claim| claim.body["fields"]["action"] == "start"
                && claim.body["fields"]["operation"] == operation.id)
    );
}

#[tokio::test]
async fn unreadable_start_provenance_does_not_block_an_authorized_fenced_stop() {
    let (_directory, store, runtime, reconciler, release, mut entered) = fixture(true, false);
    apply_source(
        &store,
        "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"true\" }",
        "unreadable-provenance-start",
    );
    runtime.verification_hidden.store(true, Ordering::Release);
    assert!(launch(&reconciler, "agent/example/worker"));
    tokio::time::timeout(Duration::from_secs(30), entered.recv())
        .await
        .unwrap()
        .unwrap();
    release.0.send(()).unwrap();
    workers_done(&reconciler).await;
    reconciler.reconcile_start_jobs().unwrap();
    runtime.provenance_error.store(true, Ordering::Release);
    apply_source(
        &store,
        "version 2\nstop \"agent/example/worker\"",
        "unreadable-provenance-stop",
    );
    let observation = runtime.snapshot_ptys().unwrap().remove(0);
    assert!(
        !reconciler
            .reconcile_runtime_stop(
                "agent/example/worker",
                &observation.runtime_id,
                true,
                observation.incarnation_id.as_deref(),
                1000,
                Some(&observation)
            )
            .unwrap()
    );
    assert_eq!(runtime.inner.stops.lock().unwrap().len(), 1);
    let operation = store
        .runtime_start_for("agent/example/worker")
        .unwrap()
        .unwrap();
    assert!(operation.revoked_by.is_some() && !operation.cleanup_verified);
    assert_eq!(store.unresolved_runtime_start_counts().unwrap(), (1, 1));
}
