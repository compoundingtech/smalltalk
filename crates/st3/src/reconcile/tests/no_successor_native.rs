//! Real, isolated PTY effects with an invented provider command. No fleet endpoint or input.
use super::*;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

struct FixtureRuntime {
    native: NativeRuntime,
    starts: AtomicUsize,
    revision_file: PathBuf,
}
impl FixtureRuntime {
    fn observed(&self, id: &str) -> RuntimeObservation {
        self.native
            .snapshot_ptys()
            .unwrap()
            .into_iter()
            .find(|o| o.runtime_id == id)
            .unwrap()
    }
}
impl Drop for FixtureRuntime {
    fn drop(&mut self) {
        if let Ok(items) = self.native.snapshot_ptys() {
            for item in items {
                let _ = self
                    .native
                    .kill(&item.runtime_id, true, item.incarnation_id.as_deref());
                let _ = self.native.remove(&item.runtime_id, true);
            }
        }
    }
}
impl RuntimeControl for FixtureRuntime {
    fn snapshot_ptys(&self) -> Result<Vec<RuntimeObservation>> {
        self.native.snapshot_ptys()
    }
    fn observe_exec(&self, id: &str) -> Result<Option<RuntimeObservation>> {
        self.native.observe_exec(id)
    }
    fn start(&self, member: &MemberSpec) -> Result<()> {
        self.start_guarded(member, &|| Ok(()))
    }
    fn start_guarded(&self, member: &MemberSpec, guard: &dyn Fn() -> Result<()>) -> Result<()> {
        let mut fixture = member.clone();
        fixture.launch = crate::model::LaunchSpec::Shell("printf '%s' \"${ST3_TURN_DESIRED_REVISION-unset}\" > \"$FIXTURE_REVISION_FILE\"; exec sleep 120".into());
        fixture.environment.insert(
            "FIXTURE_REVISION_FILE".into(),
            self.revision_file.to_string_lossy().into_owned(),
        );
        self.native.start_guarded(&fixture, guard)?;
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn stop(&self, id: &str, terminal: bool, incarnation: Option<&str>) -> Result<()> {
        self.native.stop(id, terminal, incarnation)
    }
    fn kill(&self, id: &str, terminal: bool, incarnation: Option<&str>) -> Result<()> {
        self.native.kill(id, terminal, incarnation)
    }
    fn remove(&self, id: &str, terminal: bool) -> Result<()> {
        self.native.remove(id, terminal)
    }
    fn screen(&self, id: &str) -> Result<String> {
        self.native.screen(id)
    }
    fn send_key(&self, _: &str, _: &str) -> Result<()> {
        anyhow::bail!("fixture forbids native input")
    }
    fn read_exec_log(&self, id: &str) -> Result<Option<String>> {
        self.native.read_exec_log(id)
    }
}
fn ready(subject: &str, incarnation: &str, sequence: u64) -> crate::harness_events::Publication {
    crate::harness_events::Publication {
        runtime_incarnation: incarnation.into(),
        sequence,
        claim: ClaimInput {
            subject: subject.into(),
            kind: "harness.observed".into(),
            actor: Some(subject.into()),
            fields: BTreeMap::from([
                ("state".into(), json!("idle")),
                ("driver".into(), json!("omp")),
                ("incarnation_id".into(), json!(incarnation)),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some(format!("fixture-ready-{sequence}")),
        },
    }
}

#[test]
fn isolated_native_missing_successor_fault_then_relaunch_admission_and_intentional_stop() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&root.path().join("store.sqlite"), "node").unwrap());
    let source = format!(
        "version 2\nagent \"receipt-native\" {{ workspace {:?}; harness \"omp\" {{}} }}\n",
        root.path().display().to_string()
    );
    apply_source(&store, &source, "isolated-native-desired");
    let desired = store.desired_subjects().unwrap().remove(0);
    let member = desired.member.as_ref().unwrap();
    let runtime = Arc::new(FixtureRuntime {
        native: NativeRuntime::new(
            root.path(),
            Some(&root.path().join("private-pty")),
            Path::new("pty"),
        ),
        starts: AtomicUsize::new(0),
        revision_file: root.path().join("captured-revision"),
    });
    let mut reconciler = Reconciler::new(
        store.clone(),
        runtime.clone(),
        "node".into(),
        Arc::new(Notify::new()),
    );
    reconciler.driver_state_dir = root.path().join("private-drivers");
    reconciler.endpoint = format!("unix:{}", root.path().join("unused-fixture.sock").display());
    reconciler.runtime_environment.insert(
        "ST3_TURN_DESIRED_REVISION".into(),
        "enclosing-seat-stale".into(),
    );
    reconciler.reconcile_once().unwrap();
    let first = runtime.observed(&member.runtime_id);
    assert_eq!(first.status, "running");
    reconciler.reconcile_once().unwrap(); // Adopt the real PID and creation time.
    let first_incarnation = first.incarnation_id.as_deref().unwrap();
    let first_claim = reconciler
        .running_runtime_claim(&desired.subject, first_incarnation)
        .unwrap()
        .unwrap();
    let deadline = first_claim.accepted_at_unix_ms + HARNESS_READINESS_DEADLINE_MS + 1;
    reconciler
        .reconcile_driver_readiness(&desired, member, &first, deadline)
        .unwrap();
    assert_eq!(store.fault_items(Some("person/operator")).unwrap().len(), 1);
    assert!(store.current_harness(&desired.subject).unwrap().is_none());
    assert_eq!(
        runtime.starts.load(Ordering::SeqCst),
        1,
        "missing observation must not blindly relaunch"
    );
    assert_eq!(
        fs::read_to_string(&runtime.revision_file).unwrap(),
        crate::store::desired_revision(&desired)
    );

