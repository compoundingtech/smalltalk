// Compile this identical probe against each revision. The driver uses the actual exchange,
// admission and projection paths; only time advances without waiting for days.
#[test]
#[ignore = "interactive two-build probe; driven by scripts/check-deadline-mixed-build.py"]
fn deadline_mixed_build_probe() {
    use std::io::{BufRead, Write};
    let origin = std::env::var("DEADLINE_PROBE_ORIGIN").unwrap();
    let path = std::env::var("DEADLINE_PROBE_DB").unwrap();
    let store = Store::open(std::path::Path::new(&path), &origin).unwrap();
    store.bind_fleet(TEST_FLEET).unwrap();
    println!(
        "PROBE {}",
        json!({"ready": true, "local_replay": true, "sync_heal": false})
    );
    std::io::stdout().flush().unwrap();
    for line in std::io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let op = request["op"].as_str().unwrap();
        let result = match op {
            "clock" => {
                let at = request["at"].as_u64().unwrap() as u128;
                smallclaims::store::set_thread_clock(Some(at));
                store.set_write_clock_at(at).unwrap();
                json!(true)
            }
            "create" => {
                let mission = publish_mission(
                    &store,
                    r#"version 2
mission "budget" state="ready" timeout="5m" {
  concurrent-runs
  goal "Measure timeout compatibility."
  step "work" { agentless }
}
"#,
                    "probe-mission",
                );
                let predecessor = store
                    .create_mission_run(&MissionRunRequest {
                        mission: "budget".into(),
                        revision: None,
                        workspace: "/tmp".into(),
                        requester: Some("person/operator".into()),
                        mode: Some("run".into()),
                        inputs: BTreeMap::new(),
                        idempotency_key: "probe-predecessor".into(),
                    })
                    .unwrap();
                let source = format!(
                    "version 2\nmission-run \"waiter\" {{\n mission \"mission/budget@{}\"\n workspace \"/tmp\"\n requester \"person/operator\"\n after {:?}\n}}\n",
                    mission.revision, predecessor.subject
                );
                let intent = parse_intent(&source, &origin).unwrap();
                let preview = store
                    .mission(
                        &intent,
                        IntentInput {
                            kdl: source,
                            source_name: None,
                        },
                    )
                    .unwrap();
                store
                    .apply(&intent, &preview.subject_tokens, "probe-waiter")
                    .unwrap();
                json!(store.mission_run("waiter").unwrap())
            }
            "inventory" => json!(store.replication_inventory().unwrap()),
            "export" => json!(
                store
                    .export_replication_exchange(
                        TEST_FLEET,
                        &serde_json::from_value(request["inventory"].clone()).unwrap()
                    )
                    .unwrap()
            ),
            "receive" => {
                let exchange: ReplicationExchange =
                    serde_json::from_value(request["exchange"].clone()).unwrap();
                receive_and_project(&store, &exchange.peer, &exchange);
                json!(true)
            }
            "summary" => json!(store.export_replication_summary(TEST_FLEET).unwrap()),
            "compare" => {
                let exchange: ReplicationExchange =
                    serde_json::from_value(request["exchange"].clone()).unwrap();
                json!(
                    store
                        .receive_replication_exchange_asking(
                            &exchange.peer,
                            TEST_FLEET,
                            &exchange,
                            true
                        )
                        .unwrap()
                )
            }
            "replay" => {
                store.replay_replication_graph().unwrap();
                json!(true)
            }
            "status" => json!(
                store
                    .replication_status(
                        true,
                        Some(TEST_FLEET),
                        &[request["peer"].as_str().unwrap().into()]
                    )
                    .unwrap()
            ),
            "view" => json!(store.mission_run("waiter").unwrap()),
            "deadline" => match store.next_active_mission_deadline(&origin) {
                Ok(value) => json!({"value": value}),
                Err(error) => json!({"error": error.to_string()}),
            },
            "admit" => {
                let run = store.mission_run("waiter").unwrap().unwrap();
                let after = run
                    .steps
                    .iter()
                    .find(|step| step.step == "after-run")
                    .unwrap();
                store
                    .set_step_state(&after.subject, "completed", None)
                    .unwrap();
                json!(true)
            }
            "quit" => break,
            _ => panic!("unknown operation {op}"),
        };
        println!("PROBE {result}");
        std::io::stdout().flush().unwrap();
    }
    smallclaims::store::set_thread_clock(None);
}
