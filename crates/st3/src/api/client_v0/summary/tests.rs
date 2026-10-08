use super::*;
use serde_json::json;

fn state(root: &std::path::Path) -> AppState {
    super::super::tests::test_state_named(root, "summary-fixture")
}
fn request(person: Option<&str>) -> CollectionSubscribe {
    serde_json::from_value(
        json!({"kind":"subscribe","id":"top","collection":"summary","limit":1,"person":person}),
    )
    .unwrap()
}

#[test]
fn summary_home_working_machine_predicates_and_deadline() {
    for kind in [
        "human-gate",
        "launch-approval",
        "revision-approval",
        "person-step",
        "agent-request",
        "harness-prompt",
        "custom.fixture",
    ] {
        assert!(home_kind(kind));
    }
    for kind in ["unread-message", "fault", "unknown"] {
        assert!(!home_kind(kind));
    }
    let agent = json!({"state":"running","harness_state":"working","fault":null,"delivery":{"state":"current"}});
    assert!(working(&agent));
    for changed in [
        json!({"fault":{}}),
        json!({"delivery":{"state":"stale"}}),
        json!({"state":"waiting"}),
        json!({"harness_state":"idle"}),
    ] {
        let mut a = agent.clone();
        for (k, v) in changed.as_object().unwrap() {
            a[k] = v.clone();
        }
        assert!(!working(&a));
    }
    let machines = vec![
        json!({"host_id":"host/alder","state":"unknown","transports":[]}),
        json!({"host_id":"host/birch","state":"reachable","transports":[]}),
        json!({"host_id":"host/cedar","state":"indeterminate","transports":[{"last_success_at":"1970-01-01T00:00:01Z"}]}),
        json!({"host_id":"host/delta","state":"offline","transports":[]}),
    ];
    assert_eq!(
        machine_counts(&machines, &[], "host/alder", 300999),
        json!({"connected":2,"indirect":1,"offline":1})
    );
    assert_eq!(
        machine_counts(&machines, &[], "host/alder", 301000),
        json!({"connected":2,"indirect":0,"offline":2})
    );
    let agents = vec![json!({"host_id":"host/delta","last_activity_at":"1970-01-01T00:05:00Z"})];
    assert_eq!(
        machine_counts(&machines, &agents, "host/alder", 301000),
        json!({"connected":2,"indirect":1,"offline":1})
    );
}

