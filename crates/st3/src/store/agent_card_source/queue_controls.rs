//! Provisional Kernel scheduling/reclamation controls. The opaque token comes from
//! Installer.scan, never a published root or a manually set readiness bit.
use super::*;

fn empty_context() -> (Store, Namespace, Kernel, u128) {
    let store = Store::open_memory("node").unwrap();
    let ns = tests::context(&store);
    let at = tests::clock(&store);
    let kernel = Kernel::new("node");
    // Only the captured maintenance clock is needed by these dependency controls.
    // This deliberately does not claim complete graph/source coverage.
    let clock = tests::capture(&store)
        .into_iter()
        .find(|m| key_parts(&m.key).unwrap().0 == "local_agent_card_clock")
        .unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    kernel.apply(&tx, &ns, &[clock]).unwrap();
    tx.commit().unwrap();
    drop(writer);
    (store, ns, kernel, at)
}

fn queue_fixture(store: &Store, count: usize) -> MissionRunView {
    let mut source = String::from(
        "version 2\nmission \"queue-control\" state=\"ready\" { goal \"Invented bounded queue fixture\";\n",
    );
    for n in 0..count {
        source.push_str(&format!(
            "step \"item-{n:03}\" {{ assigned-to \"agent/queue-control/{n:03}\"; goal \"Queue item\"; }}\n"
        ));
    }
    source.push_str("}\n");
    let intent = crate::parse_intent(&source, store.origin()).unwrap();
    store
        .apply_internal(&intent, "queue-control-publish")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "queue-control".into(),
            revision: None,
            workspace: "/fixture/queue-control".into(),
            requester: Some("person/fixture".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "queue-control-start".into(),
        })
        .unwrap();
    for step in &run.steps {
        store.set_step_state(&step.subject, "ready", None).unwrap();
    }
    run
}

fn replace_projected_queue(store: &Store, ns: &Namespace) {
    // Full extraction is a test oracle only; production capture remains owner-owned.
    let rows = tests::capture(store);
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    for m in rows {
        let (table, pk) = key_parts(&m.key).unwrap();
        let coverage = match table.as_str() {
            "mission_runs" => {
                agent_queue::replace_run(&tx, ns, pk[0].as_str().unwrap(), m.new.as_ref())
            }
            "step_runs" => {
                agent_queue::replace_step(&tx, ns, pk[0].as_str().unwrap(), m.new.as_ref())
            }
            _ => continue,
        }
        .unwrap();
        assert_eq!(coverage, agent_queue::Coverage::Complete);
    }
    tx.commit().unwrap();
}

fn close_queue(store: &Store, ns: &Namespace, at: u128) {
    for _ in 0..4096 {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        let page = agent_queue::drain(&tx, ns, at, WORK).unwrap();
        assert!(page.processed <= WORK);
        assert_eq!(page.coverage, agent_queue::Coverage::Complete);
        tx.commit().unwrap();
        if page.clean {
            return;
        }
    }
    panic!("fixture queue computation did not close");
}

fn queue_dirty(c: &Connection, ns: &Namespace) -> usize {
    c.query_row(
        "SELECT count(*) FROM local_agent_queue_dirty WHERE namespace=?1",
        [ns.as_str()],
        |r| r.get(0),
    )
    .unwrap()
}

fn assert_unpublished(c: &Connection, ns: &Namespace) {
    assert!(
        !c.query_row(
            "SELECT EXISTS(SELECT 1 FROM ivm_install_roots WHERE namespace=?1 AND ready=1)",
            [ns.as_str()],
            |r| r.get::<_, bool>(0),
        )
        .unwrap()
    );
}

