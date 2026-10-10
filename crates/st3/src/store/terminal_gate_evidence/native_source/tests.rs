use super::*;
use crate::graph::parse_intent;
use crate::store::{
    Store, append_claim_tx, project_mission_run_created, select_replicated_desired,
    select_replicated_mission,
};
use serde_json::json;
use smallclaims::sqlite::work::{SqliteWork, SqliteWorkScope};

struct Fixture {
    source: NativeSource,
    store: Store,
    dir: tempfile::TempDir,
}
impl Fixture {
    fn empty() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("claims.sqlite3"), "node").unwrap();
        let source = {
            let mut writer = store.connection.write();
            let tx = writer.transaction().unwrap();
            let source = NativeSource::attach(
                &tx,
                Binding {
                    origin: "node".into(),
                    mission: "one".into(),
                    run: "mission-run/one".into(),
                    generation: "run-generation/one".into(),
                    step: "step-run/one/prepare".into(),
                    path: "prepare".into(),
                    gate: "prepared".into(),
                    exec: "exec/probe".into(),
                },
            )
            .unwrap();
            tx.commit().unwrap();
            source
        };
        Self { source, store, dir }
    }
    fn seeded() -> Self {
        let fixture = Self::empty();
        let intent = parse_intent(
            r#"
version 2
exec "probe" { workspace "/tmp"; command "true"; restart "never" }
mission "one" state="ready" {
 goal "Private source fixture."
 step "prepare" {
   gate "prepared" { field "exit_code" "exec/probe" is 0 }
 }
}
"#,
            "node",
        )
        .unwrap();
        let mission = intent.missions.values().next().unwrap();
        let mut desired = intent.subjects["exec/probe"].clone();
        desired.owner_run = Some(fixture.source.binding.run.clone());
        desired.owner_generation = Some(fixture.source.binding.generation.clone());
        desired.owner_step = Some(fixture.source.binding.step.clone());
        let mut writer = fixture.store.connection.write();
        let tx = writer.transaction().unwrap();
        let publication = append_claim_tx(
            &tx,
            "node",
            &mission.subject,
            "mission.published",
            Some("person/test"),
            &serde_json::to_value(mission).unwrap(),
            &[],
            None,
        )
        .unwrap();
        select_replicated_mission(&tx, &publication, publication.store_index).unwrap();
        let created = append_claim_tx(
            &tx,
            "node",
            "mission-run/one",
            "mission-run.created",
            None,
            &json!({"fields":{"mission":"mission/one","revision":mission.revision,
                "current_generation":"run-generation/one","workspace":"/tmp",
                "requester":"person/test","mode":"run","root_revision":mission.revision,
                "root_mission_run":"mission-run/one"},"evidence":[]}),
            &[],
            None,
        )
        .unwrap();
        project_mission_run_created(&tx, &created).unwrap();
        tx.execute(
            "UPDATE step_runs SET status='working' WHERE subject='step-run/one/prepare'",
            [],
        )
        .unwrap();
        let declaration = append_claim_tx(
            &tx,
            "node",
            "exec/probe",
            "intent.desired",
            Some("person/test"),
            &serde_json::to_value(&desired).unwrap(),
            &[],
            None,
        )
        .unwrap();
        select_replicated_desired(&tx, &declaration, &desired).unwrap();
        append_claim_tx(
            &tx,
            "node",
            "exec/probe",
            "runtime.observed",
            None,
            &json!({"fields":{"runtime_id":"fixture-probe","host":"node","terminal":false,
                "status":"exited","exit_code":2,"incarnation_id":"fixture-incarnation"},
                "evidence":[declaration.id]}),
            std::slice::from_ref(&declaration.id),
            None,
        )
        .unwrap();
        fixture.source.finish(&tx).unwrap();
        assert_eq!(fixture.source.doctor_line(&tx)["status"], "warn");
        tx.commit().unwrap();
        drop(writer);
        fixture
    }
    fn edit(&self, change: impl FnOnce(&Transaction<'_>)) {
        let mut writer = self.store.connection.write();
        let tx = writer.transaction().unwrap();
        change(&tx);
        self.source.finish(&tx).unwrap();
        tx.commit().unwrap();
    }
    fn line(&self) -> Value {
        let mut writer = self.store.connection.write();
        let tx = writer.transaction().unwrap();
        self.source.doctor_line(&tx)
    }
    fn measured(&self, change: impl FnOnce(&Transaction<'_>)) -> SqliteWork {
        let mut writer = self.store.connection.write();
        let tx = writer.transaction().unwrap();
        let work = SqliteWorkScope::start();
        change(&tx);
        self.source.finish(&tx).unwrap();
        let measured = work.finish();
        tx.commit().unwrap();
        measured
    }
    fn populate_foreign(&self, n: usize) {
        let mut writer = self.store.connection.write();
        let tx = writer.transaction().unwrap();
        tx.execute("INSERT INTO batches(id,origin,replica_sequence,accepted_at_unix_ms) VALUES('foreign','node',999,'1')",[]).unwrap();
        tx.execute("WITH RECURSIVE rows(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM rows WHERE n<?1)
            INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
            SELECT 'foreign/'||n,'foreign','exec/unrelated/'||n,'runtime.observed','node','{}','[]','1' FROM rows WHERE n<=?1",
            [n]).unwrap();
        tx.commit().unwrap();
    }
}

#[test]
fn native_gate_derives_eight_native_facts_and_preserves_canonical_selection() {
    let f = Fixture::seeded();
    f.edit(|tx| {
        // A later arrival with an older canonical time must not replace the selected exit.
        tx.execute("INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
            SELECT 'late-old-observation',batch_id,subject,kind,origin,?1,'[]','0' FROM claims
            WHERE subject='exec/probe' AND kind='runtime.observed'",
            [json!({"fields":{"status":"exited","exit_code":3,"incarnation_id":"older"},"evidence":[]}).to_string()]).unwrap();
    });
    let mut writer = f.store.connection.write();
    let tx = writer.transaction().unwrap();
    let facts = f.source.extract(&tx).unwrap().unwrap();
    let claim_id = facts["observed"]["claim"].as_str().unwrap();
    let key = smallclaims::store::canonical::claim_key(&tx, claim_id).unwrap();
    let native = crate::store::latest_claim_of_kind_tx(&tx, "exec/probe", "runtime.observed")
        .unwrap()
        .unwrap();
    assert_eq!(native.id, claim_id);
    assert_ne!(claim_id, "late-old-observation");
    assert_eq!(key.5, claim_id);
    assert_eq!(facts["desired"]["step"], "step-run/one/prepare");
    assert_eq!(facts["run"]["generation"], facts["generation"]["id"]);
    let text = witness(&Value::String(facts.to_string()))
        .unwrap()
        .into_witness()
        .unwrap();
    assert_eq!(f.source.doctor_line(&tx)["message"], text);
    assert!(text.contains("exit code 2") && text.contains("will not restart"));
}

#[test]
fn native_gate_action_domain_refusal_and_recovery_preserve_unknown() {
    let f = Fixture::seeded();
    for kind in NON_SELECTING {
        f.edit(|tx| {
            // Existing native record writer; deliberately minimal raw action fixture inputs.
            crate::store::append_claim_record_tx(
                tx,
                "node",
                "exec/probe",
                kind,
                None,
                &json!({"fields":{"status":"exited"}}),
                &[],
                None,
            )
            .unwrap();
        });
        assert_eq!(f.line()["status"], "warn", "{kind}");
    }
    f.edit(|tx| {
        crate::store::append_claim_record_tx(
            tx,
            "node",
            "exec/probe",
            "runtime.action.unlisted",
            None,
            &json!({"fields":{}}),
            &[],
            None,
        )
        .unwrap();
    });
    assert_eq!(f.line()["status"], "unknown");
    f.edit(|tx| {
        tx.execute(
            "DELETE FROM claims WHERE subject='exec/probe' AND kind='runtime.action.unlisted'",
            [],
        )
        .unwrap();
    });
    assert_eq!(f.line()["status"], "warn");
}

#[test]
fn native_gate_changed_declaration_and_generation_retract_without_read_repair() {
    let f = Fixture::seeded();
    f.edit(|tx| {
        tx.execute(
            "UPDATE desired SET owner_generation='run-generation/other' WHERE subject='exec/probe'",
            [],
        )
        .unwrap();
    });
    assert_eq!(f.line()["status"], "unknown");
    f.edit(|tx| {
        tx.execute(
            "UPDATE desired SET owner_generation='run-generation/one' WHERE subject='exec/probe'",
            [],
        )
        .unwrap();
    });
    assert_eq!(f.line()["status"], "warn");
    f.edit(|tx| {
        tx.execute(
            "UPDATE mission_runs SET current_generation_id='other' WHERE id='one'",
            [],
        )
        .unwrap();
    });
    assert_eq!(f.line()["status"], "unknown");
}

#[test]
fn native_gate_caps_complete_subject_and_related_batch_rows() {
    for subject in ["exec/probe", "exec/same-batch"] {
        let f = Fixture::seeded();
        f.edit(|tx| {
            let batch:String=tx.query_row("SELECT batch_id FROM claims WHERE subject='exec/probe' AND kind='runtime.observed'",[],|r|r.get(0)).unwrap();
            for n in 0..17 {
                tx.execute("INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
                    VALUES(?1,?2,?3,'harness.observed','node','{}','[]','1')",
                    params![format!("overflow/{n}"),batch,subject]).unwrap();
            }
        });
        let line = f.line();
        assert_eq!(line["status"], "unknown");
        assert!(line["message"].as_str().unwrap().contains("cap"));
    }
}

#[test]
fn native_gate_octet_header_guard_refuses_oversize_and_proves_bundled_opcodes() {
    let f = Fixture::seeded();
    let mut writer = f.store.connection.write();
    let version: String = writer
        .query_row("SELECT sqlite_version()", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, "3.46.0", "bundled libsqlite3-sys 0.30.1 version");
    let tx = writer.transaction().unwrap();
    let opcodes = tx
        .prepare("EXPLAIN SELECT typeof(body),octet_length(body) FROM claims WHERE rowid=?1")
        .unwrap()
        .query_map([1], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(6)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    // The actual bundled sqlite3.c defines OPFLAG_BYTELENARG = 0xc0.
    assert!(
        opcodes
            .iter()
            .any(|(op, p5)| op == "Column" && (*p5 & 0xc0) == 0xc0),
        "{opcodes:?}"
    );
    tx.execute(
        "UPDATE claims SET body=?1 WHERE subject='exec/probe' AND kind='runtime.observed'",
        ["x".repeat(RAW_BYTES + 1)],
    )
    .unwrap();
    let scope = SqliteWorkScope::start();
    let rowid = point(&tx, "claims", "id", &{
        tx.query_row(
            "SELECT id FROM claims WHERE subject='exec/probe' AND kind='runtime.observed'",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
    })
    .unwrap()
    .unwrap();
    let mut bytes = 0;
    assert!(bounded_row(&tx, "claims", rowid, &[("body", RAW_BYTES)], &mut bytes).is_err());
    let work = scope.finish();
    assert_eq!(bytes, 0);
    assert_eq!(work.fullscan_steps, 0);
    assert!(work.vm_steps > 0 && work.vm_steps < 200);
    f.source.finish(&tx).unwrap();
    assert_eq!(f.source.doctor_line(&tx)["status"], "unknown");
}

#[test]
fn native_gate_malformed_deep_and_wrong_type_source_is_visible_unknown() {
    for body in [
        Value::String("{broken".into()),
        Value::String(format!("{}0{}", "[".repeat(17), "]".repeat(17))),
        Value::Null,
    ] {
        let f = Fixture::seeded();
        f.edit(|tx| {
            if let Some(raw)=body.as_str() {
                tx.execute("UPDATE claims SET body=?1 WHERE subject='exec/probe' AND kind='runtime.observed'",[raw]).unwrap();
            } else {
                tx.execute("UPDATE claims SET body=x'00' WHERE subject='exec/probe' AND kind='runtime.observed'",[]).unwrap();
            }
        });
        assert_eq!(f.line()["status"], "unknown");
        assert_ne!(f.line()["status"], "pass");
    }
}

#[test]
fn native_gate_replace_on_each_protected_table_stays_pending_without_wrapper() {
    for (table, predicate) in [
        ("desired", "subject='exec/probe'"),
        ("mission_revisions", "mission_id='one'"),
        ("mission_definitions", "mission_id='one'"),
        ("mission_runs", "id='one'"),
        ("run_generations", "id='one'"),
        ("step_runs", "subject='step-run/one/prepare'"),
        ("claims", "subject='exec/probe' AND kind='runtime.observed'"),
        (
            "batches",
            "id=(SELECT batch_id FROM claims WHERE subject='exec/probe' AND kind='runtime.observed')",
        ),
    ] {
        let f = Fixture::seeded();
        let mut writer = f.store.connection.write();
        writer
            .execute_batch("PRAGMA recursive_triggers=OFF")
            .unwrap();
        let tx = writer.transaction().unwrap();
        tx.execute(
            &format!("INSERT OR REPLACE INTO {table} SELECT * FROM {table} WHERE {predicate}"),
            [],
        )
        .unwrap();
        assert!(f.source.state(&tx).unwrap().1, "{table}");
        assert!(f.source.installer.root(&tx, VIEW).is_err(), "{table}");
        assert_eq!(f.source.doctor_line(&tx)["status"], "unknown", "{table}");
    }
    // These tables are outside the local-only policy. Their initial insert already fences;
    // reset ONLY the fixture pending bit to isolate replacement capture, never readiness.
    for (table, insertion) in [
        ("replica_records", "INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,raw,state,updated_at_unix_ms)
            VALUES('private-record','node',1,'private-envelope',0,x'00','repaired','1')"),
        ("checkpoint_claims", "INSERT INTO checkpoint_claims(id,writer,sequence,envelope_hash,subject,kind,predecessors,accepted_at_unix_ms,checkpoint)
            VALUES('private-dropped','node',1,'private-envelope','exec/probe','runtime.observed','[]',1,'private-checkpoint')"),
        ("checkpoint_envelopes", "INSERT INTO checkpoint_envelopes(writer,sequence,envelope_hash,accepted_at_unix_ms,checkpoint)
            VALUES('node',1,'private-envelope',1,'private-checkpoint')"),
    ] {
        let f = Fixture::seeded();
        let mut writer = f.store.connection.write();
        writer.execute_batch("PRAGMA recursive_triggers=OFF").unwrap();
        let tx = writer.transaction().unwrap();
        tx.execute(insertion, []).unwrap();
        assert!(f.source.state(&tx).unwrap().2.is_some(), "{table}");
        tx.execute("UPDATE test_native_terminal_guard SET pending=0 WHERE id=1", []).unwrap();
        let revision = f.source.state(&tx).unwrap().0;
        tx.execute(&format!("INSERT OR REPLACE INTO {table} SELECT * FROM {table}"), []).unwrap();
        let after = f.source.state(&tx).unwrap();
        assert!(after.1 && after.0 > revision && after.2.is_some(), "{table}");
        assert!(f.source.installer.root(&tx, VIEW).is_err(), "{table}");
        assert_eq!(f.source.doctor_line(&tx)["status"], "unknown", "{table}");
    }
}

#[test]
fn native_gate_rowid_collision_after_extraction_refuses_final_publication() {
    let f = Fixture::seeded();
    let mut writer = f.store.connection.write();
    writer
        .execute_batch("PRAGMA recursive_triggers=OFF")
        .unwrap();
    let tx = writer.transaction().unwrap();
    let before = f.source.state(&tx).unwrap().0;
    let _facts = f.source.extract(&tx).unwrap().unwrap();
    // Same claim id / different subject: DELETE triggers cannot be relied upon.
    tx.execute("INSERT OR REPLACE INTO claims
        SELECT store_index,id,batch_id,'exec/replaced',kind,origin,actor,body,predecessors,accepted_at_unix_ms
        FROM claims WHERE subject='exec/probe' AND kind='runtime.observed'",[]).unwrap();
    assert!(f.source.state(&tx).unwrap().0 > before);
    assert_eq!(f.source.doctor_line(&tx)["status"], "unknown");
    assert!(f.source.installer.root(&tx, VIEW).is_err());
}

#[test]
fn native_gate_transaction_rollback_restores_source_root_and_witness() {
    let f = Fixture::seeded();
    let mut writer = f.store.connection.write();
    let tx = writer.transaction().unwrap();
    let root = f.source.installer.root(&tx, VIEW).unwrap();
    let capture = f.source.state(&tx).unwrap();
    tx.execute_batch("SAVEPOINT native_rollback").unwrap();
    tx.execute(
        "UPDATE step_runs SET status='cancelled' WHERE subject='step-run/one/prepare'",
        [],
    )
    .unwrap();
    f.source.finish(&tx).unwrap();
    assert_eq!(f.source.doctor_line(&tx)["status"], "unknown");
    tx.execute_batch("ROLLBACK TO native_rollback;RELEASE native_rollback")
        .unwrap();
    assert_eq!(f.source.state(&tx).unwrap(), capture);
    assert_eq!(f.source.installer.root(&tx, VIEW).unwrap(), root);
    assert_eq!(f.source.doctor_line(&tx)["status"], "warn");
}

#[test]
fn native_gate_repair_checkpoint_owned_set_and_lifecycle_gaps_never_pass() {
    for family in [
        "repair",
        "checkpoint",
        "owned-set",
        "rebuild",
        "migrate",
        "restore",
    ] {
        let f = Fixture::seeded();
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        match family {
            "owned-set" => {
                crate::store::append_claim_record_tx(
                    &tx,
                    "node",
                    "owned-set/private",
                    "owned-set.revised",
                    None,
                    &json!({}),
                    &[],
                    None,
                )
                .unwrap();
            }
            "repair" => {
                // A raw repair/rank insertion into the private graph is outside LOCAL v0.
                tx.execute("UPDATE test_native_terminal_guard SET gap='unsupported repair',pending=1 WHERE id=1",[]).unwrap();
            }
            _ => {
                // Explicit fixture lifecycle fence; no production dispatch is claimed.
                tx.execute(
                    "UPDATE test_native_terminal_guard SET gap=?1,pending=1 WHERE id=1",
                    [format!("unsupported {family}")],
                )
                .unwrap();
            }
        }
        f.source.finish(&tx).unwrap();
        assert_eq!(f.source.doctor_line(&tx)["status"], "unknown", "{family}");
        assert!(f.source.installer.root(&tx, VIEW).is_err());
    }
}

#[test]
fn native_gate_unrelated_and_noop_work_at_zero_1024_and_100000_rows() {
    for n in [0, 1024, 100_000] {
        let f = Fixture::seeded();
        if n > 0 {
            f.populate_foreign(n);
        }
        {
            let writer = f.store.connection.write();
            writer
                .execute_batch(
                    "CREATE TEMP TABLE witness_writes(n INTEGER NOT NULL);
                INSERT INTO witness_writes VALUES(0);",
                )
                .unwrap();
            for table in ["test_terminal_members", "test_terminal_meta"] {
                for event in ["INSERT", "UPDATE", "DELETE"] {
                    writer
                        .execute_batch(&format!(
                            "CREATE TEMP TRIGGER writes_{table}_{event}
                        AFTER {event} ON main.{table} BEGIN UPDATE witness_writes SET n=n+1; END;"
                        ))
                        .unwrap();
                }
            }
        }
        let before = f.store.connection.write().total_changes();
        let work = f.measured(|_| {});
        assert_eq!(work.statements, 1, "{n}: {work:?}");
        assert_eq!(work.fullscan_steps, 0, "{n}: {work:?}");
        assert!(work.vm_steps > 0 && work.vm_steps <= 40, "{n}: {work:?}");
        assert_eq!(f.store.connection.write().total_changes(), before);
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        let root = f.source.installer.root(&tx, VIEW).unwrap();
        let capture = f.source.state(&tx).unwrap();
        tx.commit().unwrap();
        drop(writer);
        // At n=0 this is a genuine absent-key point operation; the larger sources update
        // one unrelated row. Neither may capture a source change or write a witness.
        let unrelated = f.measured(|tx| {
            tx.execute("UPDATE claims SET body=body WHERE id='foreign/1'", [])
                .unwrap();
        });
        eprintln!("native source unrelated path rows={n}: {unrelated:?}");
        assert!(
            unrelated.statements > 0 && unrelated.statements <= 96 && unrelated.vm_steps <= 20_000,
            "{n}: {unrelated:?}"
        );
        assert_eq!(unrelated.fullscan_steps, 0, "{n}: {unrelated:?}");
        {
            let mut writer = f.store.connection.write();
            let tx = writer.transaction().unwrap();
            assert_eq!(f.source.state(&tx).unwrap(), capture);
            assert_eq!(f.source.installer.root(&tx, VIEW).unwrap(), root);
        }
        let work=f.measured(|tx|{
            tx.execute("UPDATE claims SET body=body WHERE subject='exec/probe' AND kind='runtime.observed'",[]).unwrap();
        });
        eprintln!("native source full no-op path unrelated={n}: {work:?}");
        assert!(
            work.statements > 0 && work.statements <= 96 && work.vm_steps <= 20_000,
            "{n}: {work:?}"
        );
        assert_eq!(work.fullscan_steps, 0, "{n}: {work:?}");
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        let after = f.source.installer.root(&tx, VIEW).unwrap();
        assert!(after.revision > root.revision);
        assert_eq!(after.generation, root.generation);
        let output_writes: i64 = tx
            .query_row("SELECT n FROM witness_writes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(output_writes, 0);
        assert_eq!(f.source.doctor_line(&tx)["status"], "warn");
    }
}

#[test]
fn native_gate_raw_reopen_mutation_and_nonempty_attachment_refuse() {
    let f = Fixture::seeded();
    let raw = Connection::open(f.dir.path().join("claims.sqlite3")).unwrap();
    raw.execute(
        "UPDATE step_runs SET status='cancelled' WHERE subject='step-run/one/prepare'",
        [],
    )
    .unwrap();
    assert_eq!(f.line()["status"], "unknown");
    let mut writer = f.store.connection.write();
    let tx = writer.transaction().unwrap();
    assert!(f.source.installer.root(&tx, VIEW).is_err());
    assert!(NativeSource::attach(&tx, f.source.binding.clone()).is_err());
}

#[test]
fn native_gate_guard_binding_replacement_is_refused_or_sticky_unknown() {
    let f = Fixture::seeded();
    let mut writer = f.store.connection.write();
    let tx = writer.transaction().unwrap();
    assert!(tx.execute("INSERT OR REPLACE INTO test_native_terminal_guard SELECT * FROM test_native_terminal_guard", []).is_err());
    assert_eq!(f.source.doctor_line(&tx)["status"], "warn");
    tx.execute(
        "UPDATE test_native_terminal_guard SET exec='exec/replaced' WHERE id=1",
        [],
    )
    .unwrap();
    f.source.finish(&tx).unwrap();
    assert_eq!(f.source.doctor_line(&tx)["status"], "unknown");
    tx.execute(
        "UPDATE test_native_terminal_guard SET exec='exec/probe' WHERE id=1",
        [],
    )
    .unwrap();
    f.source.finish(&tx).unwrap();
    assert_eq!(f.source.doctor_line(&tx)["status"], "unknown");
    assert!(f.source.installer.root(&tx, VIEW).is_err());
}

#[test]
fn native_gate_guard_inventory_and_default_store_have_no_public_wiring() {
    let f = Fixture::seeded();
    let default = Store::open_memory("unregistered").unwrap();
    let mut writer = default.connection.write();
    let tx = writer.transaction().unwrap();
    let any:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name LIKE 'test_native_%' OR name LIKE 'test_terminal_%')",
        [],|r|r.get(0)).unwrap();
    assert!(!any);
    let mut writer = f.store.connection.write();
    let tx = writer.transaction().unwrap();
    let tables=tx.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'test_%' ORDER BY name")
        .unwrap().query_map([],|r|r.get::<_,String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    assert_eq!(
        tables,
        [
            "test_native_terminal_guard",
            "test_terminal_members",
            "test_terminal_meta"
        ]
    );
    let before = tx.total_changes();
    drop(tx);
    drop(writer);
    assert_eq!(f.line()["status"], "warn");
    assert_eq!(f.store.connection.write().total_changes(), before);
}
