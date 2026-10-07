use super::*;
use proptest::prelude::*;
const NS: &str = "fixture/queue-source";
const AGENT: &str = "agent/birch";
fn run(number: usize) -> String {
    format!("mission-run/sample/{number}")
}
fn canonical(id: &str, at: u128) -> ClaimKey {
    (at, "cedar".into(), 0, "batch/sample".into(), 0, id.into())
}
#[derive(Default)]
struct Oracle {
    joins: BTreeMap<String, (u128, bool)>,
    moves: BTreeMap<String, (ClaimKey, QueueMove)>,
}
impl Oracle {
    fn join(&mut self, tx: &Transaction<'_>, number: usize, at: Option<(u128, bool)>) {
        let name = run(number);
        set_join(tx, NS, AGENT, &name, at).unwrap();
        if let Some(at) = at {
            self.joins.insert(name, at);
        } else {
            self.joins.remove(&name);
        }
    }
    fn movement(
        &mut self,
        tx: &Transaction<'_>,
        id: &str,
        at: u128,
        number: usize,
        placement: Placement,
        anchor: Option<usize>,
    ) {
        let key = canonical(id, at);
        let movement = QueueMove {
            run: run(number),
            placement,
            anchor: anchor.map(run),
            at_unix_ms: at,
        };
        set_move(tx, NS, AGENT, id, Some((&key, &movement))).unwrap();
        self.moves.insert(id.into(), (key, movement));
    }
    fn retract(&mut self, tx: &Transaction<'_>, id: &str) {
        set_move(tx, NS, AGENT, id, None).unwrap();
        self.moves.remove(id);
    }
    fn expected(&self) -> Vec<String> {
        let joins = self
            .joins
            .iter()
            .map(|(run, (at, _))| QueueJoin {
                run: run.clone(),
                at_unix_ms: *at,
            })
            .collect::<Vec<_>>();
        let mut moves = self.moves.values().collect::<Vec<_>>();
        moves.sort_by(|a, b| a.0.cmp(&b.0));
        let moves = moves
            .into_iter()
            .map(|(_, movement)| movement.clone())
            .collect::<Vec<_>>();
        seat_queue::replay(&joins, &moves)
            .into_iter()
            .filter(|run| self.joins[run].1)
            .collect()
    }
    fn settle(&self, tx: &Transaction<'_>, limit: usize) -> usize {
        let mut total = 0;
        for _ in 0..20000 {
            let result = page(tx, NS, AGENT, limit).unwrap();
            assert!(result.processed <= limit);
            total += result.processed;
            if result.ready {
                assert_eq!(window(tx, NS, AGENT, 501).unwrap(), self.expected());
                return total;
            }
            assert!(window(tx, NS, AGENT, 501).is_err());
        }
        panic!("seat suffix did not settle");
    }
}
fn connection() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    create_schema(&connection).unwrap();
    connection
}

#[test]
fn canonical_ties_future_named_joins_unknowns_and_late_prerequisites() {
    let mut connection = connection();
    let tx = connection.transaction().unwrap();
    let mut source = Oracle::default();
    source.join(&tx, 0, Some((10, true)));
    source.join(&tx, 1, Some((50, true)));
    source.join(&tx, 2, Some((60, true)));
    source.join(&tx, 3, Some((5, true)));
    source.movement(&tx, "late", 12, 1, Placement::After, Some(2));
    source.movement(&tx, "second", 12, 3, Placement::Bottom, None);
    source.movement(&tx, "unknown", 8, 4, Placement::Top, None);
    source.movement(&tx, "self", 9, 0, Placement::Before, Some(0));
    source.movement(&tx, "missing-anchor", 11, 0, Placement::After, Some(5));
    source.settle(&tx, 1);
    source.join(&tx, 4, Some((90, true)));
    source.join(&tx, 5, Some((3, true)));
    source.settle(&tx, 2);
    source.movement(&tx, "aaa-earlier-tie", 12, 0, Placement::After, Some(3));
    source.settle(&tx, 1);
    source.retract(&tx, "late");
    source.settle(&tx, 1);
    source.join(&tx, 2, None);
    source.settle(&tx, 1);
    source.join(&tx, 1, Some((1, true)));
    source.settle(&tx, 1);
    source.join(&tx, 4, Some((90, false)));
    source.settle(&tx, 1);
    tx.commit().unwrap();
}