    // Corrupt only this fixture's immutable registry stamp while its real process is live.
    let metadata = root
        .path()
        .join("private-pty")
        .join(format!("{}.json", member.runtime_id));
    let original = fs::read(&metadata).unwrap();
    let mut broken: Value = serde_json::from_slice(&original).unwrap();
    broken["createdAt"] = json!("");
    fs::write(&metadata, serde_json::to_vec(&broken).unwrap()).unwrap();
    let missing = runtime.observed(&member.runtime_id);
    assert_eq!(missing.status, "running");
    assert!(
        missing.incarnation_id.is_none(),
        "empty metadata cannot manufacture PID: as native identity"
    );
    reconciler.reconcile_once().unwrap();
    assert!(
        store
            .member_reconcile_fault(&desired.subject, None)
            .unwrap()
            .unwrap()
            .contains("no usable physical incarnation")
    );
    assert_eq!(
        runtime.starts.load(Ordering::SeqCst),
        1,
        "unknown runtime ownership cannot authorize replacement"
    );
    fs::write(&metadata, original).unwrap();
    reconciler.reconcile_once().unwrap();
    assert!(
        store
            .member_reconcile_fault(&desired.subject, None)
            .unwrap()
            .is_none()
    );

    runtime
        .kill(&member.runtime_id, true, Some(first_incarnation))
        .unwrap();
    runtime.remove(&member.runtime_id, true).unwrap();
    reconciler
        .perform_start(&desired, member, "isolated explicit relaunch fixture")
        .unwrap();
    let second = runtime.observed(&member.runtime_id);
    assert_ne!(first.incarnation_id, second.incarnation_id);
    // Before graph adoption, even the genuine successor cannot publish as a running seat.
    let second_incarnation = second.incarnation_id.as_deref().unwrap();
    assert_eq!(
        store
            .append_harness_event(&ready(&desired.subject, second_incarnation, 1))
            .unwrap_err()
            .code,
        "stale-harness-event-session"
    );
    reconciler.reconcile_once().unwrap();
    assert_eq!(
        store
            .append_harness_event(&ready(&desired.subject, first_incarnation, 2))
            .unwrap_err()
            .code,
        "stale-harness-event-session"
    );
    store
        .append_harness_event(&ready(&desired.subject, second_incarnation, 1))
        .unwrap();
    reconciler
        .reconcile_driver_readiness(&desired, member, &second, deadline + 1)
        .unwrap();
    assert!(
        store
            .current_harness(&desired.subject)
            .unwrap()
            .unwrap()
            .is_ready()
    );
    assert!(
        store
            .fault_items(Some("person/operator"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(runtime.starts.load(Ordering::SeqCst), 2);

    apply_source(
        &store,
        &format!("version 2\nstop {:?}\n", desired.subject),
        "isolated-stop",
    );
    reconciler.reconcile_once().unwrap();
    reconciler.reconcile_once().unwrap();
    assert_eq!(
        runtime.starts.load(Ordering::SeqCst),
        2,
        "intentional stop has no successor obligation"
    );
}

#[test]
fn physical_native_incarnation_refuses_missing_blank_and_invalid_pid() {
    let mut observation = st_runtime::PtyObservation {
        name: "fixture".into(),
        status: "running".into(),
        exit_code: None,
        pid: Some(42),
        created_at: Some("created".into()),
        display_name: None,
        tags: BTreeMap::new(),
    };
    assert_eq!(pty_incarnation(&observation).as_deref(), Some("42:created"));
    for pid in [None, Some(0), Some(u32::MAX)] {
        observation.pid = pid;
        assert!(pty_incarnation(&observation).is_none());
    }
    observation.pid = Some(42);
    for stamp in [None, Some("".into()), Some(" ".into())] {
        observation.created_at = stamp;
        assert!(pty_incarnation(&observation).is_none());
    }
}

#[test]
fn native_exec_non_agent_purges_authored_reserved_turn_revision() {
    let root = tempfile::tempdir().unwrap();
    let intent=crate::graph::parse_test_intent(&format!("version 2\nexec \"fixture\" {{ workspace {:?}; command \"true\"; env {{ ST3_TURN_DESIRED_REVISION \"authored-not-an-agent\"; }} }}",root.path().display().to_string()),"node").unwrap();
    let mut member = intent
        .subjects
        .values()
        .find_map(|subject| subject.member.clone())
        .unwrap();
    member.launch = crate::model::LaunchSpec::Shell(
        "printf '%s' \"${ST3_TURN_DESIRED_REVISION-unset}\"".into(),
    );
    let runtime = NativeRuntime::new(root.path(), None, Path::new("unused-pty"));
    runtime.start(&member).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if runtime
            .observe_exec(&member.runtime_id)
            .unwrap()
            .is_some_and(|o| o.status == "exited")
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        runtime.read_exec_log(&member.runtime_id).unwrap().unwrap(),
        "unset"
    );
}