#[test]
fn more_than_two_queue_pages_converge_and_removed_assignees_are_acknowledged() {
    let (store, ns, kernel, _) = empty_context();
    let run = queue_fixture(&store, 2 * WORK + 1);
    let at = tests::clock(&store);
    // Advance the provisional namespace with the actual captured clock after native writes.
    let clock = tests::capture(&store)
        .into_iter()
        .find(|m| key_parts(&m.key).unwrap().0 == "local_agent_card_clock")
        .unwrap();
    {
        let mut w = store.connection.write();
        let tx = w.transaction().unwrap();
        kernel.apply(&tx, &ns, &[clock]).unwrap();
        tx.commit().unwrap();
    }
    replace_projected_queue(&store, &ns);
    close_queue(&store, &ns, at);
    let agents: Vec<String> = run
        .steps
        .iter()
        .map(|s| s.assigned_to.clone().unwrap())
        .collect();
    assert_eq!(agents.len(), 2 * WORK + 1);
    assert_eq!(queue_dirty(&store.readers.get(), &ns), agents.len());
    assert_eq!(
        agent_queue::rows(&store.readers.get(), &ns, &agents, at).unwrap(),
        store.agent_work_queues().unwrap(),
        "captured queue inputs must match the actual Store queue oracle"
    );
    for turn in 0..8 {
        let mut w = store.connection.write();
        let tx = w.transaction().unwrap();
        assert_unpublished(&tx, &ns);
        kernel.maintain(&tx, &ns).unwrap();
        if turn == 0 {
            assert_eq!(queue_dirty(&tx, &ns), agents.len());
            assert_eq!(tx.query_row(
                "SELECT count(*) FROM local_agent_card_source_work WHERE namespace=?1 AND kind='card'",
                [ns.as_str()], |r| r.get::<_, usize>(0)
            ).unwrap(), WORK);
            assert!(!kernel.families_closed(&tx, &ns, at).unwrap());
            assert!(kernel.validate_publication(&tx, &ns).is_err());
        }
        let done = queue_dirty(&tx, &ns) == 0 && !card_work(&tx, &ns).unwrap();
        tx.commit().unwrap();
        if done {
            break;
        }
    }
    let c = store.readers.get();
    assert_eq!(
        queue_dirty(&c, &ns),
        0,
        "staged first-page IDs must not starve later IDs"
    );
    assert!(!card_work(&c, &ns).unwrap());
    assert_eq!(c.query_row(
        "SELECT count(*) FROM local_agent_card_rows WHERE namespace=?1 AND (body IS NOT NULL OR current=1)",
        [ns.as_str()], |r| r.get::<_, usize>(0)
    ).unwrap(), 0, "undeclared assignees must remain absent after committed acknowledgements");
    drop(c);

    // Native terminal transition removes every queue item. Public absence acknowledgements
    // still need a second multi-page pass, even though the queue computation becomes empty.
    store
        .set_mission_run_state(&run.id, "cancelled", "terminal", Some("fixture cleanup"))
        .unwrap();
    replace_projected_queue(&store, &ns);
    close_queue(&store, &ns, at);
    assert_eq!(queue_dirty(&store.readers.get(), &ns), agents.len());
    tests::drain(&store, &ns, &kernel);
    let c = store.readers.get();
    assert!(agent_queue::rows(&c, &ns, &agents, at).unwrap().is_empty());
    assert!(store.agent_work_queues().unwrap().is_empty());
    assert_eq!(queue_dirty(&c, &ns), 0);
    assert!(!card_work(&c, &ns).unwrap());
    assert_unpublished(&c, &ns);
    assert!(
        kernel
            .validate_publication(&store.connection.write().transaction().unwrap(), &ns)
            .is_err()
    );
}

#[test]
fn absent_assignee_ack_and_row_removal_roll_back_together() {
    let (store, ns, kernel, at) = empty_context();
    let run = queue_fixture(&store, 1);
    replace_projected_queue(&store, &ns);
    close_queue(&store, &ns, at);
    let agent = run.steps[0].assigned_to.as_ref().unwrap();
    let mut w = store.connection.write();
    {
        let tx = w.transaction().unwrap();
        queue(&tx, &ns, "card", agent).unwrap();
        // A retained former public row is deliberately inserted only as cleanup input.
        // No malformed body is read, formatted, or certified by this fixture.
        tx.execute("INSERT INTO local_agent_card_rows VALUES(?1,?2,'former','running',1,1,'{}','fixture',0,'{}')", params![ns.as_str(),agent]).unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = w.transaction().unwrap();
        assert!(kernel.materialize(&tx, &ns, agent, at, 0).unwrap());
        assert_eq!(queue_dirty(&tx, &ns), 0);
        assert!(!card_work(&tx, &ns).unwrap());
        assert!(tx.query_row("SELECT body IS NULL AND current=0 FROM local_agent_card_rows WHERE namespace=?1 AND agent=?2", params![ns.as_str(),agent],|r|r.get::<_,bool>(0)).unwrap());
        tx.rollback().unwrap();
    }
    assert_eq!(queue_dirty(&w, &ns), 1);
    assert!(card_work(&w, &ns).unwrap());
    assert!(w.query_row("SELECT current=1 AND body IS NOT NULL FROM local_agent_card_rows WHERE namespace=?1 AND agent=?2", params![ns.as_str(),agent],|r|r.get::<_,bool>(0)).unwrap());
    let tx = w.transaction().unwrap();
    assert!(kernel.materialize(&tx, &ns, agent, at, 0).unwrap());
    tx.commit().unwrap();
    assert_eq!(queue_dirty(&w, &ns), 0);
    assert!(!card_work(&w, &ns).unwrap());
    assert_unpublished(&w, &ns);
}

fn namespace_tables(c: &Connection) -> Vec<String> {
    let names: Vec<String> = c.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name LIKE 'local_%' ORDER BY name").unwrap()
        .query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    names
        .into_iter()
        .filter(|table| {
            c.prepare(&format!("PRAGMA table_info({table})"))
                .unwrap()
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .any(|column| column.unwrap() == "namespace")
        })
        .collect()
}

