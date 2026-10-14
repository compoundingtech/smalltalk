use super::*;
use serde_json::json;
use smallclaims::ivm::install::{Installer, Limits, Outcome, ScanPage};

pub(crate) fn facts() -> Value {
    json!({
        "policy":{"version":"fixture-selected.v1","complete":true},
        "gate":{"revision":"rev/one","name":"prepared","subject":"exec/probe","path":"exit_code","operator":"is","expected":0},
        "run":{"id":"mission-run/one","mission":"mission/one","revision":"rev/one","generation":"run-generation/one","status":"running","phase":"active"},
        "generation":{"id":"run-generation/one","run":"mission-run/one","revision":"rev/one"},
        "step":{"id":"step-run/one","generation":"run-generation/one","name":"prepare","status":"working"},
        "desired":{"claim":"claim/launch-one","subject":"exec/probe","kind":"exec","host":"node/one","lifecycle":"service","restart":"never","run":"mission-run/one","generation":"run-generation/one","step":"step-run/one"},
        "observed":{"claim":"claim/exit-two","origin":"node/one","status":"exited","exit_code":2,"incarnation":"incarnation/one","evidence":["claim/launch-one"]},
        "domain":{"complete":true,"origin":"node/one","kinds":["runtime.observed"]}
    })
}
pub(crate) fn mutation(key: &str, value: Option<Value>) -> Mutation {
    Mutation {
        key: key.into(),
        old: None,
        new: value.map(|v| Value::String(v.to_string())),
    }
}
fn encoded(value: Value) -> Value {
    Value::String(value.to_string())
}

pub(crate) struct Fixture {
    pub(crate) db: Connection,
    pub(crate) installer: Installer,
}
impl Fixture {
    pub(crate) fn new() -> Self {
        let mut db = Connection::open_in_memory().unwrap();
        let installer = Installer::new(vec![Box::new(TerminalGates)]).unwrap();
        installer.create_schema(&db).unwrap();
        let tx = db.transaction().unwrap();
        // Explicit fixture registration of EMPTY source; no native Store or input history.
        installer
            .register_source(&tx, SOURCE, "fixture-selected.v1", 1)
            .unwrap();
        let job = installer
            .start(
                &tx,
                VIEW,
                Limits {
                    page_rows: 16,
                    page_bytes: 16 * 1024,
                    pending_rows: 64,
                    pending_bytes: 256 * 1024,
                    total_rows: 128,
                    callback_ms: 1000,
                    lifetime_ms: 60_000,
                },
                0,
            )
            .unwrap();
        let position = installer.position(&tx, SOURCE).unwrap();
        assert_eq!(
            installer
                .scan(
                    &tx,
                    &ScanPage {
                        job: job.clone(),
                        expected_cursor: vec![],
                        next_cursor: vec![],
                        position,
                        rows: vec![],
                        finished: true
                    },
                    1
                )
                .unwrap(),
            Outcome::Progress
        );
        assert_eq!(
            installer.catch_up(&tx, &job, 2).unwrap(),
            Outcome::Published
        );
        tx.commit().unwrap();
        Self { db, installer }
    }
    pub(crate) fn record(&mut self, mutation: Mutation) {
        let tx = self.db.transaction().unwrap();
        self.installer.record(&tx, SOURCE, &mutation).unwrap();
        tx.commit().unwrap();
    }
    fn member_rows(&self, ns: &Namespace) -> usize {
        // Physical storage proof in fixtures only, deliberately LIMIT 65 instead of COUNT.
        self.db
            .prepare("SELECT key FROM test_terminal_members WHERE namespace=?1 LIMIT 65")
            .unwrap()
            .query_map([ns.as_str()], |r| r.get::<_, String>(0))
            .unwrap()
            .count()
    }
}

#[test]
fn dormant_operator_accepts_six_nonselecting_action_kinds_and_terminal_observation() {
    let mut input = facts();
    input["domain"]["kinds"] = json!(
        std::iter::once("runtime.observed")
            .chain(NON_SELECTING.iter().copied())
            .collect::<Vec<_>>()
    );
    input["observed"]["evidence"] = json!(vec!["claim/launch-one"; 16]);
    let text = witness(&encoded(input.clone()))
        .unwrap()
        .into_witness()
        .unwrap();
    assert!(text.contains("exit code 2") && text.contains("will not restart"));
    let mut fixture = Fixture::new();
    fixture.record(mutation("gate/one", Some(input)));
    let root = fixture.installer.root(&fixture.db, VIEW).unwrap();
    assert_eq!(fixture.member_rows(&root.namespace), 1);
}