#[test]
fn native_summary_one_small_typed_row_and_authorized_person_scope() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let session = ClientSession::local(Some("person/avery")).unwrap();
    let row = state
        .store
        .read_snapshot(|index| {
            native(
                &state,
                &session,
                &request(None),
                &client_snapshot_at(&state, index),
                client_now_ms(),
                None,
                None,
            )
        })
        .unwrap();
    state
        .store
        .read_snapshot(|index| {
            let snapshot = client_snapshot_at(&state, index);
            let full = machine_resources(&state, false, &snapshot, &session)?;
            let lean = machine_summary_resources(&state, &snapshot, &session)?;
            assert_eq!(
                machine_counts(&full, &[], &client_host_id(&state.node), client_now_ms()),
                machine_counts(&lean, &[], &client_host_id(&state.node), client_now_ms())
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(row.len(), 1);
    assert_eq!(row[0]["id"], "summary/current");
    assert_eq!(row[0]["needs_you"], 0);
    assert_eq!(row[0]["working_agents"], 0);
    assert_eq!(row[0]["active_missions"], 0);
    assert_eq!(row[0]["machines"]["connected"], 1);
    assert!(serde_json::to_vec(&row).unwrap().len() < 600);
    let typed: st3_client::Resource = serde_json::from_value(row[0].clone()).unwrap();
    assert!(matches!(typed, st3_client::Resource::Summary(_)));
    assert_eq!(
        person_filter(&session, Some("person/other"))
            .unwrap_err()
            .status,
        StatusCode::FORBIDDEN
    );
    assert!(
        capabilities(&session)
            .iter()
            .any(|c| c["id"] == "summary" && c["version"] == 1 && c["state"] == "granted")
    );
    let mut next = row.clone();
    next[0]["updated_at"] = json!("2026-10-08T00:00:00Z");
    retain_timestamp(
        &mut next,
        &BTreeMap::from([("summary/current".into(), row[0].clone())]),
    );
    assert_eq!(next, row);
}

#[test]
fn native_lean_mission_membership_matches_full_cards() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let source = "version 2\nmission \"fixture-summary\" state=\"ready\" { goal \"Fixture summary\"; step \"build\" { assigned-to \"agent/builder\"; goal \"Build fixture\" } }\n";
    let intent = crate::graph::parse_intent(source, "summary-fixture").unwrap();
    let planned = store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(
            &intent,
            &planned.subject_tokens,
            "summary-fixture-publication",
        )
        .unwrap();
    let run = store
        .create_mission_run(&crate::model::MissionRunRequest {
            mission: "fixture-summary".into(),
            revision: None,
            workspace: root.path().display().to_string(),
            requester: Some("person/avery".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "summary-run".into(),
        })
        .unwrap();
    let step = run.steps.first().unwrap().subject.clone();
    store.set_step_state(&step, "ready", None).unwrap();
    let now = client_now_ms();
    store
        .read_snapshot(|index| {
            let ids = store.mission_collection_ids(false, 0, 1000)?;
            let full = mission_list_cards_at(store, &ids, now)?;
            let full: Vec<st3_client::Mission> = full
                .into_iter()
                .map(serde_json::from_value)
                .collect::<Result<_, _>>()?;
            let (lean, _) = store.client_summary_missions(now)?;
            let project = |rows: &Vec<st3_client::Mission>| {
                st3_ui_model::missions::adapt(
                    rows.iter(),
                    std::iter::empty(),
                    std::iter::empty(),
                    "",
                    &Display {
                        mission_label: &|m| m.header.id.clone(),
                        agent_label: &|a| a.name.clone(),
                        age_label: &|_, _| String::new(),
                        clean_text: &str::to_owned,
                    },
                )
                .into_iter()
                .filter(active)
                .map(|m| m.id)
                .collect::<BTreeSet<_>>()
            };
            assert_eq!(project(&full), project(&lean));
            assert_eq!(
                project(&lean),
                BTreeSet::from(["mission/fixture-summary".into()])
            );
            let row = native(
                &state,
                &ClientSession::local(Some("person/avery")).unwrap(),
                &request(None),
                &client_snapshot_at(&state, index),
                now,
                None,
                None,
            )?;
            assert_eq!(
                row[0]["active_missions"].as_u64().unwrap(),
                project(&full).len() as u64
            );
            Ok(())
        })
        .unwrap();
    // A physical lease changes effective membership without a graph append. The
    // cached input must expire at that exact boundary, not the next periodic tick.
    let expiry = client_now_ms() + 60_000;
    let c = rusqlite::Connection::open(root.path().join("graph.db")).unwrap();
    crate::store::configure_projection_writer(&c).unwrap();
    c.execute("UPDATE step_runs SET status='working',lease_owner='agent/builder',lease_incarnation='fixture',lease_expires_at_unix_ms=?2 WHERE subject=?1",
        rusqlite::params![step,expiry.to_string()]).unwrap();
    store
        .read_snapshot(|_| {
            let (before, deadline) = store.client_summary_missions(expiry - 1)?;
            assert_eq!(deadline, Some(expiry));
            assert_eq!(
                before[0].run_details[0].steps.as_ref().unwrap()[0].state,
                "claimed"
            );
            let (after, deadline) = store.client_summary_missions(expiry)?;
            assert_eq!(deadline, None);
            assert_eq!(
                after[0].run_details[0].steps.as_ref().unwrap()[0].state,
                "ready"
            );
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn summary_socket_tiny_snapshot_count_delta_silence_and_person_refusal() {
    use futures_util::{SinkExt as _, StreamExt as _};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_tungstenite::tungstenite::Message;
    let root = tempfile::tempdir().unwrap();
    let writer = state(root.path());
    let source = "version 2\nmission \"socket-summary\" state=\"ready\" { goal \"Fixture summary\"; step \"build\" { assigned-to \"agent/builder\" } }\n";
    let intent = crate::graph::parse_intent(source, "summary-fixture").unwrap();
    let planned = writer
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    writer
        .store
        .apply(
            &intent,
            &planned.subject_tokens,
            "socket-summary-publication",
        )
        .unwrap();
    let run = writer
        .store
        .create_mission_run(&crate::model::MissionRunRequest {
            mission: "socket-summary".into(),
            revision: None,
            workspace: root.path().display().to_string(),
            requester: Some("person/avery".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "socket-summary-run".into(),
        })
        .unwrap();
    let step = run.steps.first().unwrap().subject.clone();
    let state = writer.clone();
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = reads.clone();
    let app = axum::Router::new().route(
        "/stream",
        axum::routing::get(move |upgrade: WebSocketUpgrade| {
            let state = state.clone();
            let reads = reads.clone();
            async move {
                upgrade.on_upgrade(move |socket| {
                    let windows = collection_windows::Windows::attach(&state.store);
                    collection_stream_socket_with_reader(
                        socket,
                        state,
                        ClientSession::local(Some("person/avery")).unwrap(),
                        None,
                        move |state, session, request, permit| {
                            let windows = windows.clone();
                            let reads = reads.clone();
                            async move {
                                let result = collection_items_with_windows(
                                    &state, &session, &request, permit, windows,
                                )
                                .await;
                                reads.fetch_add(1, Ordering::SeqCst);
                                result
                            }
                        },
                    )
                })
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/stream"))
        .await
        .unwrap();
    socket
        .send(Message::Text(
            json!({"kind":"subscribe","id":"top","collection":"summary","limit":1})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let raw = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(raw.to_text().unwrap().len() < 1024);
    let first: Value = serde_json::from_str(raw.to_text().unwrap()).unwrap();
    assert_eq!(first["kind"], "snapshot");
    assert_eq!(first["order"], json!(["summary/current"]));
    assert_eq!(first["has_more"], false);
    assert_eq!(first["items"][0]["active_missions"], 0);
    writer.store.set_step_state(&step, "ready", None).unwrap();
    signal_changed(&writer);
    let raw = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(raw.to_text().unwrap().len() < 1024);
    let changed: Value = serde_json::from_str(raw.to_text().unwrap()).unwrap();
    assert_eq!(changed["kind"], "changes");
    assert_eq!(changed["upserts"][0]["active_missions"], 1);
    assert_eq!(changed["removes"], json!([]));
    let before = observed.load(Ordering::SeqCst);
    writer
        .store
        .append_claim(&crate::model::ClaimInput {
            subject: step.clone(),
            kind: "work.progress".into(),
            actor: None,
            fields: serde_json::from_value(json!({"attempt":1,"summary":"Fixture progress."}))
                .unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    signal_changed(&writer);
    tokio::time::timeout(Duration::from_secs(5), async {
        while observed.load(Ordering::SeqCst) <= before {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(150), socket.next())
            .await
            .is_err(),
        "equal counts must stay silent"
    );
    socket.send(Message::Text(json!({"kind":"subscribe","id":"foreign","collection":"summary","person":"person/other"}).to_string().into())).await.unwrap();
    let raw = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let refusal: Value = serde_json::from_str(raw.to_text().unwrap()).unwrap();
    assert_eq!(refusal["kind"], "error");
    assert_eq!(refusal["id"], "foreign");
    assert_eq!(refusal["retryable"], false);
    socket.close(None).await.unwrap();
    server.abort();
}

#[test]
fn summary_does_not_hydrate_thousands_of_unstarted_definitions() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let source =
        "version 2\nmission \"summary-base\" state=\"ready\" { goal \"Summary fixture\" }\n";
    let intent = crate::graph::parse_intent(source, "summary-fixture").unwrap();
    let planned = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(
            &intent,
            &planned.subject_tokens,
            "summary-large-publication",
        )
        .unwrap();
    let mut c = rusqlite::Connection::open(root.path().join("graph.db")).unwrap();
    crate::store::configure_projection_writer(&c).unwrap();
    let tx = c.transaction().unwrap();
    for i in 0..3000 {
        tx.execute("INSERT INTO mission_definitions(mission_id,revision,state,claim_id) SELECT ?1,revision,state,claim_id FROM mission_definitions WHERE mission_id='summary-base'", [format!("summary-unused-{i}")]).unwrap();
    }
    tx.commit().unwrap();
    let before = crate::store::STATEMENTS_RUN.with(std::cell::Cell::get);
    let (rows, _) = state
        .store
        .read_snapshot(|_| state.store.client_summary_missions(client_now_ms()))
        .unwrap();
    let statements = crate::store::STATEMENTS_RUN.with(std::cell::Cell::get) - before;
    assert!(rows.is_empty());
    // Four source queries plus the snapshot fence; never one query per definition.
    assert!(
        statements > 0 && statements <= 7,
        "unstarted definitions must not produce per-card queries: {statements}"
    );
}