#[test]
fn append_moves_touch_one_node_and_duplicates_touch_none() {
    let mut connection = connection();
    let tx = connection.transaction().unwrap();
    let mut source = Oracle::default();
    for number in 0..100 {
        source.join(&tx, number, Some((number as u128, true)));
    }
    source.settle(&tx, 31);
    tx.execute_batch("CREATE TEMP TABLE writes(n INTEGER); INSERT INTO writes VALUES(0);
 CREATE TEMP TRIGGER witness_insert AFTER INSERT ON local_seat_order_nodes BEGIN UPDATE writes SET n=n+1; END;
 CREATE TEMP TRIGGER witness_update AFTER UPDATE ON local_seat_order_nodes BEGIN UPDATE writes SET n=n+1; END;
 CREATE TEMP TRIGGER witness_delete AFTER DELETE ON local_seat_order_nodes BEGIN UPDATE writes SET n=n+1; END;").unwrap();
    source.movement(&tx, "move", 1000, 70, Placement::Before, Some(5));
    assert_eq!(source.settle(&tx, 1), 1);
    assert_eq!(
        tx.query_row("SELECT n FROM writes", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let (key, movement) = &source.moves["move"];
    assert!(!set_move(&tx, NS, AGENT, "move", Some((key, movement))).unwrap());
    assert!(!set_join(&tx, NS, AGENT, &run(70), Some((70, true))).unwrap());
    assert_eq!(source.settle(&tx, 1), 0);
    assert_eq!(
        tx.query_row("SELECT n FROM writes", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    source.movement(&tx, "already-before", 1001, 70, Placement::Before, Some(5));
    assert_eq!(source.settle(&tx, 1), 1);
    assert_eq!(
        tx.query_row("SELECT n FROM writes", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn late_suffix_fences_bounded_window_and_rolls_back() {
    let mut connection = connection();
    let mut source = Oracle::default();
    {
        let tx = connection.transaction().unwrap();
        for number in 0..20 {
            source.join(&tx, number, Some((number as u128, true)));
        }
        for number in 0..20 {
            source.movement(
                &tx,
                &format!("move-{number}"),
                100 + number as u128,
                number,
                Placement::Top,
                None,
            );
        }
        source.settle(&tx, 8);
        tx.commit().unwrap();
    }
    let before = window(&connection, NS, AGENT, 501).unwrap();
    {
        let tx = connection.transaction().unwrap();
        let key = canonical("early", 1);
        let movement = QueueMove {
            run: run(15),
            placement: Placement::Top,
            anchor: None,
            at_unix_ms: 1,
        };
        set_move(&tx, NS, AGENT, "early", Some((&key, &movement))).unwrap();
        assert!(window(&tx, NS, AGENT, 501).is_err());
        let result = page(&tx, NS, AGENT, 1).unwrap();
        assert_eq!(result.processed, 1);
        assert!(!result.ready);
        assert!(window(&tx, NS, AGENT, 501).is_err());
        tx.rollback().unwrap();
    }
    assert_eq!(window(&connection, NS, AGENT, 501).unwrap(), before);
    let tx = connection.transaction().unwrap();
    source.settle(&tx, 1);
    source.movement(&tx, "early", 1, 15, Placement::Top, None);
    source.settle(&tx, 1);
    tx.commit().unwrap();
    assert_eq!(
        window(&connection, NS, AGENT, 4).unwrap(),
        source.expected()[..4]
    );
    assert!(window(&connection, NS, AGENT, 502).is_err());
    assert!(window(&connection, NS, "agent/other", 4).is_err());
}

#[test]
fn interrupted_suffix_reopens_without_read_repair() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("order.sqlite");
    let mut source = Oracle::default();
    {
        let mut connection = Connection::open(&path).unwrap();
        create_schema(&connection).unwrap();
        let tx = connection.transaction().unwrap();
        for number in 0..12 {
            source.join(&tx, number, Some((number as u128, true)));
        }
        for number in 0..12 {
            source.movement(
                &tx,
                &format!("move-{number}"),
                100 + number as u128,
                number,
                Placement::Bottom,
                None,
            );
        }
        source.settle(&tx, 8);
        source.movement(&tx, "early", 1, 10, Placement::Top, None);
        assert!(!page(&tx, NS, AGENT, 1).unwrap().ready);
        tx.commit().unwrap();
    }
    let mut connection = Connection::open(&path).unwrap();
    create_schema(&connection).unwrap();
    assert!(window(&connection, NS, AGENT, 501).is_err());
    let tx = connection.transaction().unwrap();
    source.settle(&tx, 2);
    tx.commit().unwrap();
    assert_eq!(
        window(&connection, NS, AGENT, 501).unwrap(),
        source.expected()
    );
}

#[test]
fn actual_query_plans_seek_source_suffix_and_live_positions() {
    let mut connection = connection();
    let tx = connection.transaction().unwrap();
    let mut source = Oracle::default();
    source.join(&tx, 0, Some((1, true)));
    source.settle(&tx, 1);
    let plans = [
        (
            NEXT,
            vec![
                rusqlite::types::Value::Text(NS.into()),
                rusqlite::types::Value::Text(AGENT.into()),
                rusqlite::types::Value::Blob(Vec::new()),
            ],
        ),
        (
            LAST_APPLIED,
            vec![
                rusqlite::types::Value::Text(NS.into()),
                rusqlite::types::Value::Text(AGENT.into()),
            ],
        ),
        (
            WINDOW,
            vec![
                rusqlite::types::Value::Text(NS.into()),
                rusqlite::types::Value::Text(AGENT.into()),
                rusqlite::types::Value::Integer(501),
            ],
        ),
    ];
    for (query, params) in plans {
        let plan = tx
            .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
            .unwrap()
            .query_map(rusqlite::params_from_iter(params), |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(plan.iter().any(|line| line.contains("SEARCH")), "{plan:?}");
        assert!(
            !plan
                .iter()
                .any(|line| line.contains("SCAN") || line.contains("TEMP B-TREE")),
            "{plan:?}"
        );
    }
}

proptest! {
 #![proptest_config(ProptestConfig::with_cases(64))]
 #[test]
 fn incremental_order_matches_full_replay_after_permuted_sources(
  joins in prop::collection::vec((0u8..8,0u8..15,any::<bool>()),1..20),
  moves in prop::collection::vec((0u8..8,0u8..15,0u8..4,0u8..10),1..30),
  reverse in any::<bool>(),
 ) {
  let mut connection=connection();let tx=connection.transaction().unwrap();let mut source=Oracle::default();
  let joins=if reverse {joins.into_iter().rev().collect::<Vec<_>>()} else {joins};
  for (number,at,live) in joins {source.join(&tx,number as usize,Some((at as u128,live)));source.settle(&tx,3);}
  let mut enumerated=moves.into_iter().enumerate().collect::<Vec<_>>();if reverse {enumerated.reverse();}
  for (id,(number,at,placement,anchor)) in enumerated {
   let placement=[Placement::Top,Placement::Bottom,Placement::Before,Placement::After][placement as usize];
   source.movement(&tx,&format!("event-{id}"),at as u128,number as usize,placement,Some(anchor as usize));source.settle(&tx,3);
  }
  for number in 0..8 {source.join(&tx,number,Some(((number*3) as u128,true)));source.settle(&tx,2);}
  for id in ["event-0","event-2","event-7"] {source.retract(&tx,id);source.settle(&tx,2);}
  prop_assert_eq!(window(&tx, NS,AGENT,501).unwrap(),source.expected());
 }
}

// Full capture is an independent real-Store test oracle only. Production must deliver
// affected old/new source keys through the shared owner's certified mutation worklists.
fn capture_store_oracle(store: &Store, agent: &str, previous: &mut Oracle) {
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    create_schema(&tx).unwrap();
    let inputs = seat_queue_inputs_tx(&tx, Some(agent))
        .unwrap()
        .remove(agent)
        .unwrap_or_default();
    let expected = inputs.live_order();
    let joins = inputs
        .joins
        .into_iter()
        .map(|join| {
            let live = inputs.live.contains(&join.run);
            (join.run, (join.at_unix_ms, live))
        })
        .collect::<BTreeMap<_, _>>();
    for run in previous
        .joins
        .keys()
        .filter(|run| !joins.contains_key(*run))
    {
        set_join(&tx, NS, agent, run, None).unwrap();
    }
    for (run, join) in &joins {
        set_join(&tx, NS, agent, run, Some(*join)).unwrap();
    }
    let moves = inputs
        .moves
        .into_iter()
        .map(|recorded| {
            let key = canonical::claim_key(&tx, &recorded.view.claim_id).unwrap();
            (recorded.view.claim_id, (key, recorded.movement))
        })
        .collect::<BTreeMap<_, _>>();
    for id in previous.moves.keys().filter(|id| !moves.contains_key(*id)) {
        set_move(&tx, NS, agent, id, None).unwrap();
    }
    for (id, (key, movement)) in &moves {
        set_move(&tx, NS, agent, id, Some((key, movement))).unwrap();
    }
    for _ in 0..20000 {
        let result = page(&tx, NS, agent, 2).unwrap();
        assert!(result.processed <= 2);
        if result.ready {
            assert_eq!(window(&tx, NS, agent, 501).unwrap(), expected);
            *previous = Oracle { joins, moves };
            tx.commit().unwrap();
            return;
        }
        assert!(window(&tx, NS, agent, 501).is_err());
    }
    panic!("real Store queue did not settle");
}

#[test]
fn real_store_moves_terminal_membership_and_permuted_replication() {
    use crate::store::tests::{exchange_from, receive_and_project};
    let source = Store::open_memory("cedar").unwrap();
    let intent = crate::parse_intent(
        "version 2\nmission \"queue-sample\" state=\"ready\" { concurrent-runs max=8; goal \"Prepare samples.\"; step \"build\" { assigned-to \"agent/worker\"; goal \"Build sample\"; } }\n",
        source.origin(),
    ).unwrap();
    source.apply_internal(&intent, "publish").unwrap();
    let mut runs = Vec::new();
    for number in 0..4 {
        runs.push(
            source
                .create_mission_run(&MissionRunRequest {
                    mission: "queue-sample".into(),
                    revision: None,
                    workspace: "/example/project".into(),
                    requester: Some("person/avery".into()),
                    mode: None,
                    inputs: BTreeMap::new(),
                    idempotency_key: format!("start-{number}"),
                })
                .unwrap(),
        );
    }
    let agent = runs[0].steps[0].assigned_to.as_deref().unwrap();
    let mut previous = Oracle::default();
    capture_store_oracle(&source, agent, &mut previous);
    for (id, run, placement, anchor) in [
        ("last-top", &runs[3], "top", None),
        (
            "second-after-last",
            &runs[1],
            "after",
            Some(runs[3].subject.clone()),
        ),
    ] {
        source
            .move_seat_queue_run(&SeatQueueMoveRequest {
                agent: agent.into(),
                run: run.subject.clone(),
                placement: placement.into(),
                anchor,
                reason: Some("invented fixture".into()),
                actor: "person/avery".into(),
                idempotency_key: id.into(),
            })
            .unwrap();
        capture_store_oracle(&source, agent, &mut previous);
    }
    source
        .set_mission_run_state(&runs[3].id, "cancelled", "terminal", None)
        .unwrap();
    capture_store_oracle(&source, agent, &mut previous);
    let exchange = exchange_from(&source, &ReplicationInventory::default());
    for reverse in [false, true] {
        let target = Store::open_memory("birch").unwrap();
        let mut exchange = exchange.clone();
        if reverse {
            exchange.envelopes.reverse();
        }
        receive_and_project(&target, "cedar", &exchange);
        let mut previous = Oracle::default();
        capture_store_oracle(&target, agent, &mut previous);
        assert_eq!(
            window(&target.readers.get(), NS, agent, 501).unwrap(),
            source.seat_run_order(agent).unwrap()
        );
        receive_and_project(&target, "cedar", &exchange);
        capture_store_oracle(&target, agent, &mut previous);
    }
}

#[test]
fn namespace_outputs_and_partial_repair_are_isolated() {
    let mut connection = connection();
    let tx = connection.transaction().unwrap();
    for namespace in ["root/a", "root/b"] {
        for number in 0..3 {
            set_join(
                &tx,
                namespace,
                AGENT,
                &run(number),
                Some((number as u128, true)),
            )
            .unwrap();
        }
        while !page(&tx, namespace, AGENT, 1).unwrap().ready {}
    }
    let key = canonical("move", 10);
    let movement = QueueMove {
        run: run(2),
        placement: Placement::Top,
        anchor: None,
        at_unix_ms: 10,
    };
    set_move(&tx, "root/a", AGENT, "move", Some((&key, &movement))).unwrap();
    assert!(window(&tx, "root/a", AGENT, 501).is_err());
    assert_eq!(
        window(&tx, "root/b", AGENT, 501).unwrap(),
        [run(0), run(1), run(2)]
    );
    while !page(&tx, "root/a", AGENT, 1).unwrap().ready {}
    assert_eq!(
        window(&tx, "root/a", AGENT, 501).unwrap(),
        [run(2), run(0), run(1)]
    );
    assert_eq!(
        window(&tx, "root/b", AGENT, 501).unwrap(),
        [run(0), run(1), run(2)]
    );
    assert!(clean(&tx, "root/a").unwrap());
    assert!(clean(&tx, "root/b").unwrap());
}

#[test]
fn fractional_key_exhaustion_is_a_logical_domain_fence() {
    let mut right = vec![0; 4096];
    right[4095] = 1;
    assert!(
        between(None, Some(&right))
            .unwrap_err()
            .downcast_ref::<PositionExhausted>()
            .is_some()
    );
    assert_eq!(between(None, Some(&[1])).unwrap(), vec![0, 128]);
}