fn inventory(c: &Connection, ns: &Namespace, tables: &[String]) -> BTreeMap<String, usize> {
    tables
        .iter()
        .map(|table| {
            (
                table.clone(),
                c.query_row(
                    &format!("SELECT count(*) FROM {table} WHERE namespace=?1"),
                    [ns.as_str()],
                    |r| r.get(0),
                )
                .unwrap(),
            )
        })
        .collect()
}

fn seed_reclaim_families(tx: &Transaction<'_>, ns: &Namespace) {
    // These are private deletion witnesses only, never source/certificate inputs. Every
    // top-level reclaim family is nonempty so completion cannot hide a second full budget.
    for sql in [
        "INSERT INTO local_agent_source_dirty VALUES(?1,'fixture')",
        "INSERT INTO local_agent_owned_state VALUES(?1,0,0)",
        "INSERT INTO local_agent_queue_dirty VALUES(?1,'agent/fixture')",
        "INSERT INTO local_agent_authority_fences VALUES(?1,'agent/fixture','fixture')",
        "INSERT INTO local_agent_lifecycle_fences VALUES(?1,'agent/fixture','fixture')",
        "INSERT INTO local_agent_launch_fences VALUES(?1,'fixture')",
        "INSERT INTO local_agent_source_boundary VALUES(?1,'fixture-not-a-certificate',0,0,0,0,0,0,0,'{}')",
        "INSERT INTO local_agent_card_source_work VALUES(?1,'card','agent/fixture')",
        "INSERT INTO local_agent_card_source_cursor VALUES(?1,'fixture','')",
    ] {
        tx.execute(sql, [ns.as_str()]).unwrap();
    }
}

#[test]
fn reclamation_shares_one_delete_budget_across_all_families_and_continuation() {
    let store = Store::open_memory("node").unwrap();
    let ns = tests::context(&store);
    let other = tests::context_for(&store, "fixture.agent-card.queue-controls.other");
    assert_ne!(ns.as_str(), other.as_str());
    let kernel = Kernel::new("node");
    let mut w = store.connection.write();
    let tables = namespace_tables(&w);
    w.execute_batch("CREATE TEMP TABLE queue_control_deletions(table_name TEXT,namespace TEXT)")
        .unwrap();
    // Count actual DELETEs, including trigger cascades and the final continuation row.
    // Count deltas alone would miss deletes paired with inserts in the same callback.
    for (i, table) in tables.iter().enumerate() {
        w.execute_batch(&format!("CREATE TEMP TRIGGER queue_control_delete_{i} AFTER DELETE ON main.{table} BEGIN INSERT INTO queue_control_deletions VALUES('{table}',OLD.namespace); END;")).unwrap();
    }
    {
        let tx = w.transaction().unwrap();
        seed_reclaim_families(&tx, &ns);
        seed_reclaim_families(&tx, &other);
        tx.commit().unwrap();
    }
    let other_before = inventory(&w, &other, &tables);
    let initial = inventory(&w, &ns, &tables);
    // Reclaim rollback must restore both the family rows and its durable phase.
    {
        let tx = w.transaction().unwrap();
        assert!(!kernel.reclaim(&tx, &ns, 1).unwrap());
        assert_eq!(
            tx.query_row("SELECT count(*) FROM queue_control_deletions", [], |r| {
                r.get::<_, usize>(0)
            })
            .unwrap(),
            1
        );
        tx.rollback().unwrap();
    }
    assert_eq!(inventory(&w, &ns, &tables), initial);
    let mut finished = false;
    let mut visited = BTreeSet::new();
    for _ in 0..64 {
        let tx = w.transaction().unwrap();
        assert_unpublished(&tx, &ns);
        let phase: u64 = tx
            .query_row(
                "SELECT phase FROM local_agent_card_source_reclaim WHERE namespace=?1",
                [ns.as_str()],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
            .unwrap_or(0);
        visited.insert(phase);
        tx.execute("DELETE FROM queue_control_deletions", [])
            .unwrap();
        finished = kernel.reclaim(&tx, &ns, 1).unwrap();
        let deleted: usize = tx
            .query_row("SELECT count(*) FROM queue_control_deletions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            deleted <= 1,
            "phase {phase} deleted {deleted} rows with a total budget of one"
        );
        assert_eq!(
            inventory(&tx, &other, &tables),
            other_before,
            "other namespace changed in phase {phase}"
        );
        assert_eq!(
            tx.query_row(
                "SELECT count(*) FROM queue_control_deletions WHERE namespace<>?1",
                [ns.as_str()],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            0
        );
        if finished {
            assert_eq!(phase, 8);
            assert_eq!(
                deleted, 1,
                "final continuation removal gets its own callback budget"
            );
        }
        tx.commit().unwrap();
        if finished {
            break;
        }
    }
    assert!(finished, "bounded reclamation failed to finish");
    assert_eq!(visited, (0..=8).collect());
    assert!(
        inventory(&w, &ns, &tables)
            .values()
            .all(|count| *count == 0)
    );
    assert_eq!(inventory(&w, &other, &tables), other_before);
}
