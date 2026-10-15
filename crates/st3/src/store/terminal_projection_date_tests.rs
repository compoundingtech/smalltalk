// Bounded synthetic row/envelope controls. No fleet state or production oracle.
fn typed_rows(store: &Store) -> BTreeMap<String, Vec<Vec<rusqlite::types::Value>>> {
    let reader = store.readers.get();
    ["mission_runs", "run_generations"]
        .into_iter()
        .map(|table| {
            let columns = projection_digest::columns(&reader, table, &[]).unwrap();
            let sql = format!("SELECT {} FROM {table} ORDER BY id", columns.join(","));
            let mut statement = reader.prepare(/* sqlite-large-value: allow-for-now lv-h8057f59124d1 owner=st3::store::terminal_projection_date_tests TODO=trace-statement-and-value-representation */ &sql).unwrap();
            let rows = statement
                .query_map([], |row| {
                    (0..columns.len())
                        .map(|index| row.get(index))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            (table.into(), rows)
        })
        .collect()
}

fn checked_digests(store: &Store) -> BTreeMap<String, String> {
    let reader = store.readers.get();
    let cached = projection_digest::tables(&reader).unwrap();
    // This oracle scans only the tiny isolated fixture, never a production store.
    let oracle = projection_digest::oracle(&reader).unwrap();
    ["mission_runs", "run_generations"]
        .into_iter()
        .map(|table| {
            assert_eq!(
                cached[table], oracle[table],
                "{table}: cached versus actual fixture rows"
            );
            (table.into(), oracle[table].clone())
        })
        .collect()
}

// Only mission/work state is exercised here. Any attempted runtime effect is a fixture error.
struct DateControlRuntime;

impl crate::reconcile::RuntimeControl for DateControlRuntime {
    fn snapshot_ptys(&self) -> Result<Vec<crate::reconcile::RuntimeObservation>> {
        Ok(Vec::new())
    }
    fn observe_exec(&self, _: &str) -> Result<Option<crate::reconcile::RuntimeObservation>> {
        Ok(None)
    }
    fn start(&self, _: &crate::model::MemberSpec) -> Result<()> {
        anyhow::bail!("unexpected date-control start")
    }
    fn stop(&self, _: &str, _: bool, _: Option<&str>) -> Result<()> {
        anyhow::bail!("unexpected date-control stop")
    }
    fn kill(&self, _: &str, _: bool, _: Option<&str>) -> Result<()> {
        anyhow::bail!("unexpected date-control kill")
    }
    fn remove(&self, _: &str, _: bool) -> Result<()> {
        anyhow::bail!("unexpected date-control remove")
    }
    fn screen(&self, _: &str) -> Result<String> {
        anyhow::bail!("unexpected date-control screen")
    }
    fn send_key(&self, _: &str, _: &str) -> Result<()> {
        anyhow::bail!("unexpected date-control input")
    }
    fn read_exec_log(&self, _: &str) -> Result<Option<String>> {
        Ok(None)
    }
}

type TerminalDateTuple = (String, String, String, String, String, String, String);

fn terminal_date_tuple(store: &Store, run: &MissionRunView) -> TerminalDateTuple {
    store
        .readers
        .get()
        .query_row(
            "SELECT r.status,r.phase,r.created_at_unix_ms,r.updated_at_unix_ms,
                g.status,g.created_at_unix_ms,g.updated_at_unix_ms
         FROM mission_runs r JOIN run_generations g ON g.id=r.current_generation_id
         WHERE r.id=?1 AND g.id=?2 AND g.run_id=r.id",
            params![run.id, generation_id_from_subject(&run.generation)],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .unwrap()
}

fn terminal_date_claims(store: &Store, run: &MissionRunView) -> Vec<(String, String, String)> {
    let reader = store.readers.get();
    let mut statement = reader
        .prepare(
            "SELECT id,kind,accepted_at_unix_ms FROM claims
         WHERE subject IN (?1,?2) OR subject IN (SELECT subject FROM step_runs WHERE run_id=?3)
         ORDER BY id",
        )
        .unwrap();
    statement
        .query_map(params![run.subject, run.generation, run.id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

#[derive(Debug)]
struct TerminalDateCapture {
    dates: TerminalDateTuple,
    claims: Vec<(String, String, String)>,
    steps: Vec<(String, String, u32, bool)>,
    rows: BTreeMap<String, Vec<Vec<rusqlite::types::Value>>>,
    digests: BTreeMap<String, String>,
}

fn terminal_date_capture(store: &Store, run: &MissionRunView) -> TerminalDateCapture {
    let view = store.mission_run(&run.id).unwrap().unwrap();
    assert_eq!(
        view.generation, run.generation,
        "capture the exact generation"
    );
    TerminalDateCapture {
        dates: terminal_date_tuple(store, run),
        claims: terminal_date_claims(store, run),
        steps: view
            .steps
            .iter()
            .map(|step| {
                (
                    step.subject.clone(),
                    step.status.clone(),
                    step.attempt,
                    step.worker_reported,
                )
            })
            .collect(),
        rows: typed_rows(store),
        digests: checked_digests(store),
    }
}

fn same_envelopes(left: &Store, right: &Store) {
    let identities = |store: &Store| {
        let mut ids = store
            .replication_inventory()
            .unwrap()
            .envelopes
            .into_iter()
            .map(|id| (id.writer, id.sequence, id.hash))
            .collect::<Vec<_>>();
        ids.sort();
        ids
    };
    assert_eq!(
        identities(left),
        identities(right),
        "identical sealed canonical envelope cut"
    );
    for store in [left, right] {
        assert!(
            store.replica_records(true).unwrap().is_empty(),
            "no invalid or unknown inputs"
        );
    }
}

fn receive_dates(target: &Store, envelopes: Vec<ReplicaEnvelope>) {
    target.bind_fleet(TEST_FLEET).unwrap();
    let wire = exchange_of("node", envelopes);
    target
        .receive_replication_exchange("node", TEST_FLEET, &wire)
        .unwrap();
    target.validate_replication_backlog().unwrap();
    assert!(target.project_replication_backlog().unwrap());
}

fn terminal_date_three_paths(source: &Store, run: &MissionRunView) -> [TerminalDateCapture; 3] {
    let replica = Store::open_memory("peer").unwrap();
    let wire = exchange_from(source, &replica.replication_inventory().unwrap());
    assert!(!wire.envelopes.is_empty());
    assert!(wire.envelopes.len() <= 64, "finite fixture envelope bound");
    let origin = terminal_date_capture(source, run);
    receive_dates(&replica, wire.envelopes.clone());
    same_envelopes(source, &replica);
    let copied = terminal_date_capture(&replica, run);

    // A second member receives the same physical envelopes out of order, with a real
    // durable close/open between receipt and admission. No claim bytes or dates are edited.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("arrival.sqlite3");
    let arrival = Store::open(&path, "arrival").unwrap();
    arrival.bind_fleet(TEST_FLEET).unwrap();
    for envelope in wire.envelopes.iter().rev() {
        arrival
            .receive_replication_exchange(
                "node",
                TEST_FLEET,
                &exchange_of("node", vec![envelope.clone()]),
            )
            .unwrap();
    }
    drop(arrival);
    let arrival = Store::open(&path, "arrival").unwrap();
    arrival.validate_replication_backlog().unwrap();
    assert!(arrival.project_replication_backlog().unwrap());
    same_envelopes(source, &arrival);
    let arrived = terminal_date_capture(&arrival, run);
    // In this member projection runs between each arrival, including dependencies that
    // arrive late. The final comparison still uses exactly the same canonical envelopes.
    let incremental = Store::open_memory("incremental").unwrap();
    for envelope in wire.envelopes.iter().rev() {
        receive_dates(&incremental, vec![envelope.clone()]);
    }
    same_envelopes(source, &incremental);
    let incremental_capture = terminal_date_capture(&incremental, run);
    assert_eq!(
        incremental_capture.rows, copied.rows,
        "projection between reverse arrivals"
    );
    assert_eq!(incremental_capture.digests, copied.digests);
    assert_eq!(
        arrived.rows, copied.rows,
        "arrival/reopen versus one-shot replica"
    );
    assert_eq!(arrived.digests, copied.digests);

    let frontier = source.index().unwrap();
    source.replay_replication_graph().unwrap();
    let replay = terminal_date_capture(source, run);
    assert_eq!(
        source.index().unwrap(),
        frontier,
        "replay appends no fabricated claim"
    );
    same_envelopes(source, &replica);
    // Reopening the origin uses the real projection-version/marker path.
    let origin_path = directory.path().join("origin.sqlite3");
    source.copy_store_to(&origin_path).unwrap();
    let opened = Store::open(&origin_path, "node").unwrap();
    let opened_capture = terminal_date_capture(&opened, run);
    assert_eq!(opened_capture.rows, replay.rows);
    assert_eq!(opened_capture.digests, replay.digests);
    same_envelopes(source, &opened);
    [origin, copied, replay]
}

fn assert_terminal_date_input_and_creation(
    captures: &[TerminalDateCapture; 3],
    created: &TerminalDateTuple,
) {
    for capture in captures {
        assert_eq!(capture.dates.2, created.2, "run creation date");
        assert_eq!(capture.dates.5, created.5, "generation creation date");
        assert_eq!(
            capture.claims, captures[0].claims,
            "exact lifecycle/work inputs and acceptance times"
        );
    }
}

fn assert_terminal_date_parity(captures: &[TerminalDateCapture; 3]) {
    for capture in &captures[1..] {
        assert_eq!(
            (&capture.dates.3, &capture.dates.6),
            (&captures[0].dates.3, &captures[0].dates.6),
            "isolated source mechanism only, not incident cause: origin={:?}, replica={:?}, replay={:?}",
            captures[0].dates,
            captures[1].dates,
            captures[2].dates,
        );
        assert_eq!(capture.steps, captures[0].steps);
        assert_eq!(capture.rows, captures[0].rows);
        assert_eq!(capture.digests, captures[0].digests);
    }
}

fn terminal_date_request(mission: &str, key: &str) -> MissionRunRequest {
    MissionRunRequest {
        mission: mission.into(),
        revision: None,
        workspace: "/tmp/terminal-dates".into(),
        requester: Some("person/operator".into()),
        mode: None,
        inputs: BTreeMap::new(),
        idempotency_key: key.into(),
    }
}

// This variant uses actual claim/complete, verification and cleanup, not a fabricated run state.
#[tokio::test]
async fn work_completed_child_dates_survive_later_parent_terminal_projection() {
    let source = Arc::new(Store::open_memory("node").unwrap());
    let initial_time = now_ms() + 60_000;
    source.set_write_clock_at(initial_time).unwrap();
    publish_mission(
        &source,
        r#"version 2
mission "work-date-root" state="ready" {
  goal "Keep a completed child's dates."
  step "child" { assigned-to "person/alex" }
}
mission "work-date-child" state="ready" {
  goal "Finish by submitting actual work."
  completion { when "all-steps-exhausted" }
  step "work" { assigned-to "person/operator" }
}
"#,
        "work-child-dates-missions",
    );
    let parent = source
        .create_mission_run(&terminal_date_request(
            "work-date-root",
            "work-date-root-run",
        ))
        .unwrap();
    source.set_write_clock_at(initial_time + 1_000).unwrap();
    let child = source
        .create_child_mission_run(
            &terminal_date_request("work-date-child", "work-date-child-run"),
            &parent,
            &parent.steps[0].subject,
            None,
        )
        .unwrap();
    let created = terminal_date_tuple(&source, &child);
    let reconciler = crate::reconcile::Reconciler::new(
        source.clone(),
        Arc::new(DateControlRuntime),
        "node".into(),
        Arc::new(tokio::sync::Notify::new()),
    );
    for _ in 0..4 {
        reconciler.reconcile_once().unwrap();
    }
    assert_eq!(
        source.mission_run_status(&parent.id).unwrap().as_deref(),
        Some("running"),
        "the unsubmitted parent work must keep the root live while its child works"
    );
    assert_eq!(
        source
            .step_run(&parent.steps[0].subject)
            .unwrap()
            .unwrap()
            .status,
        "ready",
        "a person-owned parent is not auto-completed by reconciliation"
    );
    assert_eq!(
        source
            .step_run(&child.steps[0].subject)
            .unwrap()
            .unwrap()
            .status,
        "ready"
    );
    source.set_write_clock_at(initial_time + 2_000).unwrap();
    let request = |key: &str| WorkRequest {
        wait_ms: None,
        actor: Some("person/operator".into()),
        incarnation: Some("date-control-worker".into()),
        summary: Some("The actual child work is complete.".into()),
        reason: None,
        evidence: Vec::new(),
        idempotency_key: key.into(),
    };
    source
        .work_action(
            &child.steps[0].subject,
            "claim",
            &request("work-date-claim"),
        )
        .unwrap();
    let submitted = source
        .work_action(
            &child.steps[0].subject,
            "complete",
            &request("work-date-complete"),
        )
        .unwrap();
    assert_eq!(submitted.status, "verifying");
    let submission = source
        .latest_claim(&submitted.subject, Some("work.submitted"))
        .unwrap()
        .unwrap();
    assert_eq!(submission.actor.as_deref(), Some("person/operator"));
    assert_eq!(
        submission
            .body
            .pointer("/fields/submitted_incarnation")
            .and_then(Value::as_str),
        Some("date-control-worker")
    );
    // A fixed finite sequence exercises real verification, phase selection and cleanup.
    for _ in 0..8 {
        reconciler.reconcile_once().unwrap();
    }
    let completed = terminal_date_tuple(&source, &child);
    assert_eq!(
        (
            completed.0.as_str(),
            completed.1.as_str(),
            completed.4.as_str()
        ),
        ("completed", "terminal", "completed")
    );
    let child_time = (initial_time + 2_000).to_string();
    assert_eq!((&completed.3, &completed.6), (&child_time, &child_time));
    let finished_step = source.step_run(&submitted.subject).unwrap().unwrap();
    assert_eq!(finished_step.status, "completed");
    assert!(finished_step.worker_reported);
    let completed_claims = terminal_date_claims(&source, &child);
    assert!(
        completed_claims
            .iter()
            .any(|claim| claim.0 == submission.id)
    );
    assert_eq!(
        source.mission_run_status(&parent.id).unwrap().as_deref(),
        Some("running"),
        "the parent must still be live until the deliberate later terminal transition"
    );
    source.set_write_clock_at(initial_time + 3_000).unwrap();
    assert!(
        source
            .set_mission_run_state(&parent.id, "completed", "terminal", None)
            .unwrap()
    );
    assert_eq!(terminal_date_tuple(&source, &child), completed);
    assert_eq!(terminal_date_claims(&source, &child), completed_claims);
    let captures = terminal_date_three_paths(&source, &child);
    assert_terminal_date_input_and_creation(&captures, &created);
    for capture in &captures {
        assert_eq!(capture.dates.0, "completed");
        assert_eq!(capture.dates.1, "terminal");
        assert_eq!(capture.dates.4, "completed");
        assert_eq!(capture.steps, captures[0].steps);
    }
    assert_terminal_date_parity(&captures);
}

#[test]
fn active_child_dates_match_after_later_parent_terminal_projection() {
    let source = Store::open_memory("node").unwrap();
    let initial_time = now_ms() + 60_000;
    source.set_write_clock_at(initial_time).unwrap();
    publish_mission(
        &source,
        r#"version 2
mission "active-date-root" state="ready" {
  goal "End with an active child."
  step "child" { agentless }
}
mission "active-date-child" state="ready" {
  goal "Remain active until the parent ends."
  step "work" { agentless }
}
"#,
        "active-child-dates-missions",
    );
    let parent = source
        .create_mission_run(&terminal_date_request(
            "active-date-root",
            "active-date-root-run",
        ))
        .unwrap();
    source.set_write_clock_at(initial_time + 1_000).unwrap();
    let child = source
        .create_child_mission_run(
            &terminal_date_request("active-date-child", "active-date-child-run"),
            &parent,
            &parent.steps[0].subject,
            None,
        )
        .unwrap();
    let created = terminal_date_tuple(&source, &child);
    assert_eq!(created.0, "running");
    source.set_write_clock_at(initial_time + 3_000).unwrap();
    assert!(
        source
            .set_mission_run_state(&parent.id, "completed", "terminal", None)
            .unwrap()
    );
    let captures = terminal_date_three_paths(&source, &child);
    assert_terminal_date_input_and_creation(&captures, &created);
    let state_claims = captures[0]
        .claims
        .iter()
        .filter(|claim| {
            matches!(
                claim.1.as_str(),
                "mission-run.state" | "run-generation.state"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        state_claims.len(),
        2,
        "real descendant cancellation emits its own two state claims"
    );
    for capture in &captures {
        assert_eq!(capture.dates.0, "cancelled");
        assert_eq!(capture.dates.1, "terminal");
        assert_eq!(capture.dates.4, "cancelled");
        assert_eq!(capture.steps[0].1, "cancelled");
        for claim in &state_claims {
            assert_eq!(claim.2, (initial_time + 3_000).to_string());
        }
    }
    assert_terminal_date_parity(&captures);
}

#[test]
fn already_terminal_root_preserves_its_creation_dates_on_replica_and_replay() {
    let source = Store::open_memory("node").unwrap();
    let initial_time = now_ms() + 60_000;
    source.set_write_clock_at(initial_time).unwrap();
    publish_mission(
        &source,
        r#"version 2
mission "terminal-date-root" state="ready" {
  goal "Preserve immutable root creation when it is already terminal."
  step "work" { agentless }
}
"#,
        "terminal-root-dates-mission",
    );
    let root = source
        .create_mission_run(&terminal_date_request(
            "terminal-date-root",
            "terminal-date-root-run",
        ))
        .unwrap();
    let created = terminal_date_tuple(&source, &root);
    let creation_claim = source
        .latest_claim(&root.subject, Some("mission-run.created"))
        .unwrap()
        .unwrap();
    assert_eq!(created.2, creation_claim.accepted_at_unix_ms.to_string());
    assert_eq!(created.5, creation_claim.accepted_at_unix_ms.to_string());
    source.set_write_clock_at(initial_time + 2_000).unwrap();
    assert!(
        source
            .set_mission_run_state(&root.id, "completed", "terminal", None)
            .unwrap()
    );
    let captures = terminal_date_three_paths(&source, &root);
    // This separate guard checks creation stability before updated-date parity.
    assert_terminal_date_input_and_creation(&captures, &created);
    for capture in &captures {
        assert_eq!(capture.dates.0, "completed");
        assert_eq!(capture.dates.1, "terminal");
        assert_eq!(capture.dates.4, "completed");
        assert!(
            capture
                .claims
                .iter()
                .any(|claim| claim.0 == creation_claim.id)
        );
    }
    assert_terminal_date_parity(&captures);
}

// A certificate-driven trim/adoption path uses the runtime's actual rules and proof.
// This is a tiny test database, never a permission to trim a fleet store.
#[test]
fn completed_child_dates_survive_certified_trim_adoption_and_reopen() {
    use smallclaims::store::checkpoint::checkpoint_name;
    use smallclaims::store::checkpoint_agreement::SealTerms;
    let source = Store::open_memory("node").unwrap();
    let at = now_ms() + 60_000;
    source.set_write_clock_at(at).unwrap();
    publish_mission(
        &source,
        r#"version 2
mission "trim-date-root" state="ready" { goal "Keep ended children."; step "child" { agentless } }
mission "trim-date-child" state="ready" { goal "End before parent."; step "work" { agentless } }
"#,
        "trim-date-missions",
    );
    let root = source
        .create_mission_run(&terminal_date_request("trim-date-root", "trim-root"))
        .unwrap();
    let child = source
        .create_child_mission_run(
            &terminal_date_request("trim-date-child", "trim-child"),
            &root,
            &root.steps[0].subject,
            None,
        )
        .unwrap();
    source.set_write_clock_at(at + 1_000).unwrap();
    assert!(
        source
            .set_mission_run_state(&child.id, "completed", "terminal", None)
            .unwrap()
    );
    source.set_write_clock_at(at + 2_000).unwrap();
    assert!(
        source
            .set_mission_run_state(&root.id, "completed", "terminal", None)
            .unwrap()
    );
    // Repeated observations give the real retention planner superseded source to drop.
    for state in ["open", "closed", "open"] {
        source
            .append_claim(&ClaimInput {
                subject: "resource/date-trim".into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("kind".into(), json!("vcs.pull-request")),
                    ("observer".into(), json!("observer/date-trim")),
                    ("facts".into(), json!({"state":state})),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    let rows = typed_rows(&source);
    let digests = checked_digests(&source);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trim-peer.sqlite3");
    let peer = Store::open(&path, "peer").unwrap();
    receive_dates(
        &peer,
        exchange_from(&source, &peer.replication_inventory().unwrap()).envelopes,
    );
    same_envelopes(&source, &peer);
    assert_eq!(
        typed_rows(&peer),
        rows,
        "before trim, the identical cut agrees"
    );
    let cut = at + 2_001;
    let checkpoint = checkpoint_name(cut);
    let sealed = source.checkpoint_sealed_identities(cut, None).unwrap();
    let terms = SealTerms {
        cut_unix_ms: cut,
        participants: BTreeSet::from(["node".into()]),
        sealed_digest: sealed.digest.clone(),
        rules_digest: rules_digest(),
    };
    source
        .publish_seal(&checkpoint, &terms, &sealed, None)
        .unwrap();
    let (_, plan, proof) = source
        .plan_checkpoint_through(cut, None, directory.path())
        .unwrap();
    assert!(proof.passed, "actual replay/retention proof: {proof:?}");
    assert!(
        !plan.claims.is_empty(),
        "exercise real claim deletion, not an empty trim"
    );
    source
        .publish_verification(
            &checkpoint,
            &terms,
            &smallclaims::store::checkpoint_staging::StagedPlan::of(&plan),
            &proof,
        )
        .unwrap();
    receive_dates(
        &peer,
        exchange_from(&source, &peer.replication_inventory().unwrap()).envelopes,
    );
    same_envelopes(&source, &peer);
    let mut actions = Vec::new();
    source
        .trim_checkpoint(
            &checkpoint,
            cut,
            &plan.drop_digest,
            &plan.envelopes,
            &plan.claims,
            false,
            &mut actions,
        )
        .unwrap();
    let manifest = source.checkpoint_manifest(&checkpoint, cut).unwrap();
    assert!(!peer.adopt_checkpoint(&manifest).unwrap().is_empty());
    assert_eq!(
        source.checkpoint_manifest(&checkpoint, cut).unwrap(),
        peer.checkpoint_manifest(&checkpoint, cut).unwrap()
    );
    assert_eq!(typed_rows(&source), rows);
    assert_eq!(typed_rows(&peer), rows);
    assert_eq!(checked_digests(&source), digests);
    assert_eq!(checked_digests(&peer), digests);
    let before = peer.index().unwrap();
    peer.replay_replication_graph().unwrap();
    assert_eq!(peer.index().unwrap(), before);
    assert_eq!(typed_rows(&peer), rows);
    drop(peer);
    let peer = Store::open(&path, "peer").unwrap();
    assert_eq!(typed_rows(&peer), rows);
    assert_eq!(checked_digests(&peer), digests);
    same_envelopes(&source, &peer);
}