#[test]
fn dormant_key_refusal_retracts_only_that_key_and_preserves_another_proven_warning() {
    for (path, value) in [
        (["domain", "origin"], json!("node/rival")),
        (
            ["domain", "kinds"],
            json!(["runtime.observed", "agent.output"]),
        ),
        (
            ["domain", "kinds"],
            json!(["runtime.observed", "runtime.action.new-kind"]),
        ),
        (["domain", "complete"], json!(false)),
        (["policy", "version"], json!("unsupported.v9")),
        (["observed", "evidence"], json!(["claim/old-launch"])),
        (["observed", "exit_code"], Value::Null),
        (["generation", "revision"], json!("rev/old")),
        (["desired", "restart"], json!("always")),
        (["gate", "subject"], json!("exec/${ST_ATTEMPT}")),
        (["gate", "operator"], json!("contains")),
        (["gate", "expected"], json!("not-an-integer")),
    ] {
        let mut input = facts();
        input[path[0]][path[1]] = value;
        assert_eq!(
            witness(&encoded(input.clone())).unwrap(),
            Decision::NoWitness,
            "{path:?}"
        );
        let mut fixture = Fixture::new();
        fixture.record(mutation("gate/one", Some(facts())));
        fixture.record(mutation("gate/other", Some(facts())));
        fixture.record(mutation("gate/one", Some(input)));
        let root = fixture.installer.root(&fixture.db, VIEW).unwrap();
        assert!(fixture.installer.status(&fixture.db, VIEW).unwrap().ready);
        assert_eq!(fixture.member_rows(&root.namespace), 1);
        let key: String = fixture
            .db
            .query_row(
                "SELECT key FROM test_terminal_members WHERE namespace=?1",
                [root.namespace.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(key, "gate/other");
    }
}

#[test]
fn dormant_global_capture_encoding_and_storage_failures_still_fence_the_namespace() {
    let mut incomplete = facts();
    incomplete["policy"]["complete"] = json!(false);
    for input in [
        encoded(incomplete),
        Value::String("{\"policy\":}".into()),
        Value::String(" ".repeat(INPUT_BYTES + 1)),
    ] {
        assert!(witness(&input).is_err());
        let mut fixture = Fixture::new();
        fixture.record(mutation("gate/one", Some(facts())));
        fixture.record(mutation("gate/other", Some(facts())));
        let root = fixture.installer.root(&fixture.db, VIEW).unwrap();
        fixture.record(Mutation {
            key: "gate/one".into(),
            old: None,
            new: Some(input),
        });
        assert!(fixture.installer.root(&fixture.db, VIEW).is_err());
        assert!(!fixture.installer.status(&fixture.db, VIEW).unwrap().ready);
        // Saved outputs remain physically present but may not be presented as current.
        assert_eq!(fixture.member_rows(&root.namespace), 2);
    }
    let mut fixture = Fixture::new();
    fixture.record(mutation("gate/one", Some(facts())));
    fixture
        .db
        .execute_batch("DROP TABLE test_terminal_members")
        .unwrap();
    fixture.record(mutation("gate/other", Some(facts())));
    assert!(fixture.installer.root(&fixture.db, VIEW).is_err());
    assert!(!fixture.installer.status(&fixture.db, VIEW).unwrap().ready);
}

#[test]
fn dormant_decoder_caps_bytes_depth_markers_identifiers_and_evidence_before_use() {
    for raw in [
        " ".repeat(INPUT_BYTES + 1),
        format!("{}0{}", "[".repeat(9), "]".repeat(9)),
        format!(
            "[{}]",
            std::iter::repeat_n("0", STRUCTURAL_MARKERS + 1)
                .collect::<Vec<_>>()
                .join(",")
        ),
    ] {
        assert!(preflight(&raw).is_err());
    }
    assert!(witness(&Value::Null).is_err());
    let mut input = facts();
    input["gate"]["name"] = json!("x".repeat(129));
    assert!(witness(&encoded(input)).is_err());
    let mut input = facts();
    input["observed"]["evidence"] = json!(vec!["claim/launch-one"; 17]);
    assert!(witness(&encoded(input)).is_err());
    let mut input = facts();
    input["observed"]["incarnation"] = json!("bad\nidentity");
    assert!(witness(&encoded(input)).is_err());
    let mut input = facts();
    input["ninth_fact"] = json!(true);
    assert_eq!(witness(&encoded(input)).unwrap(), Decision::NoWitness);
    // Escaped strings are not mistaken for nested JSON syntax.
    preflight(r#"{"text":"[\"{,\""}"#).unwrap();
}

#[test]
fn dormant_expansion_accepts_stable_bindings_and_refuses_attempt_assignee_and_inputs() {
    for variable in [
        "ST_MISSION_RUN",
        "ST_RUN_GENERATION",
        "ST_STEP",
        "ST_STEP_RUN",
        "ST_MISSION",
        "ST_MISSION_REVISION",
    ] {
        let mut input = facts();
        input["gate"]["subject"] = json!(format!("exec/${{{variable}}}"));
        let f: Facts = serde_json::from_value(input.clone()).unwrap();
        input["desired"]["subject"] = json!(expanded_subject(&f).unwrap().unwrap());
        assert!(matches!(
            witness(&encoded(input)).unwrap(),
            Decision::Negative(_)
        ));
    }
    for variable in [
        "ST_ATTEMPT",
        "ST_ASSIGNEE",
        "ST_WORKSPACE",
        "ST_INPUT",
        "PATH",
    ] {
        let mut input = facts();
        input["gate"]["subject"] = json!(format!("exec/${{{variable}}}"));
        assert_eq!(witness(&encoded(input)).unwrap(), Decision::NoWitness);
    }
}

#[test]
fn dormant_operator_retracts_cancelled_running_matching_and_relaunched_evidence() {
    for (path, value) in [
        (["run", "status"], json!("cancelled")),
        (["step", "status"], json!("completed")),
        (["observed", "status"], json!("running")),
        (["observed", "exit_code"], json!(0)),
    ] {
        let mut fixture = Fixture::new();
        fixture.record(mutation("gate/one", Some(facts())));
        let mut input = facts();
        input[path[0]][path[1]] = value;
        fixture.record(mutation("gate/one", Some(input)));
        let root = fixture.installer.root(&fixture.db, VIEW).unwrap();
        assert_eq!(fixture.member_rows(&root.namespace), 0);
    }
    let mut fixture = Fixture::new();
    fixture.record(mutation("gate/old", Some(facts())));
    fixture.record(mutation("gate/old", None));
    let mut next = facts();
    next["desired"]["claim"] = json!("claim/launch-two");
    next["observed"]["evidence"] = json!(["claim/launch-two"]);
    fixture.record(mutation("gate/new", Some(next)));
    let root = fixture.installer.root(&fixture.db, VIEW).unwrap();
    assert_eq!(fixture.member_rows(&root.namespace), 1);
    let keys: String = fixture
        .db
        .query_row(
            "SELECT key FROM test_terminal_members WHERE namespace=?1",
            [root.namespace.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(keys, "gate/new");
}

#[test]
fn dormant_membership_is_physically_capped_and_overflow_fences_without_dropping_witnesses() {
    let mut fixture = Fixture::new();
    for n in 0..MEMBERS {
        fixture.record(mutation(&format!("gate/{n:03}"), Some(facts())));
    }
    let root = fixture.installer.root(&fixture.db, VIEW).unwrap();
    assert_eq!(fixture.member_rows(&root.namespace), MEMBERS);
    fixture.record(mutation("gate/overflow", Some(facts())));
    assert!(fixture.installer.root(&fixture.db, VIEW).is_err());
    assert_eq!(fixture.member_rows(&root.namespace), MEMBERS);
    assert!(
        fixture
            .installer
            .status(&fixture.db, VIEW)
            .unwrap()
            .error
            .unwrap()
            .contains("membership cap")
    );
}

#[test]
fn dormant_duplicate_equality_and_rollback_preserve_membership_and_generation() {
    let mut fixture = Fixture::new();
    fixture.record(mutation("gate/one", Some(facts())));
    let before_duplicate = fixture.installer.root(&fixture.db, VIEW).unwrap();
    fixture.record(mutation("gate/one", Some(facts())));
    // An admitted duplicate advances source revision, but not semantic generation.
    // The rollback baseline is the cut AFTER that independently committed duplicate.
    let root = fixture.installer.root(&fixture.db, VIEW).unwrap();
    assert_eq!(root.generation, before_duplicate.generation);
    assert_eq!(root.revision, before_duplicate.revision + 1);
    let before = fixture.db.total_changes();
    let tx = fixture.db.transaction().unwrap();
    assert!(
        !TerminalGates
            .apply(&tx, &root.namespace, &[mutation("gate/one", Some(facts()))])
            .unwrap()
    );
    tx.commit().unwrap();
    assert_eq!(fixture.db.total_changes(), before);
    assert_eq!(fixture.installer.root(&fixture.db, VIEW).unwrap(), root);
    let tx = fixture.db.transaction().unwrap();
    fixture
        .installer
        .record(&tx, SOURCE, &mutation("gate/one", None))
        .unwrap();
    let during_rollback = fixture.installer.root(&tx, VIEW).unwrap();
    assert_eq!(during_rollback.revision, root.revision + 1);
    assert_eq!(during_rollback.generation, root.generation + 1);
    tx.rollback().unwrap();
    assert_eq!(fixture.installer.root(&fixture.db, VIEW).unwrap(), root);
    assert_eq!(fixture.member_rows(&root.namespace), 1);
}

thread_local! { static SQL_STATEMENTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
fn count_sql(_: &str) {
    SQL_STATEMENTS.with(|count| count.set(count.get() + 1));
}

#[test]
fn dormant_fixture_flush_probe_and_unrelated_lookup_have_fixed_measured_sql_work() {
    let mut fixture = Fixture::new();
    let root = fixture.installer.root(&fixture.db, VIEW).unwrap();
    // The proposed dispatcher still performs this ONE probe even for unsupported steps.
    // This is a fixture seam, not a measured native after_projection hook or latency proof.
    for growth in [0, 1024] {
        if growth != 0 {
            // One row per foreign namespace: supporting index fixture, not maintained
            // native authority state. No namespace exceeds the membership cap.
            fixture.db.execute_batch("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<1024)
                INSERT INTO test_terminal_members SELECT 'foreign/'||i,'other','other witness' FROM n;").unwrap();
        }
        for sql in [
            "SELECT members<>0 FROM test_terminal_meta WHERE namespace=?1",
            "SELECT EXISTS(SELECT 1 FROM test_terminal_members WHERE namespace=?1 AND key='unrelated')",
        ] {
            SQL_STATEMENTS.with(|count| count.set(0));
            fixture.db.trace(Some(count_sql));
            let before = fixture.db.total_changes();
            let mut statement = fixture.db.prepare(sql).unwrap();
            let found: bool = statement
                .query_row([root.namespace.as_str()], |r| r.get(0))
                .unwrap();
            let vm = statement.get_status(rusqlite::StatementStatus::VmStep);
            let scans = statement.get_status(rusqlite::StatementStatus::FullscanStep);
            drop(statement);
            fixture.db.trace(None);
            assert!(!found);
            assert_eq!(SQL_STATEMENTS.with(|count| count.get()), 1);
            assert!(vm > 0 && vm <= 40, "fixture VM steps {vm}");
            assert_eq!(scans, 0);
            assert_eq!(fixture.db.total_changes(), before);
        }
    }
}

#[test]
fn dormant_layout_is_absent_from_an_ordinary_native_store() {
    let store = crate::store::Store::open_memory("node").unwrap();
    let installed: bool = store
        .readers
        .get()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name LIKE 'test_terminal_%')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!installed);
}

#[test]
fn dormant_retraction_permutations_reach_the_same_output_and_fanout_overflow_refuses() {
    for order in [["gate/a", "gate/b"], ["gate/b", "gate/a"]] {
        let mut fixture = Fixture::new();
        for key in order {
            fixture.record(mutation(key, Some(facts())));
        }
        fixture.record(mutation("gate/a", None));
        let root = fixture.installer.root(&fixture.db, VIEW).unwrap();
        assert_eq!(fixture.member_rows(&root.namespace), 1);
        let key: String = fixture
            .db
            .query_row(
                "SELECT key FROM test_terminal_members WHERE namespace=?1",
                [root.namespace.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(key, "gate/b");
        let rows = (0..17)
            .map(|n| mutation(&format!("gate/{n}"), Some(facts())))
            .collect::<Vec<_>>();
        let tx = fixture.db.transaction().unwrap();
        assert!(TerminalGates.apply(&tx, &root.namespace, &rows).is_err());
        tx.rollback().unwrap();
        assert_eq!(fixture.installer.root(&fixture.db, VIEW).unwrap(), root);
    }
}
