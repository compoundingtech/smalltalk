use super::*;

#[test]
fn slo_list_groups_objectives_and_budgets_and_explain_prints_the_plain_words() {
    let rows = vec![
        json!({"id": "person-read", "kind": "objective", "status": "met", "target": "p99 ≤ 100 ms",
            "current": "p99 80 ms over 200 samples", "window": "1h", "remaining_percent": 50.0,
            "explain": "Reads a person waits on."}),
        json!({"id": "ada/claude/weekly", "kind": "budget", "status": "stale", "target": "100% allowance",
            "current": "40% used", "window": "weekly", "remaining_percent": 60.0,
            "resets_at_unix_ms": 86_400_000u64, "explain": "An allowance that resets."}),
        json!({"id": "cpu", "kind": "objective", "status": "no-data", "target": "≤ 2 cores",
            "current": "not measured", "window": "1h", "remaining_percent": null, "explain": "x"}),
    ];
    let list = render_slo_list(&rows);
    let lines = list.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "OBJECTIVES  a target with windows and an error budget · 2");
    assert!(lines[1].starts_with("STATUS   ID           TARGET"), "{}", lines[1]);
    assert!(lines[2].starts_with("met      person-read  p99 ≤ 100 ms"), "{}", lines[2]);
    assert!(lines[2].ends_with("1h      50%"), "{}", lines[2]);
    assert!(lines[3].ends_with("1h      -"), "{}", lines[3]);
    assert_eq!(lines[5], "BUDGETS  an allowance that resets · 1");
    assert!(lines[7].contains("ada/claude/weekly") && lines[7].ends_with("60%"), "{}", lines[7]);
    assert_eq!(render_slo_list(&[]), "No objectives or budgets.\n");

    let explained = render_slo_explanation(&rows[1]);
    assert!(explained.starts_with("ada/claude/weekly  budget · stale\n"));
    assert!(explained.contains("LEFT     60% of the allowance"));
    assert!(explained.contains("RESETS   1970-01-02 00:00 UTC"));
    assert!(explained.ends_with("An allowance that resets.\n"));
}

#[test]
fn slo_chart_draws_the_lowest_reading_of_each_column_and_marks_gaps() {
    let row = json!({"id": "person-read", "kind": "objective"});
    let history = json!({
        "since_ms": 0, "until_ms": 86_400_000u64, "samples": 3,
        "steps": [100.0, 50.0, null, 0.0],
    });
    let chart = render_slo_chart(&row, &history, "24h");
    let lines = chart.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "person-read · error budget left · last 24h");
    assert_eq!(lines[1], "100% │█   ");
    assert_eq!(lines[5], " 50% │██  ");
    assert_eq!(lines[8], "  0% │██·▁");
    assert_eq!(lines[9], "     └────");
    assert!(lines[10].starts_with("      01-01 00:00 UTC "), "{}", lines[10]);
    assert!(lines[10].ends_with("01-02 00:00"), "{}", lines[10]);
    assert_eq!(lines[11], "LOW 0% · LATEST 0% · 3 readings · a column is its lowest reading");
    let empty = render_slo_chart(&row, &json!({"samples": 0, "steps": [null]}), "1h");
    assert!(empty.contains("No readings in this period"));
}

#[test]
fn slo_period_takes_minutes_hours_and_days_up_to_the_history() {
    assert_eq!(parse_slo_period("90m").unwrap(), 5_400_000);
    assert_eq!(parse_slo_period("24h").unwrap(), 86_400_000);
    assert_eq!(parse_slo_period("7d").unwrap(), 7 * 86_400_000);
    for bad in ["8d", "0m", "24", "h", "1w", "", "-1h"] {
        assert!(parse_slo_period(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn daemon_start_version_keeps_unstamped_builds_bare_and_bounds_metadata() {
    let unstamped = concat!(env!("CARGO_PKG_VERSION"), "+dev");
    assert_eq!(
        daemon_start_version(unstamped).unwrap(),
        env!("CARGO_PKG_VERSION")
    );
    for version in ["0.1.0+abc1234-dirty", "0.1.0+local.abc1234.dirty"] {
        assert_eq!(daemon_start_version(version).unwrap(), version);
    }
    assert!(daemon_start_version(&"x".repeat(128)).is_ok());
    assert!(daemon_start_version(&"x".repeat(129)).is_err());
    assert!(daemon_start_version(&"é".repeat(65)).is_err());
}

#[test]
fn checkpoint_status_labels_startup_revisions_and_missing_advertisements() {
    let status = st3::store::CheckpointStatusView {
        node: "alder".into(),
        newest_stable: None,
        trimmed: None,
        halted: false,
        pending: None,
        participants: ["alder".into(), "birch".into()].into(),
        excused: Default::default(),
        left: Default::default(),
        machine_version: [("alder".into(), "0.1.0+abc1234-dirty".into())].into(),
    };
    let output = render_checkpoint_status(&status);
    assert!(output.contains("last daemon start (advertised build)"));
    assert!(output.contains("machine_version  alder: 0.1.0+abc1234-dirty"));
    assert!(output.contains("machine_version  birch: not advertised"));
}

#[test]
fn seats_renew_once_a_minute_each_at_its_own_second_not_all_on_the_minute_boundary() {
    let seats: Vec<String> = (0..120).map(|n| format!("agent/example/seat-{n}")).collect();
    let mut renewals_per_second = [0usize; 60];
    for seat in &seats {
        let offset = renewal_offset_secs(seat);
        assert!(offset < 50, "{seat}: {offset}");
        // A driver ticking each second from a minute's start renews exactly once in each
        // minute, at its offset, and every minute is still covered.
        let mut renewed = None;
        let mut renewals = Vec::new();
        for second in 0..180u64 {
            if let Some(minute) = renewal_minute_at(seat, 1_000 * 60 + second)
                && renewed != Some(minute)
            {
                renewed = Some(minute);
                renewals.push(second);
            }
        }
        assert_eq!(renewals, vec![offset, 60 + offset, 120 + offset], "{seat}");
        renewals_per_second[offset as usize] += 1;
    }
    let busiest = renewals_per_second.iter().max().copied().unwrap();
    assert!(
        busiest <= 8,
        "120 seats should spread across the minute, not queue together: {renewals_per_second:?}"
    );
    assert!(renewals_per_second.iter().filter(|n| **n > 0).count() >= 35);
}

// LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
#[test]
fn omp_pending_ask_retry_is_bound_to_the_expected_call_and_sent_once_across_reexec() {
    let mut state = PiChannelResume::default();
    let frame = json!({"type":"retry_pending_ask","toolCallId":"X"});
    assert!(state.take_pending_ask_retry(&frame, None).is_none());
    assert!(state.take_pending_ask_retry(&frame, Some("Y")).is_none());
    assert!(state.take_pending_ask_retry(&json!({"type":"state","toolCallId":"X"}), Some("X")).is_none());
    assert_eq!(state.take_pending_ask_retry(&frame, Some("X")).as_deref(), Some("X"));
    assert!(state.take_pending_ask_retry(&frame, Some("X")).is_none());
    let mut restored: PiChannelResume =
        serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
    assert!(restored.take_pending_ask_retry(&frame, Some("X")).is_none());
}

#[test]
fn omp_delivery_readiness_does_not_clear_the_reopened_human_block() {
    let mut state = PiChannelResume::default();
    assert!(state.accept_frame(r#"{"type":"state","state":"active","blockedOn":"human","ask":"question"}"#));
    assert!(!state.first_idle_seen);
    assert!(!state.accept_frame(r#"{"type":"delivery_ready"}"#));
    assert!(state.first_idle_seen);
    assert_eq!(state.pending.blocked_on.as_deref(), Some("human"));
    assert_eq!(state.pending.ask.as_deref(), Some("question"));
    assert_eq!(state.pending.state.as_ref().map(|(status, _)| status.as_str()), Some("working"));
    assert!(state.accept_frame(r#"{"type":"state","state":"idle"}"#));
    assert!(state.pending.blocked_on.is_none());
}
// LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge

#[test]
fn clients_and_devices_say_who_is_connected_and_when_others_were_seen() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-03T12:00:00Z")
        .unwrap()
        .timestamp_millis() as u128;
    let found: st3_client::ClientConnections = serde_json::from_value(serde_json::json!({
        "kind": "client-connections", "member": "host/example",
        "items": [
            {"actor": "person/avery/session/0011", "person": "person/avery", "client": "smalltalk-ios 1.0 (42)",
             "device_id": "device/001122", "device_name": "Pocket", "member": "host/example", "via": "gateway",
             "connected": true, "streams": 1, "since": "2026-10-03T11:58:00Z", "last_seen": "2026-10-03T12:00:00Z",
             "follows": ["agents", "conversation:agent/example/harbor"]},
            {"actor": "person/avery", "person": "person/avery", "client": null, "member": "host/example",
             "via": "local", "connected": false, "streams": 0, "since": "2026-10-03T09:00:00Z",
             "last_seen": "2026-10-03T11:57:00Z", "follows": []}
        ]
    }))
    .unwrap();
    let shown = render_clients(&found, now);
    assert!(shown.contains("CLIENTS  2 on host/example"), "{shown}");
    assert!(shown.contains("smalltalk-ios 1.0 (42)  person/avery (Pocket)  via gateway  connected now, since 2m ago"), "{shown}");
    assert!(shown.contains("  follows: agents, conversation:agent/example/harbor"), "{shown}");
    assert!(shown.contains("(unnamed client)  person/avery  via local  last seen 3m ago"), "{shown}");
    let devices = "DEVICES\ndevice/001122  active  person/avery/session/0011  scopes 4\n  action: st devices --as person/avery revoke device/001122\n";
    let marked = with_device_presence(devices, Some(&found), now);
    assert!(marked.contains("scopes 4  · connected now · smalltalk-ios 1.0 (42)"), "{marked}");
    assert_eq!(with_device_presence(devices, None, now), devices);
}

#[test]
fn harness_todo_graph_channel_rehydrates_and_spools_only_current_binding() {
    let root = tempfile::tempdir().unwrap();
    st_drivers::harness_events::enable(root.path(), "runtime-a").unwrap();
    let mut state = PiChannelResume {
        incarnation: "runtime-a".into(),
        todo_outbox: Some(root.path().into()),
        ..PiChannelResume::default()
    };
    let frame = json!({
        "type":"todo","session":"native-a","source_op":"hydrate",
        "observed_at":"2026-10-03T15:00:00Z","phases":[],
        "totals":{"pending":0,"in_progress":0,"completed":0,"blocked":0},
        "truncated":false
    }).to_string();
    assert!(!state.accept_frame(&frame));
    assert!(state.accept_frame(r#"{"type":"ready","sessionId":"native-a"}"#));
    assert!(state.accept_frame(&frame));
    let events = st_drivers::harness_events::pending(root.path(), 100).unwrap();
    assert_eq!(events[0].payload["source_op"], "hydrate");
    assert_eq!(events[0].payload["phases"], json!([]));
    let mut resumed: PiChannelResume =
        serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
    assert!(resumed.accept_frame(r#"{"type":"session","sessionId":"native-b"}"#));
    assert!(!resumed.accept_frame(&frame));
    assert_eq!(st_drivers::harness_events::pending(root.path(), 100).unwrap().len(), 1);
}

#[test]
fn harness_todo_spool_reconnect_preserves_sequence_and_next_incarnation_cleans_only_its_seat() {
    let root = tempfile::tempdir().unwrap();
    let current = prepare_channel_todo_outbox(root.path(), "agent/one", "runtime-a").unwrap();
    st_drivers::harness_events::enable(&current, "runtime-a").unwrap();
    let fields = json!({
        "harness":"omp", "incarnation_id":"runtime-a", "session_id":"native",
        "observed_at":"2026-10-03T15:00:00Z", "source_op":"hydrate", "phases":[],
        "totals":{"pending":0,"in_progress":0,"completed":0,"blocked":0}, "truncated":false,
    });
    st_drivers::harness_events::write_channel_todo(&current, "runtime-a", &fields).unwrap();
    let sequence = st_drivers::harness_events::pending(&current, 10).unwrap()[0].sequence;
    let resumed = prepare_channel_todo_outbox(root.path(), "agent/one", "runtime-a").unwrap();
    assert_eq!(resumed, current);
    st_drivers::harness_events::write_channel_todo(&resumed, "runtime-a", &fields).unwrap();
    assert!(st_drivers::harness_events::pending(&resumed, 10).unwrap()[1].sequence > sequence);
    let unrelated = prepare_channel_todo_outbox(root.path(), "agent/two", "runtime-b").unwrap();
    st_drivers::harness_events::enable(&unrelated, "runtime-b").unwrap();
    prepare_channel_todo_outbox(root.path(), "agent/one", "runtime-c").unwrap();
    assert!(!current.exists());
    assert!(unrelated.exists());
    assert!(!todo_runtime_has_ended(&json!({"incarnation_id":"a","status":"running"}), "a"));
    assert!(todo_runtime_has_ended(&json!({"incarnation_id":"a","status":"exited"}), "a"));
    assert!(todo_runtime_has_ended(&json!({"incarnation_id":"b","status":"running"}), "a"));
    assert!(!todo_runtime_has_ended(&json!({}), "a"));
}

#[tokio::test]
async fn todo_liveness_confirmation_requires_full_period_including_eof() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("spool");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("pending"), b"unpublished todo").unwrap();
    let store = std::sync::Arc::new(Store::open_memory("todo-liveness").unwrap());
    let observe = |status: &str| {
        store.append_claim(&ClaimInput {
            subject: "agent/seat".into(), kind: "runtime.observed".into(), actor: None,
            fields: serde_json::from_value(json!({
                "incarnation_id":"current", "status":status,
            })).unwrap(),
            evidence: vec![], expected_subject: None, idempotency_key: None,
        }).unwrap();
    };
    let state = st3::api::AppState {
        store: store.clone(), notify: std::sync::Arc::new(tokio::sync::Notify::new()),
        event_notify: tokio::sync::watch::channel(0).0,
        node: "todo-liveness".into(), state_dir: root.path().into(),
        pty_root: root.path().join("pty"), pty_binary: "pty".into(),
        fleet_id: None, configured_peers: vec![], client_relay: None,
        native_session_home: None, planner_default: Default::default(),
    };
    let path = root.path().join("api.sock");
    let socket = path.clone();
    let server = tokio::spawn(async move {
        st3::api::serve_unix(&socket, st3::api::router(state)).await.unwrap();
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() { tokio::task::yield_now().await; }
    }).await.unwrap();
    let client = Client::unix(&path);
    tokio::time::pause();
    // Keep socket I/O from automatically advancing the paused clock to unrelated timers.
    let clock_guard = tokio::spawn(async {
        loop { tokio::task::yield_now().await; }
    });
    let mut end_seen = None;
    observe("vanished");
    assert!(!remove_confirmed_ended_channel_todo_outbox(
        &client, "agent/seat", "current", &dir, &mut end_seen,
    ).await.unwrap());
    assert_eq!(fs::read(dir.join("pending")).unwrap(), b"unpublished todo");
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(!remove_confirmed_ended_channel_todo_outbox(
        &client, "agent/seat", "current", &dir, &mut end_seen,
    ).await.unwrap());
    tokio::time::advance(Duration::from_secs(58)).await;
    assert!(!remove_confirmed_ended_channel_todo_outbox(
        &client, "agent/seat", "current", &dir, &mut end_seen,
    ).await.unwrap());
    assert!(dir.join("pending").exists());
    observe("running");
    assert!(!remove_confirmed_ended_channel_todo_outbox(
        &client, "agent/seat", "current", &dir, &mut end_seen,
    ).await.unwrap());
    observe("stopped");
    assert!(!remove_confirmed_ended_channel_todo_outbox(
        &client, "agent/seat", "current", &dir, &mut end_seen,
    ).await.unwrap());
    assert!(!remove_confirmed_ended_channel_todo_outbox(
        &client, "agent/missing", "current", &dir, &mut end_seen,
    ).await.unwrap());
    assert!(!remove_confirmed_ended_channel_todo_outbox(
        &client, "agent/seat", "current", &dir, &mut end_seen,
    ).await.unwrap());
    let unavailable = Client::unix(&root.path().join("unavailable.sock"));
    assert!(remove_confirmed_ended_channel_todo_outbox(
        &unavailable, "agent/seat", "current", &dir, &mut end_seen,
    ).await.is_err());
    assert!(!remove_confirmed_ended_channel_todo_outbox(
        &client, "agent/seat", "current", &dir, &mut end_seen,
    ).await.unwrap());
    assert!(dir.join("pending").exists());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(!remove_confirmed_ended_channel_todo_outbox(
        &client, "agent/seat", "current", &dir, &mut end_seen,
    ).await.unwrap());
    tokio::time::advance(Duration::from_secs(59)).await;
    assert!(remove_confirmed_ended_channel_todo_outbox(
        &client, "agent/seat", "current", &dir, &mut end_seen,
    ).await.unwrap());
    assert!(!dir.exists());
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("pending"), b"unpublished EOF todo").unwrap();
    end_seen = None;
    assert!(!remove_confirmed_ended_channel_todo_outbox(
        &client, "agent/seat", "current", &dir, &mut end_seen,
    ).await.unwrap());
    let first_seen = end_seen.unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    clock_guard.abort();
    let _ = clock_guard.await;
    assert!(finish_channel_todo_outbox(
        &client, "agent/seat", "current", &dir, &mut end_seen,
    ).await.unwrap());
    assert!(first_seen.elapsed() >= CHANNEL_TODO_END_CONFIRMATION_GAP);
    assert!(!dir.exists());
    server.abort();
    let _ = server.await;
}

#[test]
fn client_api_errors_print_in_plain_words_with_their_code() {
    let api = st3_client::ClientError::Api(
        st3_client::ErrorCode::StaleFence,
        "the client snapshot changed before the action was submitted".into(),
        Box::new(st3_client::ErrorEnvelope {
            api_version: "st3.client.v0".into(),
            error_version: "st3.client.error.v0".into(),
            request_id: "request/test".into(),
            code: st3_client::ErrorCode::StaleFence,
            message: "the client snapshot changed before the action was submitted".into(),
            retryable: false,
            retry_after_ms: None,
            details: Default::default(),
        }),
    );
    let error = anyhow::Error::new(api).context("start the terminal");
    assert_eq!(
        plain_error(&error),
        "start the terminal: st changed while this was on its way, so it was not applied (stale-fence)"
    );
    assert_eq!(plain_error(&anyhow::anyhow!("plain")), "plain");
}

#[test]
fn a_claude_session_without_a_binding_is_reported_once_after_the_grace() {
    let dir = tempfile::tempdir().unwrap();
    let start = Instant::now();
    let mut watch = ClaudeBindingWatch::default();
    assert_eq!(watch.overdue(dir.path(), None, start), None);
    assert_eq!(watch.overdue(dir.path(), Some("wrapper-1"), start), None);
    let late = start + CLAUDE_BINDING_GRACE;
    let reason = watch.overdue(dir.path(), Some("wrapper-1"), late).unwrap();
    assert!(reason.contains("bound no native session"), "{reason}");
    assert_eq!(watch.overdue(dir.path(), Some("wrapper-1"), late), None);
    watch.retry();
    assert!(watch.overdue(dir.path(), Some("wrapper-1"), late).is_some());
    // A new wrapper session gets its own grace, and a bound one is never reported.
    assert_eq!(watch.overdue(dir.path(), Some("wrapper-2"), late), None);
    fs::write(
        dir.path().join(st3::hooks::CLAUDE_BINDING_FILE),
        r#"{"incarnation":"wrapper-2","native_session_id":"native-2"}"#,
    )
    .unwrap();
    assert_eq!(
        watch.overdue(dir.path(), Some("wrapper-2"), late + CLAUDE_BINDING_GRACE),
        None
    );
}
use std::sync::atomic::{AtomicUsize, Ordering};

fn peer_status(peer: &str, digest: Option<&str>) -> st3::model::ReplicationPeerStatus {
    st3::model::ReplicationPeerStatus {
        last_successful_exchange: None,
        last_failed_exchange: None,
        worker: None,
        projection_digests: Default::default(),
        incompatible_projection_tables: Vec::new(),
        differing_tables: Vec::new(),
        projection_comparison_waiting: false,
        projection_comparison_waiting_reason: None,
        projection_graphs_equal: None,
        projection_compared_at_unix_ms: None,
        peer: peer.into(),
        status: "up".into(),
        last_success_at_unix_ms: None,
        last_error: None,
        last_failure_at_unix_ms: None,
        refusal_reason: None,
        schema_digest: None,
        projection_registry_digest: None,
        projection_versions: Default::default(),
        authority_digest: digest.map(str::to_owned),
        graph_digest: None,
        sync: None,
    }
}

#[test]
fn a_wait_counts_only_exchanges_since_it_began() {
    let since = 10_000;
    let with = |name: &str, status: &str, measured: Option<(u128, u64)>| {
        let mut peer = peer_status(name, None);
        peer.status = status.into();
        peer.sync = measured.map(|(at, peer_only)| st3::model::ReplicationPeerSync {
            peer_only_envelopes: peer_only,
            measured_at_unix_ms: at,
            ..Default::default()
        });
        peer
    };
    let status = |peers| ReplicationStatus {
        peers,
        ..Default::default()
    };
    // An in-sync measurement from before the wait, as after a restart, proves nothing.
    let error = caught_up_since(&status(vec![with("alder", "up", Some((9_000, 0)))]), since)
        .unwrap_err();
    assert_eq!(error, "no exchange with alder yet");
    let error = caught_up_since(
        &status(vec![with("alder", "up", Some((11_000, 300)))]),
        since,
    )
    .unwrap_err();
    assert_eq!(error, "alder has 300 envelopes this node lacks");
    // A member that is not up cannot be checked, but some member must be.
    let caught_up = caught_up_since(
        &status(vec![
            with("alder", "up", Some((11_000, 0))),
            with("birch", "last-seen", Some((9_000, 0))),
        ]),
        since,
    )
    .unwrap();
    assert_eq!(caught_up.peers, vec![("alder".to_owned(), 11_000)]);
    assert_eq!(caught_up.not_checked, vec!["birch (last-seen)".to_owned()]);
    assert!(
        render_caught_up(&caught_up, 12_000)
            .contains("not checked, since they are not up: birch")
    );
    let error =
        caught_up_since(&status(vec![with("birch", "last-seen", None)]), since).unwrap_err();
    assert_eq!(error, "no peer has exchanged with this node");
}

#[test]
fn fleet_wait_refuses_only_on_admissions_the_fleet_fenced() {
    let residue = |reason: &str| smallclaims::store::FleetAdmissionResidue {
        writer: "willow".into(),
        reason: reason.into(),
        envelopes: 2,
    };
    assert_eq!(render_fenced_admissions(&[]), None);
    assert_eq!(render_fenced_admissions(&[residue("admitted-unsigned")]), None);
    let refused = render_fenced_admissions(&[
        residue("admitted-unsigned"),
        residue("admitted-beyond-high-water"),
    ])
    .unwrap();
    assert!(refused.contains("admitted 2 envelopes from willow beyond the writer's high water"), "{refused}");
}

#[test]
fn mixed_build_status_reports_log_verification_and_waiting_projections() {
    let first = st3::model::ReplicationFirstSync {
        state: "verified".into(),
        authority_digest: Some("log-digest".into()),
        envelopes: Some(3),
        peer: Some("alder".into()),
        ..Default::default()
    };
    let rendered = render_first_sync(&first, 0);
    assert!(rendered.contains("log digest log-digest"));
    assert!(!rendered.contains("projection comparison waits"));
    let mut peer = peer_status("alder", Some("log-digest"));
    peer.graph_digest = Some("another-graph".into());
    peer.projection_comparison_waiting = true;
    peer.projection_comparison_waiting_reason = Some("sample reason".into());
    peer.sync = Some(Default::default());
    let rendered = render_replication_peers(&[peer], "local-graph", 0);
    assert!(rendered.contains("same envelopes"));
    assert!(rendered.contains("waiting because sample reason"));
    assert!(rendered.contains("projection comparison waits for a settled exchange or a compatible build"));
    assert!(!rendered.contains("graphs differ"));
    assert!(!rendered.contains("diverged"));
}

#[test]
fn a_held_comparison_decides_in_sync_and_shows_its_age() {
    let held = |equal: bool| {
        let mut peer = peer_status("alder", Some("log-digest"));
        peer.projection_digests = [("documents".to_string(), "x".to_string())].into();
        peer.graph_digest = Some("older-graph".into());
        peer.projection_graphs_equal = Some(equal);
        peer.projection_compared_at_unix_ms = Some(60_000);
        peer.sync = Some(Default::default());
        render_replication_peers(&[peer], "local-graph", 240_000)
    };
    let equal = held(true);
    assert!(equal.contains("in sync"), "{equal}");
    assert!(equal.contains("graphs compared 3m ago"), "{equal}");
    assert!(!equal.contains("graphs differ"), "{equal}");
    let different = held(false);
    assert!(different.contains("graphs differ"), "{different}");
    assert!(different.contains("compared 3m ago"), "{different}");
    assert!(!different.contains("in sync"), "{different}");
}

#[test]
fn a_leave_is_confirmed_by_a_matching_digest_or_a_refusal_as_left() {
    let mut status = ReplicationStatus {
        authority_digest: "mine".into(),
        peers: vec![peer_status("a", Some("theirs")), peer_status("c", None)],
        ..Default::default()
    };
    assert_eq!(leave_confirmation(&status, None).unwrap(), None);
    assert!(leave_peer_summary(&status).contains("a up (different digest)"));

    // A member that admitted the leave refuses this node and never reports its digest.
    let left = st3::config::FleetRemoval {
        reported_by: "a".into(),
        code: "member-left".into(),
    };
    assert_eq!(
        leave_confirmation(&status, Some(&left)).unwrap(),
        Some("a".into())
    );

    // A removal is no confirmation: writes after it do not replicate.
    let removed = st3::config::FleetRemoval {
        reported_by: "c".into(),
        code: "member-removed".into(),
    };
    let error = leave_confirmation(&status, Some(&removed)).unwrap_err();
    assert!(error.to_string().contains("--offline"), "{error}");

    status.peers[1].authority_digest = Some("mine".into());
    assert_eq!(leave_confirmation(&status, None).unwrap(), Some("c".into()));
}

#[test]
fn missions_tree_fixture_renders_all_sections() {
    let fixture: Value =
        serde_json::from_str(include_str!("../tests/fixtures/missions-tree.json"))
            .expect("valid missions tree fixture");
    assert_eq!(
        render_missions_tree(&fixture),
        include_str!("../tests/fixtures/missions-tree.txt")
    );
    let cli = Cli::try_parse_from(["st", "missions", "tree", "--json"])
        .expect("missions tree --json parses");
    assert!(cli.json);
    assert!(matches!(
        cli.command,
        Command::Missions {
            command: MissionViewCommand::Tree
        }
    ));
}

#[test]
fn a_conversation_reads_as_stui_shows_it_and_raw_keeps_every_entry() {
    let items: Vec<ClientTimelineEntry> = serde_json::from_str(include_str!(
        "../../../fixtures/clients/transcripts/claude.json"
    ))
    .unwrap();
    let pretty = conversation_text(
        "session/example",
        &items,
        80,
        false,
        st3_conversation_ui::Density::Full,
    );
    assert!(pretty.starts_with("CONVERSATION  session/example"), "{pretty}");
    assert!(pretty.contains("Please check why the n"), "{pretty}");
    // The harness's own wrappers are cleaned away, as in stui; no escapes without colour.
    for noise in ["<task-notification>", "<channel", "<command-name>", "\x1b["] {
        assert!(!pretty.contains(noise), "{noise}: {pretty}");
    }
    let raw = timeline_entries_text("session/example", &items);
    assert!(raw.contains("<task-notification>"), "raw keeps what was stored");
}

#[test]
fn documents_get_renders_markdown_on_a_terminal_and_leaves_data_alone() {
    let rendered = render_document(b"# Title\n\nSome **bold** words.\n\n- one\n- two\n", 60).unwrap();
    assert!(rendered.contains("Title") && rendered.contains("\x1b["), "{rendered:?}");
    assert!(rendered.contains("bold") && !rendered.contains("**bold**"), "{rendered:?}");
    // Data and binaries print as stored.
    assert!(render_document(br#"{"a": 1}"#, 60).is_none());
    assert!(render_document(b"version 2\nmission \"x\" {}", 60).is_none());
    assert!(render_document(&[0xff, 0xfe, 0x00], 60).is_none());
}

#[test]
fn cli_timeline_folds_claude_skill_and_raw_preserves_the_expansion() {
    let items: Vec<ClientTimelineEntry> = serde_json::from_str(include_str!(
        "../../../fixtures/clients/transcripts/claude-skill.json"
    ))
    .unwrap();
    for density in [
        st3_conversation_ui::Density::Full,
        st3_conversation_ui::Density::Simple,
    ] {
        let pretty = conversation_text("session/example", &items, 100, false, density);
        assert!(pretty.contains("Skill st"), "{pretty}");
        assert!(!pretty.contains("## Messages"), "{pretty}");
        assert!(pretty.contains("I have loaded the st skill."), "{pretty}");
    }
    let raw = timeline_entries_text("session/example", &items);
    assert!(raw.contains("# st\n\nThis applies only"), "{raw}");
    assert!(raw.contains("When\nit prints nothing"), "{raw}");
    assert!(
        raw.contains("`<smalltalk-message>`, followed by a bounded preview."),
        "{raw}"
    );
    // Paging can start after Skill: the expansion remains one block, with intact rows.
    let orphan = conversation_text(
        "session/example",
        &items[2..3],
        100,
        false,
        st3_conversation_ui::Density::Full,
    );
    assert!(orphan.lines().any(|line| line.trim() == "## Messages"), "{orphan}");
    assert!(
        orphan.contains("<smalltalk-message>, followed by a bounded preview."),
        "{orphan}"
    );
    assert!(!orphan.contains("Whenit"), "{orphan}");
}

#[test]
fn usage_report_ranks_each_group_by_cost_and_marks_unpriced_tokens() {
    let report = json!({"rows": [
        {"agent":"agent/cheap","mission_run":"mission-run/one","step":"step-run/one/build","model":"model-a","account":"claude/aaaa","host":"host/a","cost_microusd":250000,"total_tokens":900,"input_tokens":200,"output_tokens":100,"cache_write_tokens":0,"cached_tokens":600,"unpriced_tokens":0},
        {"agent":"agent/dear","mission_run":"mission-run/two","step":"step-run/two/review","model":"model-b","account":"claude/bbbb","host":"host/b","cost_microusd":3000000,"total_tokens":30,"input_tokens":5,"output_tokens":2,"cache_write_tokens":3,"cached_tokens":20,"unpriced_tokens":0},
        {"agent":"agent/dear","mission_run":"mission-run/two","step":"step-run/two/review","model":"model-b","account":"claude/bbbb","host":"host/b","cost_microusd":1000000,"total_tokens":10,"input_tokens":2,"output_tokens":1,"cache_write_tokens":1,"cached_tokens":6,"unpriced_tokens":0},
        {"agent":"agent/local","mission_run":"","step":"","model":"model-local","account":"","host":"host/a","cost_microusd":0,"total_tokens":50,"input_tokens":50,"output_tokens":0,"cache_write_tokens":0,"cached_tokens":0,"unpriced_tokens":50},
    ]});
    let output = render_usage_report(&report, 24, None);
    assert_eq!(output.matches("USAGE  ").count(), 6);
    assert!(
        output.starts_with("SPEND  $4.25+ · 990 tokens · 24h · API-equivalent\n"),
        "{output}"
    );
    assert!(output.contains("UNPRICED  50 tokens on models without a price"));
    // Cost ranks, not tokens: the dear agent spent fewer tokens on a pricier model.
    assert!(
        output.find("$4.00  40  7  3  4  26  agent/dear").unwrap()
            < output
                .find("$0.25  900  200  100  0  600  agent/cheap")
                .unwrap()
    );
    assert!(output.contains("$0.00+  50  50  0  0  0  agent/local"));
    assert!(output.contains("$4.00  40  7  3  4  26  step-run/two/review"));
    assert!(output.contains("$4.00  40  7  3  4  26  claude/bbbb"));
    assert!(output.contains("$0.00+  50  50  0  0  0  unknown"));
    let with_limits = json!({"rows": [], "limits": [
        {"account": "claude/aaaa", "weekly_percent": 96.4, "five_hour_percent": null,
         "weekly_resets_at_unix_ms": 1_800_000_000_000_u64, "measured_at_unix_ms": 1_799_000_000_000_u64},
    ]});
    let output_with_limits = render_usage_report(&with_limits, 24, None);
    assert!(
        output_with_limits
            .contains("96%  ?  2027-01-15 08:00 UTC  2027-01-03 18:13 UTC  claude/aaaa"),
        "{output_with_limits}"
    );
    let exempt = json!({"rows": [], "limits": [
        {"account": "codex/aaaa", "weekly_percent": 99.0, "five_hour_percent": null, "exempt": true,
         "weekly_resets_at_unix_ms": 1_800_000_000_000_u64, "measured_at_unix_ms": 1_799_000_000_000_u64},
        {"account": "claude/bbbb", "weekly_percent": 50.0, "five_hour_percent": null, "exempt": false,
         "weekly_resets_at_unix_ms": 1_800_000_000_000_u64, "measured_at_unix_ms": 1_799_000_000_000_u64},
    ], "limits_policy": {"stop_at_weekly_percent": 95, "exempt_accounts": ["ada/codex"], "exempt_harnesses": []}});
    let output_exempt = render_usage_report(&exempt, 24, None);
    assert!(output_exempt.contains("codex/aaaa  exempt\n"), "{output_exempt}");
    assert!(output_exempt.contains("claude/bbbb\n"), "{output_exempt}");
    assert!(
        output_exempt.contains("EXEMPT  never stopped at 95% · accounts: ada/codex · harnesses: none"),
        "{output_exempt}"
    );
    let by_step = render_usage_report(&report, 24, Some(UsageBy::Step));
    assert_eq!(by_step.matches("USAGE  ").count(), 1);
    assert!(by_step.contains("by step"));
}

#[test]
fn usage_report_labels_the_daily_proxy_and_unknown_share() {
    let report = json!({"rows":[],"agent_messages":{
        "source":"dated audit", "fallback_low_microusd":220000,"fallback_high_microusd":330000,
        "days":[{"day_start_ms":0,"since_ms":1,"until_ms":86400000,
            "messages":2,"calibrated_messages":1,"low_microusd":440000,"high_microusd":660000,
            "usage_cost_microusd":0,"unpriced_tokens":100,"low_percent":null,"high_percent":null}]
    }});
    let rendered = render_usage_report(&report, 24, None);
    assert!(rendered.contains("daily estimate"));
    assert!(rendered.contains("includes useful work"));
    assert!(
        rendered
            .contains("1970-01-01 (partial)  2  $0.44–$0.66  unknown (no priced usage)  1/2"),
        "{rendered}"
    );
}

#[test]
fn declared_accounts_keep_response_spend_and_limits_separate_without_provider_identity() {
    let store = Store::open_memory("alder").unwrap();
    let mut labels = Vec::new();
    for (index, account) in ["ada/one", "ada/two"].into_iter().enumerate() {
        let subject = format!("agent/example/seat-{index}");
        let mut claim = ClaimInput {
            subject: subject.clone(), kind: "harness.timeline".into(), actor: Some(subject),
            fields: serde_json::from_value(json!({
                "operation": "append", "entry_id": format!("response-{index}"), "source_id": format!("response-{index}"),
                "sequence": 1, "revision": 1, "role": "system", "entry_type": "usage", "final": true,
                "driver": "codex", "incarnation_id": "inc-one", "observed_at_unix_ms": now_ms() as u64,
                "body": {"semantics": "response", "model": "example-model", "input_tokens": 10, "output_tokens": 2, "total_tokens": 12}
            })).unwrap(),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        };
        bind_observation_account(&mut claim, Some(account));
        labels.push(claim.fields["body"]["account"].clone());
        let observation = store.append_claim(&claim).unwrap();
        let rollup = store
            .usage_rollup_for_timeline(&observation)
            .unwrap()
            .unwrap();
        assert_eq!(rollup.fields["account"], labels[index]);
        assert_eq!(rollup.fields["total_tokens"], 12);
        store.append_claim(&rollup).unwrap();

        claim.kind = "harness.limits".into();
        claim.fields =
            serde_json::from_value(json!({"driver": "codex", "weekly_percent": 20 + index,
            "measured_at_unix_ms": now_ms() as u64}))
            .unwrap();
        bind_observation_account(&mut claim, Some(account));
        assert_eq!(claim.fields["account"], labels[index]);
        assert_eq!(claim.fields["account_ref"], account);
        store.append_claim(&claim).unwrap();
    }
    assert_ne!(labels[0], labels[1]);
    assert_eq!(store.account_limits().unwrap().len(), 2);
}

#[test]
fn an_unbound_observation_keeps_the_provider_account() {
    let mut claim = ClaimInput {
        subject: "agent/example/seat".into(),
        kind: "harness.limits".into(),
        actor: None,
        fields: BTreeMap::from([
            ("driver".into(), json!("claude")),
            ("account".into(), json!("claude/provider-label")),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: None,
    };
    let before = claim.fields.clone();
    bind_observation_account(&mut claim, None);
    assert_eq!(claim.fields, before);
}

#[test]
fn usage_marks_a_reading_older_than_the_limits_cadence_as_stale() {
    let report = json!({"rows": [], "limits": [
        {"account": "claude/aaaa", "account_ref": "ada/claude-1", "weekly_percent": 84.0, "stale": true,
         "measured_at_unix_ms": 1_799_000_000_000_u64},
        {"account": "claude/bbbb", "weekly_percent": 50.0, "stale": false,
         "measured_at_unix_ms": 1_799_000_000_000_u64},
    ]});
    let output = render_usage_report(&report, 24, None);
    assert!(output.contains("  ada/claude-1 (claude/aaaa)  stale\n"), "{output}");
    assert!(output.contains("  claude/bbbb\n"), "{output}");
}

#[test]
fn usage_names_a_declared_account_beside_the_providers_label() {
    let report = json!({
        "rows": [
            {"agent":"agent/a","model":"m","account":"claude/aaaa","host":"host/a","cost_microusd":1000000,"total_tokens":10,"input_tokens":5,"output_tokens":5,"cache_write_tokens":0,"cached_tokens":0,"unpriced_tokens":0},
            {"agent":"agent/b","model":"m","account":"claude/bbbb","host":"host/a","cost_microusd":1000000,"total_tokens":10,"input_tokens":5,"output_tokens":5,"cache_write_tokens":0,"cached_tokens":0,"unpriced_tokens":0},
        ],
        "limits": [
            {"account": "claude/aaaa", "account_ref": "ada/claude-1", "weekly_percent": 40.0,
             "measured_at_unix_ms": 1_799_000_000_000_u64},
            {"account": "claude/bbbb", "weekly_percent": 10.0,
             "measured_at_unix_ms": 1_799_000_000_000_u64},
        ],
    });
    let output = render_usage_report(&report, 24, None);
    assert!(
        output.contains("  ada/claude-1 (claude/aaaa)\n"),
        "{output}"
    );
    assert!(output.contains("40%  ?"), "{output}");
    assert!(
        output.contains("  claude/bbbb\n"),
        "an account no declaration names keeps its label: {output}"
    );
}

#[test]
fn conversations_ls_accepts_the_read_spelling_of_its_mailbox() {
    let cli = Cli::try_parse_from(["st3", "conversations", "ls", "--as", "agent/run-1/worker"])
        .expect("conversations ls --as parses");
    let Command::Conversations {
        command: MessageCommand::Ls(args),
    } = cli.command
    else {
        panic!("conversations ls")
    };
    assert_eq!(args.actor.as_deref(), Some("agent/run-1/worker"));
    assert!(
        Cli::try_parse_from(["st3", "conversations", "ls", "agent/a", "--as", "agent/b"])
            .is_err()
    );
}

#[test]
fn pi_family_mail_uses_the_shared_envelope_and_the_steer_boundary() {
    let message = st3::model::MessageView {
        subject: "message/0123456789abcdef".into(),
        from: "agent/run-1/wake.claude".into(),
        to: "agent/run-1/wake.omp-2".into(),
        content: "FACT QUARTZ".into(),
        status: "sent".into(),
        title: Some("Cross-harness consensus: idle".into()),
        in_reply_to: None,
        tags: vec![],
        body_ref: None,
        body_bytes: None,
        body_origin: None,
        created_index: 1,
        attachments: Vec::new(),
    };
    let omp = pi_family_message_frame(&message, "FACT QUARTZ", "run-1/wake.omp-2", &[]);
    assert_eq!(omp["deliverAs"], "steer");
    assert_eq!(
        omp["content"],
        format!(
            "<smalltalk-message id=\"0123456789abcdef\" from=\"agent/run-1/wake.claude\" \
                 to=\"agent/run-1/wake.omp-2\" subject=\"Cross-harness consensus: idle\" \
                 sha256=\"{}\" graph=\"message/0123456789abcdef\">\nFACT QUARTZ\n</smalltalk-message>",
            st_drivers::ding::st3_body_sha256("FACT QUARTZ")
        )
    );
    assert_eq!(omp["meta"]["messageId"], "message/0123456789abcdef");
}

#[test]
fn native_push_preserves_delivery_notices_and_person_signatures() {
    let mut view = MessageView {
        subject: "message/notice".into(),
        from: "agent/example/writer".into(),
        to: "agent/example/reader".into(),
        content: "body".into(),
        status: "sent".into(),
        title: None,
        in_reply_to: None,
        tags: vec![
            st3::silent::SILENT_TAG.into(),
            "dictated".into(),
            format!("{}12", st3::silent::REMAINING_PREFIX),
        ],
        attachments: vec![],
        body_ref: None,
        body_bytes: None,
        body_origin: None,
        created_index: 1,
    };
    let queued = native_queued_message(&view, "body".into(), &[]);
    let rendered = st_drivers::ding::with_tag_notices("body".into(), &queued.tags);
    assert!(rendered.contains("silent: held"));
    assert!(rendered.contains("12 older silent held; st conversations ls"));
    assert!(rendered.contains("dictated by voice"));
    assert_eq!(queued.body, "body");
    view.from = "person/operator".into();
    let queued = native_queued_message(&view, "body".into(), &[]);
    assert!(!queued.tags.iter().any(|tag| tag == st3::silent::SILENT_TAG));
    assert!(
        view.tags.iter().any(|tag| tag == st3::silent::SILENT_TAG),
        "signed durable tags are unchanged"
    );
}

#[test]
fn a_person_option_points_an_agent_to_its_own_commands() {
    let refusal = parse_person_subject("agent/run-1/worker").unwrap_err();
    assert!(refusal.contains("takes a person, not the agent `agent/run-1/worker`"));
    assert!(refusal.contains("work ls --as"));
    assert_eq!(
        parse_person_subject("person/operator").as_deref(),
        Ok("person/operator")
    );
    assert!(parse_person_subject("operator").is_err());
}

#[test]
fn a_harness_cannot_act_as_another_agent() {
    let own = Some("agent/run-1/wake.omp-2");
    let run = Some("run-1");
    let refusal = foreign_agent_actor("agent/run-1/wake.codex", own, run).unwrap();
    assert!(refusal.contains("this harness is `agent/run-1/wake.omp-2`"));
    assert!(foreign_agent_actor("wake.codex", own, run).is_some());
    assert!(foreign_agent_actor("agent/run-1/wake.omp-2", own, run).is_none());
    assert!(foreign_agent_actor("wake.omp-2", own, run).is_none());
    // Non-agent actors and processes without a seat identity are not seat impersonation.
    assert!(foreign_agent_actor("person/eval-requester", own, run).is_none());
    assert!(foreign_agent_actor("requester", own, run).is_none());
    assert!(foreign_agent_actor("exec/run-1/controller", own, run).is_none());
    assert!(foreign_agent_actor("agent/run-1/wake.codex", None, run).is_none());
    assert!(
        foreign_agent_actor("agent/run-1/wake.codex", Some("person/operator"), run).is_none()
    );
}

#[test]
fn mission_start_requires_an_explicit_actor() {
    assert!(Cli::try_parse_from(["st3", "missions", "start", "mission/demo"]).is_err());
}

#[test]
fn a_harness_cannot_mutate_as_a_peer_or_person() {
    let cases: &[&[&str]] = &[
        &["st3", "work", "done", "step-run/example/ask", "--as", "person/avery", "--summary", "Friday"],
        &["st3", "conversations", "archive", "message/notice", "--as", "person/avery"],
        &["st3", "work", "delegation", "--for", "person/avery", "--as", "person/avery", "--evidence", "claim/decision"],
        &[
            "st3",
            "apply",
            "mission.kdl",
            "--as",
            "agent/peer",
        ],
        &[
            "st3",
            "missions",
            "start",
            "mission/demo",
            "--as",
            "person/operator",
        ],
        &[
            "st3",
            "agents",
            "queue",
            "move",
            "agent/worker",
            "mission-run/demo/one",
            "--top",
            "--as",
            "agent/peer",
        ],
        &[
            "st3",
            "work",
            "revision",
            "approve",
            "revision-proposal/x",
            "hash",
            "--as",
            "person/operator",
        ],
        &["st3", "work", "wake", "step-run/x/y", "--as", "agent/peer"],
        &[
            "st3",
            "diagnostic",
            "--as",
            "person/operator",
            "--code",
            "test",
            "--reason",
            "test",
        ],
        &[
            "st3",
            "gh",
            "watch",
            "acme/garden#12",
            "--as",
            "person/operator",
        ],
        &[
            "st3",
            "gh",
            "unwatch",
            "acme/garden#12",
            "--as",
            "agent/peer",
        ],
    ];
    for arguments in cases {
        let cli = Cli::try_parse_from(*arguments).unwrap();
        assert!(
            guard_mutating_cli_actor(&cli.command, Some("agent/own"), None).is_err(),
            "accepted {arguments:?}"
        );
    }
    let own = Cli::try_parse_from(["st3", "work", "wake", "step-run/x/y", "--as", "agent/own"])
        .unwrap();
    assert!(guard_mutating_cli_actor(&own.command, Some("agent/own"), None).is_ok());
    assert!(guard_mutating_cli_actor(&own.command, None, None).is_ok());
}

#[test]
fn a_claim_prints_each_host_document_under_its_reference() {
    let documents = [(
        "doc/hosts/example@abc".to_owned(),
        "# Example host\n\n- Services run through systemd.\n".to_owned(),
    )];
    assert_eq!(
        render_host_facts("example", &documents, OutputStyle::plain()),
        "\nHOST  example\n  doc/hosts/example@abc\n    # Example host\n\n    - Services run through systemd.\n"
    );
    assert_eq!(render_host_facts("example", &[], OutputStyle::plain()), "");
}

#[test]
fn pi_family_session_context_carries_saved_context_and_no_instructions() {
    assert_eq!(pi_family_session_context("fleet/example/omp", " \n"), "");
    assert_eq!(
        pi_family_session_context("fleet/example/omp", "Resume the release notes.\n"),
        "<context source=\"st3/context/now.md\" agent=\"fleet/example/omp\">\nResume the release notes.\n</context>"
    );
}

#[test]
fn a_pi_family_channel_carries_its_frames_across_a_replacement() {
    let mut state = PiChannelResume {
        incarnation: "1:one".into(),
        session: "session".into(),
        ..PiChannelResume::default()
    };
    state
        .lines
        .push(b"{\"type\":\"state\",\"state\":\"idle\"}\n{\"type\":\"deliv");
    while let Some(line) = state.lines.next_line() {
        assert!(state.accept_frame(&line));
    }
    assert!(state.first_idle_seen);
    assert_eq!(state.pending.state, Some(("idle".to_owned(), 1)));
    state.delivered.insert("message/one".into());

    // The next image reads everything back, including the partial frame.
    let mut resumed: PiChannelResume =
        serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
    resumed
        .lines
        .push(b"ered\",\"meta\":{\"messageId\":\"message/one\"}}\n");
    let line = resumed.lines.next_line().unwrap();
    assert!(resumed.accept_frame(&line));
    assert!(resumed.pending.acknowledgements.contains("message/one"));
    assert!(resumed.delivered.contains("message/one"));
    assert_eq!(resumed.frame_sequence, 1);
    // A handoff failure below the retry limit makes the message deliverable again.
    assert!(!resumed.accept_frame(r#"{"type":"failed","meta":{"messageId":"message/one"}}"#));
    assert!(!resumed.delivered.contains("message/one"));
}

#[test]
fn pi_family_channel_logs_unhandled_frames_without_claiming_or_panicking() {
    #[derive(Clone)]
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    for frame_type in ["future_frame", "pre_compact", "timeline", "context", "turn"] {
        let capture = Capture(Default::default());
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let mut state = PiChannelResume::default();
        tracing::subscriber::with_default(subscriber, || {
            assert!(
                !accept_managed_channel_frame(
                    &mut state,
                    &mut None,
                    &json!({"type": frame_type, "private_payload": "not logged"}).to_string(),
                    "agent/test/seat",
                )
                .unwrap()
            );
        });
        let output = String::from_utf8(
            capture.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone(),
        ).unwrap();
        assert!(
            output.contains("unhandled pi-family channel frame"),
            "{output}"
        );
        assert!(
            output.contains(&format!("frame_type=\"{frame_type}\"")),
            "{output}"
        );
        assert!(!output.contains("not logged"), "{output}");
        assert!(state.pending.state.is_none());
        assert!(state.pending.native_session.is_none());
    }
}

#[tokio::test]
async fn native_mailbox_replays_delivered_unread_but_never_queues_read_or_closed_mail()
{
    // The two providers share this pump but load their own native receipt ledgers.
    for driver in ["codex", "opencode"] {
        let root = tempfile::tempdir().unwrap();
        let agent_dir = root.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        st_drivers::push_mailbox::register(&agent_dir);
        let client = Client::new(Endpoint::Unix(root.path().join("absent-daemon.sock")));
        let mut view = MessageView {
            subject: "message/legacy".into(),
            from: "person/eval".into(),
            to: "agent/eval.worker".into(),
            content: "Recovered signal".into(),
            status: "delivered".into(),
            title: None,
            in_reply_to: None,
            tags: Vec::new(),
            body_ref: None,
            body_bytes: None,
            body_origin: None,
            created_index: 1,
            attachments: Vec::new(),
        };
        let mut mailbox = NativeMailbox {
            subscription: None,
            fence: st3::mailbox::Fence::new(&view.to, "new-incarnation", "delivery"),
            messages: vec![view.clone()],
            queued: BTreeMap::new(),
            replayed: true,
            last_title_warning: None,
        };
        view.subject = "message/pending".into();
        view.status = "staged".into();
        view.content = "Fresh signal".into();
        mailbox.messages.push(view.clone());
        for status in ["read", "closed"] {
            let mut settled = view.clone();
            settled.subject = format!("message/{status}");
            settled.status = status.into();
            mailbox.messages.push(settled);
        }
        for _ in 0..3 {
            let receipts = if driver == "codex" {
                NativeDeliveryReceipts::Codex {
                    state_dir: root.path(),
                    identity: "eval.worker",
                    runtime_id: "worker",
                }
            } else {
                NativeDeliveryReceipts::OpenCode {
                    session_dir: root.path(),
                    identity: "eval.worker",
                    runtime_id: "worker",
                }
            };
            // Replay requires no backward staging or invented consumption receipt.
            mailbox.pump(&client, &agent_dir, receipts).await.unwrap();
            let queued = st_drivers::push_mailbox::messages(
                &agent_dir,
                &agent_dir.join("resources/inbox"),
            )
            .unwrap();
            let mut ids = queued.iter().map(|message| message.filename.as_str()).collect::<Vec<_>>();
            ids.sort_unstable();
            assert_eq!(ids, ["message/legacy", "message/pending"], "{driver}");
        }
        assert!(!agent_dir.join("resources").exists());
    }
}

#[tokio::test]
async fn pi_family_pending_read_survives_daemon_outage_and_reexec_across_both_transports() {
    use axum::{Json, Router, routing::post};
    for fenced in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("daemon.sock");
        let client = Client::new(st3::client::Endpoint::Unix(path.clone()));
        let mut fence = st3::mailbox::Fence::new("agent/eval.worker", "session-1", "delivery");
        fence.epoch = 7; // The daemon's already-allocated binding, carried through exec.
        let mut state = PiChannelResume {
            incarnation: "session-1".into(),
            session: "native-session".into(),
            pending: PiFamilyReports {
                fence: fenced.then_some(fence.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        state.accept_frame(r#"{"type":"read","meta":{"messageId":"message/native"}}"#);
        assert!(
            state
                .pending
                .publish(
                    &client,
                    &fence.subject,
                    "omp",
                    &fence.incarnation,
                    "native-session"
                )
                .await
                .is_err()
        );
        assert!(state.pending.reads.contains("message/native"));
        assert!(state.pending.acknowledgements.contains("message/native"));
        let resume_path =
            st_drivers::reexec::write_state(root.path(), "channel-resume", &state).unwrap();
        let mut resumed: PiChannelResume = st_drivers::reexec::read_state(&resume_path).unwrap();
        assert_eq!(
            serde_json::to_value(&resumed.pending.fence).unwrap(),
            serde_json::to_value(fenced.then_some(&fence)).unwrap()
        );
        let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = received.clone();
        let expected = serde_json::to_value(&fence).unwrap();
        let store = std::sync::Arc::new(
            st3::store::Store::open(&root.path().join("graph.db"), "node").unwrap(),
        );
        store
            .append_claim(&ClaimInput {
                subject: "message/native".into(),
                kind: "message.sent".into(),
                actor: Some("person/eval".into()),
                fields: BTreeMap::from([
                    ("status".into(), json!("sent")),
                    ("from".into(), json!("person/eval")),
                    ("to".into(), json!(fence.subject)),
                    ("content".into(), json!("QUARTZ SIGNAL")),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("read-resume-send".into()),
            })
            .unwrap();
        let graph = store.clone();
        let handler = post(move |Json(receipt): Json<Value>| {
            let received = captured.clone();
            let expected = expected.clone();
            let store = graph.clone();
            async move {
                let lifecycle = receipt["lifecycle"].as_str().unwrap().to_owned();
                let (message, actor) = if fenced {
                    let receipt: st3::mailbox::Receipt = serde_json::from_value(receipt).unwrap();
                    assert_eq!(serde_json::to_value(&receipt.fence).unwrap(), expected);
                    (receipt.message, receipt.fence.subject)
                } else {
                    assert_eq!(receipt["actor"], "agent/eval.worker");
                    ("message/native".into(), "agent/eval.worker".into())
                };
                received.lock().unwrap().push(lifecycle.clone());
                let record = store
                    .append_claim(&ClaimInput {
                        subject: message,
                        kind: format!("message.{lifecycle}"),
                        actor: Some(actor),
                        fields: BTreeMap::from([("status".into(), json!(lifecycle))]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: None,
                    })
                    .unwrap();
                Json(json!({"api_version":"st3.v1", "value":record}))
            }
        });
        let app = Router::new()
            .route("/v1/mailbox/receipts", handler.clone())
            .route("/v1/messages/{message_id}/claims", handler);
        let server_path = path.clone();
        let server = tokio::spawn(async move {
            st3::api::serve_unix(&server_path, app).await.unwrap();
        });
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        resumed
            .pending
            .publish(
                &client,
                &fence.subject,
                "omp",
                &fence.incarnation,
                "native-session",
            )
            .await
            .unwrap();
        assert!(resumed.pending.reads.is_empty());
        assert!(resumed.pending.acknowledgements.is_empty());
        assert_eq!(*received.lock().unwrap(), vec!["delivered", "read"]);
        assert_eq!(
            store.message("message/native").unwrap().unwrap().status,
            "read"
        );
        assert!(!root.path().join("resources").exists());
        server.abort();
    }
}
#[test]
fn a_human_ask_survives_channel_replacement_until_an_answered_state_frame() {
    let mut state = PiChannelResume::default();
    assert!(state.accept_frame(
        r#"{"type":"state","state":"active","blockedOn":"human","ask":"question","reason":"Which deployment target?"}"#,
    ));
    let mut resumed: PiChannelResume =
        serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
    assert_eq!(resumed.pending.state, Some(("working".into(), 1)));
    assert_eq!(resumed.pending.blocked_on.as_deref(), Some("human"));
    assert_eq!(resumed.pending.ask.as_deref(), Some("question"));
    assert_eq!(resumed.pending.reason.as_deref(), Some("Which deployment target?"));
    // The extension only emits an unblocked state for the matching ask's result.
    // An unrelated tool result is timeline data, not a new harness observation.
    assert!(!resumed.accept_frame(
        r#"{"type":"timeline","event":"tool_result","payload":{"toolCallId":"unrelated"}}"#,
    ));
    assert_eq!(resumed.pending.blocked_on.as_deref(), Some("human"));
    assert!(resumed.accept_frame(r#"{"type":"state","state":"active"}"#));
    assert_eq!(resumed.pending.state, Some(("working".into(), 2)));
    assert!(resumed.pending.blocked_on.is_none());
    assert!(resumed.pending.ask.is_none());
    assert!(resumed.pending.reason.is_none());
}

#[test]
fn repeated_negative_handoffs_keep_retrying_and_resume_the_backoff() {
    let mut state = PiChannelResume::default();
    for _ in 0..5 {
        state.delivered.insert("message/retry".into());
        state.accept_frame(r#"{"type":"failed","meta":{"messageId":"message/retry"}}"#);
        assert!(!state.delivered.contains("message/retry"));
        assert!(state.retry_after_ms.contains_key("message/retry"));
    }
    assert!(state.failed_diagnostics.contains("message/retry"));
    let mut resumed: PiChannelResume =
        serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
    assert_eq!(state.retry_after_ms, resumed.retry_after_ms);
    resumed.accept_frame(r#"{"type":"delivered","meta":{"messageId":"message/retry"}}"#);
    assert!(!resumed.retry_after_ms.contains_key("message/retry"));
    assert!(!resumed.failed_diagnostics.contains("message/retry"));
    assert!(!resumed.failed_handoffs.contains_key("message/retry"));
}

#[test]
fn a_driver_resume_state_names_its_driver_session_and_loop() {
    let resume = DriverResume {
        driver: "claude".into(),
        subject: "agent/example/worker".into(),
        incarnation: "1:one".into(),
        session: st_drivers::provider_session::DetachedSession::Provider {
            pid: 42,
            session: "token".into(),
            seq: 3,
        },
        loop_state: NativeLoopState {
            paths: None,
            mailbox_fence: None,
            ready: true,
            harness_record_started: true,
            predecessor_harness_record: Some(b"ignored".to_vec()),
            published_timeline: BTreeSet::from(["one:1:1:upsert".to_owned()]),
            delivery_episode: 2,
            claude_attachment_phase: "blocked".into(),
            claude_attachment_episode: 3,
            claude_attachment_pending: None,
            claude_attachment_reconciled: false,
            claude_attachment_terminal: None,
            claude_readiness_fallback: ClaudeReadinessFallback::default(),
            settle: ResumeSettle::resumed(),
        },
    };
    let back: DriverResume =
        serde_json::from_slice(&serde_json::to_vec(&resume).unwrap()).unwrap();
    assert_eq!(back.session, resume.session);
    assert_eq!(back.incarnation, resume.incarnation);
    assert!(back.loop_state.ready);
    assert_eq!(back.loop_state.delivery_episode, 2);
    assert_eq!(back.loop_state.claude_attachment_phase, "blocked");
    assert_eq!(back.loop_state.claude_attachment_episode, 3);
    assert_eq!(
        back.loop_state.published_timeline,
        resume.loop_state.published_timeline
    );
    // A predecessor's harness record is only a startup fence; it never crosses an exec.
    assert!(back.loop_state.predecessor_harness_record.is_none());
    // Nor does a settle: the replacement image draws its own.
    assert!(back.loop_state.settle.0.is_none());
}

#[test]
fn a_delivery_report_names_the_process_and_its_channel() {
    let root = tempfile::tempdir().unwrap();
    let report: Value =
        serde_json::from_str(&native_delivery_report("claude-channel", Some(root.path())))
            .unwrap();
    assert_eq!(report["transport"], "claude-channel");
    assert_eq!(report["pid"], std::process::id());
    assert!(report["image"].is_string());
    assert!(report["channel"].is_null(), "no channel has reported yet");
    let codex: Value =
        serde_json::from_str(&native_delivery_report("app-server", None)).unwrap();
    assert!(codex.get("channel").is_none());
}

#[test]
fn agent_card_todo_preserves_snapshot_and_reports_active_blocked_empty_truncated_stale() {
    let mut agent: st3_client::Agent = serde_json::from_value(serde_json::json!({
        "kind":"agent", "id":"agent/worker", "revision":"one",
        "updated_at":"2026-10-03T09:00:00Z", "name":"Worker",
        "state":"running", "reachability":"local", "runtime_ids":[],
        "todo": {
            "claim_id":"claim/todo", "accepted_at":"2026-10-03T09:00:00Z", "stale":false,
            "snapshot": {
                "harness":"omp", "session_id":"native", "incarnation_id":"one",
                "observed_at":"2026-10-03T09:00:00Z", "source_op":"update",
                "phases":[{"name":"Build", "tasks":[
                    {"content":"Compile", "status":"in_progress", "blocker":null},
                    {"content":"Deploy", "status":"blocked", "blocker":"Approval"},
                    {"content":"Later", "status":"in_progress", "blocker":null}
                ]}],
                "totals":{"pending":4,"in_progress":2,"completed":3,"blocked":1},
                "truncated":true
            }
        }
    })).unwrap();
    let card = render_client_agent(&agent, &[], 0);
    assert!(card.contains("Todo         ▶ Compile · 3/10 done · 1 blocked · truncated\n"));
    assert!(!card.contains("▶ Later"));
    let value = serde_json::to_value(&agent).unwrap();
    assert_eq!(value["todo"]["snapshot"]["phases"][0]["tasks"][1]["blocker"], "Approval");
    assert_eq!(value["todo"]["claim_id"], "claim/todo");
    agent.todo.as_mut().unwrap().snapshot.phases[0].tasks[0].content =
        "\u{1b}[31mCompile\u{1b}[0m\nnext\tstep\u{1b}]0;spoofed title\u{7}\u{8}".into();
    assert!(render_client_agent(&agent, &[], 0)
        .contains("Todo         ▶ Compile next step · 3/10 done · 1 blocked · truncated\n"));
    agent.todo.as_mut().unwrap().snapshot.totals.abandoned = 2;
    assert!(render_client_agent(&agent, &[], 0)
        .contains("3/10 done · 1 blocked · 2 abandoned · truncated"));
    let todo = agent.todo.as_mut().unwrap();
    todo.stale = true;
    todo.snapshot.phases.clear();
    todo.snapshot.truncated = false;
    todo.snapshot.totals = st3_client::HarnessTodoTotals {
        pending: 0, in_progress: 0, completed: 0, blocked: 0, abandoned: 0,
    };
    assert!(render_client_agent(&agent, &[], 0).contains("Todo         0/0 done · 0 blocked · stale\n"));
    agent.todo = None;
    assert!(!render_client_agent(&agent, &[], 0).contains("Todo"));
    assert!(serde_json::to_value(agent).unwrap()["todo"].is_null());
}

#[test]
fn agent_card_shows_the_member_reconcile_fault() {
    let agent: st3_client::Agent = serde_json::from_value(serde_json::json!({
        "kind": "agent", "id": "agent/bad", "revision": "one",
        "updated_at": "2026-09-27T20:04:00Z", "name": "Bad",
        "state": "failed", "reachability": "local", "runtime_ids": [],
        "fault": "render refuses to change tracked file .claude/settings.local.json"
    }))
    .unwrap();
    let card = render_client_agent(&agent, &[], 0);
    assert!(card.contains("STATE        failed"));
    assert!(card.contains(
        "FAULT        render refuses to change tracked file .claude/settings.local.json"
    ));
}

#[test]
fn agent_card_names_a_pending_codex_approval() {
    let agent: st3_client::Agent = serde_json::from_value(serde_json::json!({
        "kind": "agent", "id": "agent/approval", "revision": "one",
        "updated_at": "2026-10-06T12:00:00Z", "name": "Approval",
        "state": "running", "reachability": "local", "runtime_ids": [],
        "driver": "codex", "harness_state": "blocked", "blocked_on": "human",
        "ask": "permission", "reason": "waitingOnApproval"
    })).unwrap();
    let card = render_client_agent(&agent, &[], 0);
    assert!(card.contains("HARNESS      codex · blocked\n"));
    assert!(card.contains("AWAITING     approval\n"));
}

fn subagent_worker() -> serde_json::Value {
    serde_json::json!({
        "kind": "agent", "id": "agent/crew/worker", "revision": "one",
        "updated_at": "2026-10-02T09:00:00Z", "name": "Worker",
        "state": "running", "reachability": "local", "runtime_ids": [],
        "owner_run_id": "mission-run/crew", "harness_state": "working",
        "subagents": [
            {"id": "a1", "subagent_type": "Explore", "description": "map the code",
             "driver": "claude", "work_id": "step-run/crew/build",
             "started_at": "2026-10-02T09:00:00Z",
             "lease_expires_at": "2026-10-02T09:10:00Z"},
            {"id": "019a-thread", "subagent_type": null, "description": null,
             "driver": "codex", "started_at": null,
             "lease_expires_at": "2026-10-02T09:10:00Z"}
        ]
    })
}

#[test]
fn agent_card_lists_the_subagents_its_harness_runs() {
    let st3_client::Resource::Agent(agent) = serde_json::from_value(subagent_worker()).unwrap()
    else {
        panic!("agent resource")
    };
    let started = 1_790_931_600_000_u128;
    let card = render_client_agent(&agent, &[], started + 180_000);
    assert!(
        card.contains(
            "SUBAGENT     map the code · Explore · started 3m ago\nSUBAGENT     019a-thread\n"
        ),
        "{card}"
    );
    // An agent without subagents prints no subagent line.
    let quiet = render_client_agent(
        &st3_client::Agent {
            subagents: Vec::new(),
            ..agent
        },
        &[],
        started,
    );
    assert!(!quiet.contains("SUBAGENT"), "{quiet}");
}

#[test]
fn agent_tree_nests_running_subagents_under_their_agent() {
    let page: ClientPage = serde_json::from_value(serde_json::json!({
        "kind": "page", "collection": "agents",
        "items": [subagent_worker()],
        "page": {"limit": 50, "has_more": false}
    }))
    .unwrap();
    let tree = render_client_agents(&page, true, false, "st agents tree");
    assert_eq!(
        tree,
        "AGENT TREE  1\n\
             └─ crew\n\
             \x20  └─ worker  working · local\n\
             \x20     agent/crew/worker\n\
             \x20     ├─ map the code · Explore\n\
             \x20     └─ 019a-thread\n"
    );
}

#[test]
fn agent_card_shows_current_and_next_work_ids() {
    let resource: st3_client::Resource = serde_json::from_value(serde_json::json!({
        "kind": "agent", "id": "agent/worker", "revision": "one",
        "updated_at": "2026-09-24T09:00:00Z", "name": "Worker",
        "state": "running", "reachability": "local", "runtime_ids": [],
        "current_work_ids": ["step-run/older/work"], "active_work_count": 1,
        "next_work_id": "step-run/newer/review",
        "upcoming_work_ids": ["step-run/newer/review"], "queued_work_count": 1
    }))
    .unwrap();
    let st3_client::Resource::Agent(agent) = resource else {
        panic!("agent resource")
    };
    let card = render_client_agent(&agent, &[], 0);
    assert!(card.contains("CURRENT WORK step-run/older/work"));
    assert!(card.contains("NEXT WORK    step-run/newer/review"));
    assert!(
        !card.contains("PROGRESS"),
        "an unreadable step leaves only its id"
    );
}

#[test]
fn agent_card_shows_the_current_step_and_its_last_progress() {
    let resource: st3_client::Resource = serde_json::from_value(serde_json::json!({
        "kind": "agent", "id": "agent/worker", "revision": "one",
        "updated_at": "2026-09-24T09:00:00Z", "name": "Worker",
        "state": "running", "reachability": "local", "runtime_ids": [],
        "current_work_ids": ["step-run/one/build", "step-run/two/review", "step-run/two/docs"],
        "active_work_count": 3
    }))
    .unwrap();
    let st3_client::Resource::Agent(agent) = resource else {
        panic!("agent resource")
    };
    let step = |subject: &str, title: &str, progress: Option<(&str, u128)>| {
        serde_json::from_value::<StepRunView>(serde_json::json!({
            "subject": subject, "run": "mission-run/demo", "generation": "run-generation/one",
            "step": subject.rsplit('/').next().unwrap(), "definition_hash": "hash",
            "status": "working", "attempt": 1, "assigned_to": "agent/worker",
            "agentless": false, "title": title, "worker_reported": false,
            "claimant": "agent/worker", "claim_incarnation": "worker:1",
            "claim_expires_at_unix_ms": 900_000, "readiness_epoch": 1,
            "blocked_reason": null, "not_before_unix_ms": null,
            "created_at_unix_ms": 0, "updated_at_unix_ms": 0,
            "progress_summary": progress.map(|(summary, _)| summary),
            "progress_at_unix_ms": progress.map(|(_, at)| at),
        }))
        .unwrap()
    };
    let current = [
        step(
            "step-run/one/build",
            "Build the parser",
            Some(("Tests pass\nnext: docs", 60_000)),
        ),
        step("step-run/two/review", "Review the parser", None),
        StepRunView {
            status: "verifying".into(),
            completion_summary: Some("Published the guide".into()),
            ..step(
                "step-run/two/docs",
                "Write the guide",
                Some(("Drafting", 0)),
            )
        },
    ];

    let card = render_client_agent(&agent, &current, 360_000);

    assert!(card.contains(
        "CURRENT WORK step-run/one/build\n\
             CURRENT STEP Build the parser · working\n\
             PROGRESS     Tests pass… · 5m ago\n\
             CURRENT WORK step-run/two/review\n\
             CURRENT STEP Review the parser · working\n\
             PROGRESS     none reported\n\
             CURRENT WORK step-run/two/docs\n\
             CURRENT STEP Write the guide · verifying\n\
             DONE         Published the guide\n"
    ));
}

#[test]
fn a_down_agent_card_says_since_when_and_marks_the_step_its_holder_cannot_work() {
    let resource: st3_client::Resource = serde_json::from_value(serde_json::json!({
        "kind": "agent", "id": "agent/worker", "revision": "one",
        "updated_at": "2026-09-24T09:00:00Z", "name": "Worker",
        "state": "down", "down_since": "2026-09-23T21:00:00.000Z",
        "reachability": "reachable", "runtime_ids": [],
        "current_work_ids": ["step-run/one/build"], "active_work_count": 1,
        "current_work": [{
            "id": "step-run/one/build", "mission_id": "mission/demo",
            "mission_run_id": "mission-run/demo", "path": "build", "state": "claimed",
            "since": "2026-09-23T20:00:00.000Z",
            "holder_down_since": "2026-09-23T21:00:00.000Z"
        }]
    }))
    .unwrap();
    let st3_client::Resource::Agent(agent) = resource else {
        panic!("agent resource")
    };
    let step = serde_json::from_value::<StepRunView>(serde_json::json!({
        "subject": "step-run/one/build", "run": "mission-run/demo",
        "generation": "run-generation/one", "step": "build", "definition_hash": "hash",
        "status": "working", "attempt": 1, "assigned_to": "agent/worker",
        "agentless": false, "title": "Build the parser", "worker_reported": false,
        "claimant": "agent/worker", "claim_incarnation": "worker:1",
        "claim_expires_at_unix_ms": 900_000, "readiness_epoch": 1,
        "blocked_reason": null, "not_before_unix_ms": null,
        "created_at_unix_ms": 0, "updated_at_unix_ms": 0,
        "progress_summary": "Tests pass", "progress_at_unix_ms": 60_000,
    }))
    .unwrap();
    let card = render_client_agent(&agent, &[step], 360_000);
    assert!(card.contains("STATE        down · no session since 2026-09-23T21:00:00.000Z\n"), "{card}");
    assert!(
        card.contains("CURRENT STEP Build the parser · working · held by a seat with no session since 2026-09-23T21:00:00.000Z\n"),
        "{card}"
    );
}

#[test]
fn agent_queue_view_lists_the_claim_then_runs_in_order_and_moves() {
    let queue: st3_client::AgentQueue = serde_json::from_value(serde_json::json!({
        "kind": "agent-queue", "agent_id": "agent/example/worker",
        "current_work_ids": ["step-run/held/build"],
        "next_work_id": "step-run/second/review",
        "runs": [
            {
                "mission_run_id": "mission-run/held", "position": 1, "state": "claimed",
                "run_state": "running", "joined_at": "2026-09-24T09:00:00.000Z",
                "claimed_work_ids": ["step-run/held/build"], "ready_work_ids": [],
                "waiting_work_ids": []
            },
            {
                "mission_run_id": "mission-run/gated", "position": 2, "state": "waiting",
                "run_state": "running", "joined_at": "2026-09-24T09:01:00.000Z",
                "claimed_work_ids": [], "ready_work_ids": [],
                "waiting_work_ids": ["step-run/gated/ship"]
            },
            {
                "mission_run_id": "mission-run/second", "position": 3, "state": "ready",
                "run_state": "running", "joined_at": "2026-09-24T09:02:00.000Z",
                "claimed_work_ids": [],
                "ready_work_ids": ["step-run/second/review", "step-run/second/docs"],
                "waiting_work_ids": []
            }
        ],
        "moves": [{
            "claim_id": "claim-one", "mission_run_id": "mission-run/held",
            "placement": "before", "anchor_run_id": "mission-run/gated",
            "actor_id": "person/operator", "reason": "finish the build first",
            "moved_at": "2026-09-24T09:03:00.000Z"
        }],
        "move_count": 1
    }))
    .unwrap();
    assert_eq!(
        render_agent_queue(&queue),
        "AGENT QUEUE  agent/example/worker\n\
             CURRENT      step-run/held/build\n\
             NEXT WORK    step-run/second/review\n\
             RUNS         3\n  \
             1. mission-run/held  claimed  step-run/held/build\n  \
             2. mission-run/gated  waiting  step-run/gated/ship not ready\n  \
             3. mission-run/second  ready  next step-run/second/review (+1 ready)\n\
             MOVES        1 total\n  \
             2026-09-24T09:03:00.000Z  person/operator moved mission-run/held before \
             mission-run/gated: finish the build first\n"
    );
}

#[tokio::test]
async fn a_pi_family_delivery_after_the_recipient_read_the_message_keeps_the_channel() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let store = Arc::new(Store::open_memory("pi-delivery-test").unwrap());
    let seat = "agent/run-1/wake.omp";
    let claim =
        |subject: &str, kind: &str, actor: &str, fields: Vec<(&str, &str)>| ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: Some(actor.into()),
            fields: fields
                .into_iter()
                .map(|(key, value)| (key.to_owned(), Value::String(value.into())))
                .collect(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        };
    for subject in ["message/held", "message/pending"] {
        store
            .append_claim(&claim(
                subject,
                "message.sent",
                "agent/run-1/wake.codex",
                vec![
                    ("from", "agent/run-1/wake.codex"),
                    ("to", seat),
                    ("content", "AGREEMENT EMBER+ORBIT"),
                    ("status", "sent"),
                ],
            ))
            .unwrap();
    }
    // The seat read the held message through the CLI, which records delivery and the read.
    for lifecycle in ["delivered", "read"] {
        store
            .append_claim(&claim(
                "message/held",
                &format!("message.{lifecycle}"),
                seat,
                vec![("status", lifecycle)],
            ))
            .unwrap();
    }
    let state = AppState {
        store: store.clone(),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "pi-delivery-test".into(),
        state_dir: root.path().to_path_buf(),
        pty_root: root.path().join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: PlannerSpec::default(),
    };
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        serve_unix(&server_socket, router(state)).await.unwrap();
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(socket.exists(), "the test API socket did not start");
    let client = Client::unix(&socket);

    // The channel can lose a race to the recipient's CLI read without ending its loop.
    assert!(
        !stage_pi_family_message(&client, "message/held", seat, "omp")
            .await
            .unwrap()
    );
    assert!(
        stage_pi_family_message(&client, "message/pending", seat, "omp")
            .await
            .unwrap()
    );
    assert_eq!(
        store.message("message/pending").unwrap().unwrap().status,
        "staged"
    );

    // The recipient's late acknowledgement settles to its existing read evidence.
    deliver_message(&client, "message/held", seat, "late".into())
        .await
        .unwrap();
    // Receipt replay must not end the channel or cause another handoff.
    acknowledge_pi_family_delivery(&client, seat, "message/held")
        .await
        .unwrap();
    assert_eq!(
        store.message("message/held").unwrap().unwrap().status,
        "read"
    );
    acknowledge_pi_family_delivery(&client, seat, "message/pending")
        .await
        .unwrap();
    assert_eq!(
        store.message("message/pending").unwrap().unwrap().status,
        "delivered"
    );
    // A message that does not exist is still an error.
    assert!(
        acknowledge_pi_family_delivery(&client, seat, "message/absent")
            .await
            .is_err()
    );
    server.abort();
}

#[tokio::test]
async fn trace_after_index_reads_the_first_bounded_page() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let store = Arc::new(Store::open_memory("trace-cursor-test").unwrap());
    let indexes = (0..5)
        .map(|number| {
            store
                .append_claim(&ClaimInput {
                    subject: "host/trace-cursor-test".into(),
                    kind: "transport.observed".into(),
                    actor: None,
                    fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("trace-cursor-test-{number}")),
                })
                .unwrap()
                .store_index
        })
        .collect::<Vec<_>>();
    let state = AppState {
        store,
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "trace-cursor-test".into(),
        state_dir: root.path().to_path_buf(),
        pty_root: root.path().join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: PlannerSpec::default(),
    };
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        serve_unix(&server_socket, router(state)).await.unwrap();
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(socket.exists(), "the test API socket did not start");
    let client = Client::unix(&socket);
    let mut args = TraceArgs {
        subject: Some("host/trace-cursor-test".into()),
        owner_run: None,
        limit: 2,
        after_index: Some(indexes[0]),
        follow: false,
    };

    let after = trace_claims(&client, &args).await.unwrap();
    assert_eq!(
        after
            .iter()
            .map(|claim| claim.store_index)
            .collect::<Vec<_>>(),
        indexes[1..3]
    );

    args.after_index = None;
    let recent = trace_claims(&client, &args).await.unwrap();
    assert_eq!(
        recent
            .iter()
            .map(|claim| claim.store_index)
            .collect::<Vec<_>>(),
        indexes[3..5]
    );
    server.abort();
}

#[tokio::test]
async fn trace_wait_starts_after_existing_events() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let store = Arc::new(Store::open_memory("wait-cursor-test").unwrap());
    let mut last_index = 0;
    for number in 0..3 {
        last_index = store
            .append_claim(&ClaimInput {
                subject: "host/wait-cursor-test".into(),
                kind: "transport.observed".into(),
                actor: None,
                fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("wait-cursor-test-{number}")),
            })
            .unwrap()
            .store_index;
    }
    let state = AppState {
        store,
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "wait-cursor-test".into(),
        state_dir: root.path().to_path_buf(),
        pty_root: root.path().join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: PlannerSpec::default(),
    };
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let app = router(state).layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let sent = sent.clone();
            async move {
                if request.uri().path() == "/v1/events/page" {
                    let _ = sent.send(request.uri().query().unwrap_or_default().to_owned());
                }
                next.run(request).await
            }
        },
    ));
    let server = tokio::spawn(async move {
        serve_unix(&socket, app).await.unwrap();
    });
    let path = root.path().join("st3.sock");
    for _ in 0..100 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let client = Client::unix(&path);
    let waiter = tokio::spawn(async move {
        wait_for_condition(&client, "host/wait-cursor-test", "completed", None).await
    });
    let query = tokio::time::timeout(Duration::from_secs(2), received.recv())
        .await
        .expect("the wait did not request events")
        .expect("the server stopped before the event request");
    let after = query
        .split('&')
        .find_map(|part| part.strip_prefix("after="))
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!(
        after >= last_index,
        "the wait replayed existing events: {query}"
    );
    waiter.abort();
    server.abort();
}

#[test]
fn conversation_follow_is_explicit_and_bounded() {
    let cli = Cli::try_parse_from([
        "st3",
        "conversations",
        "follow",
        "session/remote-agent",
        "--limit",
        "25",
    ])
    .unwrap();
    let Command::Conversations {
        command:
            MessageCommand::Follow {
                session,
                actor,
                limit,
            },
    } = cli.command
    else {
        panic!("the conversation follow command did not parse");
    };
    assert_eq!(session, "session/remote-agent");
    assert_eq!(actor, None);
    assert_eq!(limit, 25);
}

#[test]
fn conversation_follow_emits_new_entries_and_revisions_once() {
    let entry = |id: &str, sequence: u64, revision: u32| {
        serde_json::from_value::<ClientTimelineEntry>(json!({
            "id": id,
            "sequence": sequence,
            "revision": revision,
            "timestamp": "2026-09-24T15:00:00Z",
            "role": "assistant",
            "final": true,
            "type": "content",
            "body": {"media_type":"text/plain","text":"hello","attachment_id":null}
        }))
        .unwrap()
    };
    let mut seen = BTreeMap::new();
    let initial = vec![entry("timeline-entry/a", 1, 1)];
    assert_eq!(unseen_timeline_entries(&initial, &mut seen), initial);
    assert!(unseen_timeline_entries(&initial, &mut seen).is_empty());
    let changed = vec![
        entry("timeline-entry/b", 2, 1),
        entry("timeline-entry/a", 1, 2),
    ];
    assert_eq!(
        unseen_timeline_entries(&changed, &mut seen),
        vec![changed[1].clone(), changed[0].clone()]
    );
    assert!(unseen_timeline_entries(&changed, &mut seen).is_empty());
}

#[test]
fn every_cli_command_has_a_valid_help_surface() {
    fn visit(command: &clap::Command, path: &[String]) {
        if !command.is_hide_set() {
            assert!(
                command.get_about().is_some(),
                "{} has no human job or reason",
                path.join(" ")
            );
        }
        let mut help = command.clone();
        assert!(
            !help.render_long_help().to_string().trim().is_empty(),
            "{} has empty help",
            path.join(" ")
        );
        let subcommands = command.get_subcommands().cloned().collect::<Vec<_>>();
        for subcommand in subcommands {
            let mut child_path = path.to_vec();
            child_path.push(subcommand.get_name().to_owned());
            let mut argv = child_path.clone();
            argv.push("--help".into());
            let error = match Cli::try_parse_from(argv) {
                Ok(_) => panic!("each command path must accept --help"),
                Err(error) => error,
            };
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp,
                "{} did not render help",
                child_path.join(" ")
            );
            visit(&subcommand, &child_path);
        }
    }

    let command = Cli::command();
    command.clone().debug_assert();
    visit(&command, &["st".into()]);
}

#[test]
fn top_level_surface_exactly_matches_the_pristine_v0_inventory() {
    let contract: Value = serde_json::from_str(include_str!(
        "../../../docs/st3/operational-state/cli-commands.json"
    ))
    .unwrap();
    let expected = contract["canonical_roots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let command = Cli::command();
    let visible = command
        .get_subcommands()
        .filter(|subcommand| !subcommand.is_hide_set())
        .map(|subcommand| subcommand.get_name().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(visible, expected);

    let mut help = command.clone();
    let help = help.render_long_help().to_string();
    assert!(help.contains("Usage: st ["), "{help}");
    assert!(
        command
            .find_subcommand("claude-channel")
            .is_some_and(|command| command.is_hide_set()),
        "the expert ST3 channel lifecycle must remain callable but hidden"
    );

    for legacy in [
        "claude",
        "codex",
        "preview",
        "planning",
        "mission",
        "publish",
        "exec",
        "logs",
        "pty",
        "inspect",
        "wait",
        "doc",
        "eval",
        "graph",
        "status",
        "runtime",
        "context",
        "resource",
        "review",
        "message",
        "gate-result",
    ] {
        assert!(
            command.find_subcommand(legacy).is_none(),
            "legacy root `{legacy}` remains public"
        );
    }
}

#[test]
fn removed_legacy_handlers_are_absent_from_cli_and_server_sources() {
    // The CLI's source is main.rs and the command files it declares (`mod NAME_cli;`); the
    // list must name every one, so a legacy handler cannot come back in a command file.
    let command_files = [
        ("agents", include_str!("agents_cli.rs")),
        ("attention", include_str!("attention_cli.rs")),
        ("change", include_str!("change_cli.rs")),
        ("conversations", include_str!("conversations_cli.rs")),
        ("doctor", include_str!("doctor_cli.rs")),
        ("driver", include_str!("driver_cli.rs")),
        ("fleet", include_str!("fleet_cli.rs")),
        ("launch", include_str!("launch_cli.rs")),
        ("replication", include_str!("replication_cli.rs")),
        ("terminals", include_str!("terminals_cli.rs")),
        ("uninstall", include_str!("uninstall_cli.rs")),
        ("work", include_str!("work_cli.rs")),
    ];
    let main_source = include_str!("main.rs");
    let declared: BTreeSet<&str> = main_source
        .lines()
        .filter_map(|line| line.strip_prefix("mod ")?.strip_suffix("_cli;"))
        .collect();
    assert_eq!(
        declared,
        command_files.iter().map(|(name, _)| *name).collect::<BTreeSet<_>>(),
        "every command file main.rs declares is scanned"
    );
    let cli_source = std::iter::once(main_source)
        .chain(command_files.iter().map(|(_, source)| *source))
        .collect::<String>();
    for handler in [
        "run_preview",
        "run_exec",
        "run_logs",
        "run_status",
        "run_runtime",
        "run_context",
        "run_resource",
        "run_review",
        "run_gate_result",
        "run_eval",
        "run_graph",
        "run_quick",
    ] {
        assert!(
            !cli_source.contains(&format!("fn {handler}(")),
            "legacy CLI handler {handler} remains compiled"
        );
    }
    for command_type in [
        "QuickArgs",
        "PublishArgs",
        "ExecArgs",
        "LogsArgs",
        "StatusArgs",
        "RuntimeCommand",
        "ContextCommand",
        "ResourceCommand",
        "ReviewCommand",
        "GateResultArgs",
        "EvalArgs",
        "GraphArgs",
    ] {
        assert!(
            !cli_source.contains(&format!("struct {command_type}"))
                && !cli_source.contains(&format!("enum {command_type}")),
            "legacy CLI type {command_type} remains compiled"
        );
    }

    let server_source = include_str!("../../st3-daemon/src/api.rs");
    for handler in [
        "watch_resource",
        "unwatch_resource",
        "refresh_resource",
        "reset_runtime",
        "quick_claude",
        "quick_codex",
        "start_mission_run",
    ] {
        assert!(
            !server_source.contains(&format!("fn {handler}(")),
            "legacy server handler {handler} remains compiled"
        );
    }
}

fn fixture_product_page(kinds: &[&str], has_more: bool) -> ClientPage {
    let resources: Vec<Value> = serde_json::from_str(include_str!(
        "../../../docs/st3/client-v0/fixtures/resources.json"
    ))
    .unwrap();
    let items = resources
        .into_iter()
        .filter(|resource| {
            resource["kind"]
                .as_str()
                .is_some_and(|kind| kinds.contains(&kind))
        })
        .map(|resource| serde_json::from_value(resource).unwrap())
        .collect();
    ClientPage {
        kind: "resource-page".into(),
        collection: "fixture".into(),
        filters: BTreeMap::new(),
        items,
        page: st3_client::PageInfo {
            limit: 100,
            has_more,
            next_cursor: has_more.then(|| "cursor/next".into()),
            cursor_expires_at: None,
        },
        sync: None,
        replicated: None,
    }
}

#[test]
fn replication_status_names_shared_table_differences() {
    let mut peer = peer_status("alder", None);
    peer.projection_digests
        .insert("planning_sessions".into(), "different".into());
    peer.differing_tables = vec!["documents".into(), "planning_sessions".into()];
    let rendered = render_replication_peers(&[peer], "local", 0);
    assert!(rendered.contains("shared tables differ: documents, planning_sessions"));
}

#[test]
fn replication_status_renders_the_fabric_grant_reason() {
    let mut peer = peer_status("cobalt", None);
    peer.status = "refused".into();
    peer.refusal_reason =
        Some("refused by that member's Fabric grants (service st3-peer-v1)".into());
    let output = render_replication_peers(&[peer], "local", 2000);
    assert!(output.contains("peer\tcobalt\trefused\trefused by that member's Fabric grants"));
    assert!(!output.contains("down"));
}

#[test]
fn replication_status_says_not_catching_up_when_nothing_closes_the_gap() {
    let now = 1_000_000;
    let peer = |sync: st3::model::ReplicationPeerSync| ReplicationPeerStatus {
        last_successful_exchange: None,
        last_failed_exchange: None,
        worker: None,
        projection_digests: BTreeMap::new(),
        incompatible_projection_tables: Vec::new(),
        differing_tables: Vec::new(),
        projection_comparison_waiting: false,
        projection_comparison_waiting_reason: None,
        projection_graphs_equal: None,
        projection_compared_at_unix_ms: None,
        peer: "ExampleMac".into(),
        status: "up".into(),
        last_success_at_unix_ms: Some(now - 2_000),
        last_error: None,
        last_failure_at_unix_ms: None,
        refusal_reason: None,
        schema_digest: None,
        projection_registry_digest: None,
        projection_versions: Default::default(),
        authority_digest: None,
        graph_digest: None,
        sync: Some(sync),
    };
    let behind = |receive: Option<f64>, catch_up: Option<f64>, eta: Option<u64>| {
        st3::model::ReplicationPeerSync {
            peer_only_envelopes: 14_652,
            measured_at_unix_ms: now - 2_000,
            receive_rate_per_second: receive,
            catch_up_rate_per_second: catch_up,
            estimated_catch_up_seconds: eta,
            catching_up: true,
            ..Default::default()
        }
    };
    let summary = |estimate: &str| {
        format!(
            "sync\tcatching up: ExampleMac has 14,652 envelopes this node lacks, {estimate} (arrival only)\n"
        )
    };
    let render = |sync| render_replication_peers(&[peer(sync)], "local", now);
    // Stalled: the count fell without receipts, or the gap grew. Neither closes it.
    for sync in [
        behind(Some(0.0), Some(122.0), None),
        behind(Some(0.1), Some(0.0), None),
    ] {
        let output = render(sync);
        assert!(output.contains(&summary("not catching up")), "{output}");
        assert!(
            output.contains("not catching up (arrival only; measured 2s ago)"),
            "{output}"
        );
        assert!(!output.contains("estimating time to catch up"), "{output}");
        assert!(!output.contains("closing at"), "{output}");
    }
    // Stale: view() returns no rates or forecast for a measurement gone quiet.
    let output = render(st3::model::ReplicationPeerSync {
        stale: true,
        ..behind(None, None, None)
    });
    assert!(output.contains(&summary("estimating time to catch up")), "{output}");
    assert!(output.contains("  stale: no exchange since the measurement below"), "{output}");
    assert!(
        output.contains("  estimating time to catch up (arrival only; measured 2s ago)"),
        "{output}"
    );
    assert!(!output.contains("receiving") && !output.contains("closing at"), "{output}");
    // Positive: the forecast names the closing rate it divides by.
    let output = render(behind(Some(142.5), Some(140.0), Some(889)));
    assert!(output.contains(&summary("caught up in about 15m")), "{output}");
    assert!(
        output.contains(
            "  receiving 142.5 envelopes/s, closing at 140.0/s, caught up in about 15m \
                 (arrival only; measured 2s ago)"
        ),
        "{output}"
    );
    // Nothing missing: no catching-up summary and no promise of a time.
    let nothing_missing = st3::model::ReplicationPeerSync {
        peer_only_envelopes: 0,
        local_only_envelopes: 3,
        catching_up: false,
        estimated_catch_up_seconds: Some(0),
        ..behind(Some(0.0), Some(0.0), Some(0))
    };
    let output = render(nothing_missing.clone());
    assert!(!output.contains("catching up"), "{output}");
    assert!(!output.contains("caught up in") && !output.contains("closing at"), "{output}");
    assert!(!output.contains("nothing was missing at the last measurement"), "{output}");
    // The same measurement gone stale: view() withdrew its rates and its zero forecast, and
    // the status says only what the last measurement found, whether or not this node held
    // envelopes the peer lacked. The graph comparison lines are unchanged.
    for local_only in [3, 0] {
        let output = render(st3::model::ReplicationPeerSync {
            stale: true,
            local_only_envelopes: local_only,
            receive_rate_per_second: None,
            catch_up_rate_per_second: None,
            estimated_catch_up_seconds: None,
            ..nothing_missing.clone()
        });
        assert!(
            output.contains("  stale: no exchange since the measurement below (2s ago)"),
            "{output}"
        );
        assert!(
            output.contains(
                "  nothing was missing at the last measurement (arrival only; measured 2s ago)"
            ),
            "{output}"
        );
        assert!(!output.contains("estimating time to catch up"), "{output}");
        assert!(
            !output.contains("caught up in about") && !output.contains("catching up"),
            "{output}"
        );
    }
}

#[test]
fn replication_status_says_which_side_holds_what_and_how_long_catching_up_takes() {
    let now = 1_000_000;
    let local = "1111111111111111aaaa";
    let peer =
        |name: &str, graph: Option<&str>, sync: Option<st3::model::ReplicationPeerSync>| {
            ReplicationPeerStatus {
                last_successful_exchange: None,
                last_failed_exchange: None,
                worker: None,
                projection_digests: BTreeMap::from([("claim_sources".into(), "sample".into())]),
                incompatible_projection_tables: Vec::new(),
                differing_tables: Vec::new(),
                projection_comparison_waiting: false,
                projection_comparison_waiting_reason: None,
                projection_graphs_equal: None,
                projection_compared_at_unix_ms: None,
                peer: name.into(),
                status: "up".into(),
                last_success_at_unix_ms: Some(now - 2_000),
                last_error: None,
                last_failure_at_unix_ms: None,
                refusal_reason: None,
                schema_digest: None,
                projection_registry_digest: None,
                projection_versions: Default::default(),
                authority_digest: None,
                graph_digest: graph.map(str::to_owned),
                sync,
            }
        };
    let same_envelopes = |compared: Option<u128>, differs: Option<u128>, diverged| {
        Some(st3::model::ReplicationPeerSync {
            measured_at_unix_ms: now,
            graph_compared_at_unix_ms: compared,
            graph_differs_since_unix_ms: differs,
            diverged,
            ..Default::default()
        })
    };
    let output = render_replication_peers(
        &[
            peer(
                "ExampleMac",
                Some("3333333333333333"),
                Some(st3::model::ReplicationPeerSync {
                    peer_only_envelopes: 124_384,
                    local_only_envelopes: 3,
                    measured_at_unix_ms: now - 2_000,
                    receive_rate_per_second: Some(142.5),
                    catch_up_rate_per_second: Some(140.0),
                    estimated_catch_up_seconds: Some(889),
                    catching_up: true,
                    ..Default::default()
                }),
            ),
            peer("Quiet", Some(local), same_envelopes(Some(now), None, false)),
            peer(
                "Moved",
                Some("4444444444444444"),
                same_envelopes(Some(now), None, false),
            ),
            peer(
                "Settling",
                Some("5555555555555555"),
                same_envelopes(Some(now), Some(now - 10_000), false),
            ),
            peer(
                "Laptop",
                Some("2222222222222222bbbb"),
                same_envelopes(Some(now - 1_000), Some(now - 180_000), true),
            ),
            peer("Fresh", None, None),
        ],
        local,
        now,
    );
    assert_eq!(
        output,
        "sync\tdiverged: Laptop holds the same envelopes but projects a different graph, \
             since 3m ago\n\
             sync\tcatching up: ExampleMac has 124,384 envelopes this node lacks, \
             caught up in about 15m (arrival only)\n\
             peer\tExampleMac\tup\t\n\
             \x20 last seen 2s ago\n\
             \x20 ExampleMac has 124,384 envelopes this node lacks\n\
             \x20 this node has 3 envelopes ExampleMac lacks\n\
             \x20 receiving 142.5 envelopes/s, closing at 140.0/s, caught up in about 15m \
             (arrival only; measured 2s ago)\n\
             peer\tQuiet\tup\t\n\
             \x20 last seen 2s ago\n\
             \x20 in sync: the same envelopes and the same graph (measured now, graphs compared now)\n\
             peer\tMoved\tup\t\n\
             \x20 last seen 2s ago\n\
             \x20 same envelopes (measured now), but the graphs differ (this node \
             111111111111, Moved 444444444444; compared now); the next exchange compares them\n\
             peer\tSettling\tup\t\n\
             \x20 last seen 2s ago\n\
             \x20 graphs differ: the same envelopes project different graphs since 10s ago \
             (compared now; this node 111111111111, Settling 555555555555)\n\
             \x20 diverged if this lasts a minute; a peer still projecting settles by itself\n\
             peer\tLaptop\tup\t\n\
             \x20 last seen 2s ago\n\
             \x20 diverged: the same envelopes project different graphs since 3m ago \
             (compared 1s ago; this node 111111111111, Laptop 222222222222)\n\
             \x20 exchanges cannot fix this; one node projects the same claims wrongly, and its views are wrong until that is fixed\n\
             peer\tFresh\tup\t\n\
             \x20 last seen 2s ago\n\
             \x20 difference not measured yet\n"
    );
}

#[test]
fn a_catching_up_page_leads_with_how_far_behind_this_host_is() {
    let mut page = fixture_product_page(&["attention"], false);
    page.sync = Some(st3_client::SyncNotice {
        state: "catching-up".into(),
        peers: vec![st3_client::SyncPeer {
            last_successful_exchange: None,
            last_failed_exchange: None,
            host_id: "host/ExampleMac".into(),
            peer_only_envelopes: 1,
            local_only_envelopes: 0,
            last_exchange_at: Some("1970-01-01T00:16:38Z".into()),
            estimated_catch_up_seconds: None,
            diverged_since: None,
        }],
    });
    let output = render_now_page(&page, "st3 now --as person/alex");
    assert!(
        output.starts_with(
            "SYNCING  ExampleMac has 1 envelope this host lacks · estimating time to catch up · \
                 last exchange "
        ),
        "{output}"
    );
    assert!(
        output.contains("items below can be out of date"),
        "{output}"
    );
    assert_eq!(
        render_sync_notice(page.sync.as_ref().unwrap(), 1_000_000),
        "SYNCING  ExampleMac has 1 envelope this host lacks · estimating time to catch up · \
             last exchange 2s ago\n  Until then, items below can be out of date. \
             Progress: st3 replication status\n\n"
    );
    assert_eq!(catch_up_estimate(Some(0)), "caught up");
    assert_eq!(catch_up_estimate(Some(59)), "caught up in under a minute");
    assert_eq!(catch_up_estimate(Some(3_601)), "caught up in about 1h 1m");
    assert_eq!(catch_up_estimate(Some(90_000)), "caught up in about 1d 1h");
    assert_eq!(envelope_count(1_234_567), "1,234,567 envelopes");

    // A diverged peer outranks catching up: exchanges cannot fix what the page shows.
    let sync = page.sync.as_mut().unwrap();
    sync.state = "diverged".into();
    sync.peers[0].diverged_since = Some("1970-01-01T00:13:40Z".into());
    assert_eq!(
        render_sync_notice(page.sync.as_ref().unwrap(), 1_000_000),
        "DIVERGED  ExampleMac projects a different graph from the same envelopes · since 3m ago\n\
             \x20 Exchanges cannot fix this, so items below can be wrong. \
             Details: st3 replication status\n\n"
    );

    page.sync = None;
    assert!(!render_now_page(&page, "st3 now").contains("SYNCING"));
}

#[test]
fn mission_list_counts_active_and_finished_runs_apart() {
    let mut page = fixture_product_page(&["mission"], false);
    let ClientResource::Mission(mission) = &mut page.items[0] else {
        panic!("expected mission fixture");
    };
    mission.runs = (1..=6).map(|run| format!("mission-run/r{run}")).collect();
    let render = |active: Option<usize>, runs: usize| {
        let mut mission = mission.clone();
        mission.runs.truncate(runs);
        mission.active_runs = active;
        render_mission_runs(&mission)
    };
    assert_eq!(render(Some(1), 6), "1 active · 5 finished");
    assert_eq!(render(Some(2), 2), "2 active runs");
    assert_eq!(render(Some(0), 1), "1 finished run");
    assert_eq!(render(Some(0), 0), "0 runs");
    assert_eq!(render(None, 6), "6 runs");
    // A bounded recent-run window must not replace the daemon's complete run count.
    mission.total_runs = Some(100);
    mission.active_runs = Some(1);
    mission.runs.truncate(2);
    assert_eq!(render_mission_runs(mission), "1 active · 99 finished");
}

#[test]
fn terminal_list_shows_a_working_peek_target() {
    let mut page = fixture_product_page(&["runtime"], false);
    if let ClientResource::Runtime(runtime) = &mut page.items[0] {
        runtime.terminal_id = Some("terminal/agent/release".into());
    } else {
        panic!("expected runtime fixture");
    }
    let rendered = render_product_page("TERMINALS", &page, "st terminals");
    assert!(
        rendered.contains("peek: st terminals peek agent/release"),
        "{rendered}"
    );
}

#[test]
fn work_detail_labels_a_reason_blocked_only_for_blocked_work() {
    let page = fixture_product_page(&["work"], false);
    let ClientResource::Work(work) = &page.items[0] else {
        panic!("expected work fixture");
    };
    let mut work = work.clone();
    work.blocked_reason = Some("the step's lease expired".into());
    work.state = "claimed".into();
    let claimed = render_client_work_detail(&work);
    assert!(!claimed.contains("Blocked:"), "{claimed}");
    assert!(
        claimed.contains("\nReason: the step's lease expired\n"),
        "{claimed}"
    );
    work.state = "blocked".into();
    let blocked = render_client_work_detail(&work);
    assert!(
        blocked.contains("\nBlocked: the step's lease expired\n"),
        "{blocked}"
    );
}

#[test]
fn work_detail_says_when_its_holder_was_nudged() {
    let page = fixture_product_page(&["work"], false);
    let ClientResource::Work(work) = &page.items[0] else {
        panic!("expected work fixture");
    };
    let mut work = work.clone();
    assert!(!render_client_work_detail(&work).contains("Nudged:"));
    work.nudged_at_unix_ms = Some(1_791_581_505_000);
    let rendered = render_client_work_detail(&work);
    assert!(
        rendered.contains("\nNudged: 2026-10-09T21:31:45Z ("),
        "{rendered}"
    );
}

#[test]
fn attention_from_a_retired_requester_says_who_can_close_it() {
    let mut page = fixture_product_page(&["attention"], false);
    let before = render_product_page("NOW", &page, "st now");
    assert!(!before.contains("requester retired"), "{before}");
    let ClientResource::Attention(attention) = &mut page.items[0] else {
        panic!("expected attention fixture");
    };
    attention.header.operational = Some(st3_client::Operational {
        layer: "current".into(),
        actionable: true,
        reasons: vec!["requester-retired".into()],
        owner_generation: None,
        runtime_incarnation: None,
    });
    let rendered = render_product_page("NOW", &page, "st now");
    assert!(
        rendered.contains("  requester retired: only person/alex can close it\n"),
        "{rendered}"
    );
}

#[test]
fn document_continuation_preserves_prefix_and_history() {
    assert_eq!(
        document_continuation_command(Some("doc/type case"), true, 1, "document/abc"),
        "st documents ls 'doc/type case' --limit 1 --all --cursor document/abc"
    );
}

#[test]
fn product_renderers_have_exact_empty_and_mixed_now_output() {
    assert_eq!(
        render_product_page("NOW", &fixture_product_page(&[], false), "st now"),
        "NOW  0\nNo current items.\n"
    );
    let mixed = render_product_page(
        "NOW",
        &fixture_product_page(&["attention", "work", "operation"], false),
        "st now",
    );
    // An attention item leads with its kind and title, then who it is from and how long ago
    // (which moves with the clock) and its priority when it is not normal.
    let (head, rest) = mixed
        .split_once("  action: st alerts show")
        .expect(&mixed);
    assert!(
        head.starts_with("NOW  3\n\nrequest   Review release\n          "),
        "{mixed}"
    );
    assert!(head.ends_with(" ago · high priority\n"), "{mixed}");
    assert_eq!(
        rest,
        concat!(
            " launch/release --as person/alex\n",
            "  id: attention/release-review\n",
            "work/release/1/build  work  claimed  build  attempt 1\n",
            "  assigned: agent/release\n",
            "  action: st work show work/release/1/build\n",
            "operation/transport-host-b  operation  warning  degraded  Peer is retrying\n",
            "  recovery: st doctor\n",
        )
    );
}

#[test]
fn an_attention_item_names_its_kind_sender_and_age() {
    let mut item = match fixture_product_page(&["attention"], false)
        .items
        .into_iter()
        .next()
        .unwrap()
    {
        ClientResource::Attention(item) => item,
        other => panic!("{other:?}"),
    };
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-20T11:07:00Z")
        .unwrap()
        .timestamp_millis() as u128;
    assert_eq!(
        attention_heading(&item, now),
        "\nrequest   Review release\n          7m ago · high priority\n"
    );
    item.priority = "normal".into();
    item.requester_id = Some("agent/example/worker".into());
    item.update = Some(st3_client::PersonUpdate {
        version: 1,
        entry_type: "update".into(),
        about: "message/abc".into(),
        subjects: Vec::new(),
        summary: None,
    });
    assert_eq!(
        attention_heading(&item, now),
        "\nupdate    Review release\n          from example/worker · 7m ago\n"
    );
    // An update's action line warns that opening it reads it.
    let mut page = fixture_product_page(&["attention"], false);
    if let Some(ClientResource::Attention(shown)) = page.items.first_mut() {
        shown.update = item.update.clone();
    }
    let rendered = render_product_page("NOW", &page, "st now");
    assert!(
        rendered.contains("  action (marks it read): st alerts show launch/release"),
        "{rendered}"
    );
    item.state = "snoozed".into();
    item.update = None;
    item.review_mode = Some("approve".into());
    assert!(attention_heading(&item, now).starts_with("\nreview    "));
    assert!(attention_heading(&item, now).ends_with(" · 7m ago · snoozed\n"));
}

#[test]
fn now_page_without_work_does_not_claim_zero_working() {
    let attention_only = render_now_page(
        &fixture_product_page(&["attention"], false),
        "st now --as person/alex",
    );
    assert!(attention_only.starts_with("1 alert\n"), "{attention_only}");
    assert!(!attention_only.contains("WORKING"), "{attention_only}");
    assert!(!attention_only.contains("UNHEALTHY"), "{attention_only}");
    assert!(
        attention_only.ends_with("\nWork: st work ls · Health: st doctor\n"),
        "{attention_only}"
    );

    let with_work = render_now_page(
        &fixture_product_page(&["attention", "work"], false),
        "st now --as person/alex --owner-run mission-run/release/1",
    );
    assert!(with_work.contains("\nWORKING  1\n"), "{with_work}");
    assert!(!with_work.contains("UNHEALTHY"), "{with_work}");
    assert!(!with_work.contains("Work: st work ls"), "{with_work}");
}

#[test]
fn now_counts_alerts_alone_and_says_nothing_when_none_wait() {
    let page = fixture_product_page(&["attention"], false);
    let mut alerts = page.clone();
    alerts.items = [page.items.clone(), page.items.clone()].concat();
    assert!(
        render_now_page(&alerts, "st now").starts_with("2 alerts\n"),
        "{}",
        render_now_page(&alerts, "st now")
    );
    // An update asks nothing: it is listed apart and never counted as an alert.
    let mut updates = page.clone();
    let ClientResource::Attention(update) = &mut updates.items[0] else {
        panic!("expected attention fixture");
    };
    update.update = Some(st3_client::PersonUpdate {
        version: 1,
        entry_type: "update".into(),
        about: "message/abc".into(),
        subjects: Vec::new(),
        summary: None,
    });
    update.alert = Some(false);
    let now = render_now_page(&updates, "st now");
    assert!(now.starts_with("UPDATES  1\n"), "{now}");
    assert!(!now.contains("1 alert\n") && !now.contains("0 alerts"), "{now}");
    // Nothing waits: no line about alerts at all, not even a zero.
    assert_eq!(
        render_now_page(&fixture_product_page(&[], false), "st now"),
        "Work: st work ls · Health: st doctor\n"
    );
    // `st alerts ls` counts the alerts under its heading, then lists updates apart.
    let mut mixed = page.clone();
    mixed.items.extend(updates.items.clone());
    let listed = render_alert_sections(&mixed, Some("ALERTS FOR person/alex"), "st alerts ls");
    assert!(listed.starts_with("ALERTS FOR person/alex  1\n"), "{listed}");
    assert!(listed.contains("\nUPDATES  1\n"), "{listed}");
    // A daemon that predates alerts says nothing: an update is still not an alert.
    let ClientResource::Attention(update) = &mut mixed.items[1] else {
        panic!("expected attention fixture");
    };
    update.alert = None;
    assert_eq!(
        render_alert_sections(&mixed, Some("ALERTS FOR person/alex"), "st alerts ls"),
        listed
    );
}

#[test]
fn provider_capacity_backoff_is_bounded_deterministic_and_increases() {
    let first = provider_capacity_backoff_ms("agent/node.worker", "one", 1);
    let second = provider_capacity_backoff_ms("agent/node.worker", "one", 2);
    assert_eq!(
        first,
        provider_capacity_backoff_ms("agent/node.worker", "one", 1)
    );
    assert!(first >= PROVIDER_CAPACITY_BASE_BACKOFF_MS);
    assert!(second > first);
    assert!(
        provider_capacity_backoff_ms("agent/node.worker", "one", u32::MAX)
            <= PROVIDER_CAPACITY_MAX_BACKOFF_MS
                + PROVIDER_CAPACITY_MAX_BACKOFF_MS.saturating_div(4)
    );
}

#[test]
fn context_occupancy_alone_is_not_reported_as_zero_tokens() {
    let mut page = fixture_product_page(&["session"], false);
    let ClientResource::Session(session) = &mut page.items[0] else {
        panic!("expected session fixture");
    };
    session.usage = Some(
        serde_json::from_value(json!({
            "total_tokens": 0,
            "input_tokens": 0,
            "output_tokens": 0,
            "cached_tokens": 0,
            "incarnation_count": 0,
            "aggregation": "cumulative-per-incarnation-else-response-deltas",
            "context": { "used_tokens": 319465, "observed_at_unix_ms": 1 }
        }))
        .unwrap(),
    );
    let rendered = render_product_page("SESSIONS", &page, "st conversations sessions");
    assert!(!rendered.contains("0 tokens"), "{rendered}");
    assert!(
        rendered.contains("  usage not reported · context 319465 tokens\n"),
        "{rendered}"
    );

    let ClientResource::Session(session) = &mut page.items[0] else {
        unreachable!();
    };
    let usage = session.usage.as_mut().unwrap();
    usage.incarnation_count = 1;
    usage.total_tokens = 1200;
    let rendered = render_product_page("SESSIONS", &page, "st conversations sessions");
    assert!(rendered.contains("  usage 1200 tokens\n"), "{rendered}");
}

#[test]
fn an_empty_mailbox_prints_a_heading_and_no_current_items() {
    assert_eq!(
        render_mailbox("person/alex", None, false, &[]),
        "MESSAGES  0\nFILTERS  mailbox=person/alex\nNo current items.\n"
    );
    assert_eq!(
        render_mailbox(
            "agent/worker",
            Some("person/alex"),
            true,
            &["message/one\tread\tperson/alex\tHello".into()]
        ),
        concat!(
            "MESSAGES  1\n",
            "FILTERS  mailbox=agent/worker · from=person/alex · archived=included\n",
            "message/one\tread\tperson/alex\tHello\n",
        )
    );
}

#[test]
fn claimed_work_renews_for_the_exact_live_harness_incarnation_even_while_idle() {
    let step: StepRunView = serde_json::from_value(json!({
        "subject": "step-run/run/work",
        "run": "mission-run/run",
        "generation": "run-generation/run",
        "step": "work",
        "definition_hash": "definition",
        "status": "claimed",
        "attempt": 1,
        "assigned_to": "agent/node.worker",
        "agentless": false,
        "title": null,
        "worker_reported": false,
        "claimant": "agent/node.worker",
        "claim_incarnation": "worker-one",
        "claim_expires_at_unix_ms": 10,
        "execution_elapsed_ms": 0,
        "readiness_epoch": 1,
        "blocked_reason": null,
        "not_before_unix_ms": null,
        "created_at_unix_ms": 1,
        "updated_at_unix_ms": 1
    }))
    .unwrap();
    let harness: CurrentHarnessView = serde_json::from_value(json!({
        "state": "working",
        "incarnation_id": "worker-one",
        "claim": "claim/harness",
        "observed_at_unix_ms": 1
    }))
    .unwrap();
    assert!(work_claim_has_active_harness(
        &step,
        "agent/node.worker",
        Some(&harness)
    ));

    let mut idle = harness.clone();
    idle.state = "idle".into();
    assert!(work_claim_has_active_harness(
        &step,
        "agent/node.worker",
        Some(&idle)
    ));
    let mut blocked = idle;
    blocked.state = "blocked".into();
    assert!(work_claim_has_active_harness(
        &step,
        "agent/node.worker",
        Some(&blocked)
    ));
    let mut replacement = harness;
    replacement.incarnation_id = "worker-two".into();
    assert!(!work_claim_has_active_harness(
        &step,
        "agent/node.worker",
        Some(&replacement)
    ));
    assert!(!work_claim_has_active_harness(
        &step,
        "agent/other",
        Some(&replacement)
    ));
    assert!(!work_claim_has_active_harness(
        &step,
        "agent/node.worker",
        None
    ));
    let mut ended = replacement.clone();
    ended.incarnation_id = "worker-one".into();
    ended.state = "ended".into();
    assert!(!work_claim_has_active_harness(
        &step,
        "agent/node.worker",
        Some(&ended)
    ));
    for state in ["claimed", "working", "verifying", "blocked"] {
        let mut held = step.clone();
        held.status = state.into();
        assert!(held.work_claim_is_held_by("agent/node.worker"));
        assert!(!held.work_claim_is_held_by("agent/other"));
    }
    for state in ["ready", "pending", "completed", "cancelled", "failed"] {
        let mut unheld = step.clone();
        unheld.status = state.into();
        assert!(!unheld.work_claim_is_held_by("agent/node.worker"));
    }
}

fn retry_lease_step(status: &str, claimant: &str, expiry: Option<u128>) -> StepRunView {
    serde_json::from_value(json!({
        "subject": "step-run/retry/work", "run": "mission-run/retry",
        "generation": "run-generation/retry", "step": "work",
        "definition_hash": "definition", "status": status, "attempt": 1,
        "assigned_to": claimant, "agentless": false, "title": null,
        "worker_reported": false, "claimant": claimant,
        "claim_incarnation": "worker-one", "claim_expires_at_unix_ms": expiry,
        "execution_elapsed_ms": 0, "readiness_epoch": 1,
        "blocked_reason": null, "not_before_unix_ms": null,
        "created_at_unix_ms": 1, "updated_at_unix_ms": 1
    }))
    .unwrap()
}

fn repeated_timeout_pause(retry: &mut st3::client::DriverRetry) -> Duration {
    let error = anyhow::anyhow!("daemon response exceeded its deadline");
    let mut pause = Duration::ZERO;
    for _ in 0..6 {
        pause = retry.failed(&error);
    }
    pause
}

#[tokio::test(start_paused = true)]
async fn renewal_retry_empty_selection_retires_an_expired_lease_bound() {
    let mut retry = st3::client::DriverRetry::default();
    retry.lease_margin(Duration::from_secs(1));
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(repeated_timeout_pause(&mut retry) <= Duration::from_secs(1));
    // This is the production selector, called only after the work read succeeds.
    update_renewal_retry_lease(&mut retry, &[], "agent/worker", 1_000);
    retry.succeeded();
    assert!((Duration::from_secs(15)..=Duration::from_secs(30))
        .contains(&repeated_timeout_pause(&mut retry)));
}

#[tokio::test]
async fn renewal_retry_failed_work_read_preserves_the_known_lease_bound() {
    let root = tempfile::tempdir().unwrap();
    let client = Client::unix(root.path().join("missing.sock"));
    let mut retry = st3::client::DriverRetry::default();
    retry.lease_margin(Duration::from_secs(2));
    // Exercise the real request/selection boundary: an error is not an empty response.
    assert!(
        renew_claimed_work(&client, "agent/worker", 0, &mut retry)
            .await
            .is_err()
    );
    assert!(repeated_timeout_pause(&mut retry) <= Duration::from_secs(1));
}

#[tokio::test(start_paused = true)]
async fn renewal_retry_selects_live_leases_and_replaces_the_old_bound() {
    let mut retry = st3::client::DriverRetry::default();
    retry.lease_margin(Duration::ZERO);
    let work = [
        retry_lease_step("working", "agent/worker", Some(21_000)),
        retry_lease_step("verifying", "agent/worker", Some(41_000)),
        retry_lease_step("blocked", "agent/worker", Some(61_000)),
        retry_lease_step("claimed", "agent/other", Some(1_000)),
        retry_lease_step("completed", "agent/worker", Some(1_000)),
    ];
    update_renewal_retry_lease(&mut retry, &work, "agent/worker", 1_000);
    // The earliest relevant live lease wins; foreign/terminal rows cannot clamp it.
    assert_eq!(repeated_timeout_pause(&mut retry), Duration::from_secs(10));
    let replacement = [retry_lease_step("claimed", "agent/worker", Some(101_000))];
    update_renewal_retry_lease(&mut retry, &replacement, "agent/worker", 1_000);
    retry.succeeded();
    assert!((Duration::from_secs(15)..=Duration::from_secs(30))
        .contains(&repeated_timeout_pause(&mut retry)));
}

#[tokio::test(start_paused = true)]
async fn renewal_retry_unknown_expiry_and_success_do_not_retire_a_known_bound() {
    let mut retry = st3::client::DriverRetry::default();
    retry.lease_margin(Duration::from_secs(20));
    let work = [retry_lease_step("claimed", "agent/worker", None)];
    update_renewal_retry_lease(&mut retry, &work, "agent/worker", 1_000);
    retry.succeeded();
    assert_eq!(repeated_timeout_pause(&mut retry), Duration::from_secs(10));
}

#[test]
fn legacy_attention_help_directs_callers_to_person_work() {
    let mut command = Cli::command();
    let help = command
        .find_subcommand_mut("attention")
        .unwrap()
        .find_subcommand_mut("request")
        .unwrap()
        .render_long_help()
        .to_string();
    assert!(help.contains("attention-migrated"), "{help}");
    assert!(help.contains("work ask"), "{help}");
}

#[test]
fn machines_help_and_contract_include_configured_failures_by_default() {
    let command = Cli::command();
    let machines = command.find_subcommand("machines").unwrap();
    let all = machines
        .get_arguments()
        .find(|argument| argument.get_id() == "all")
        .unwrap();
    assert_eq!(
        all.get_help().unwrap().to_string(),
        "Include historical and discovered hosts beyond the current configured fleet"
    );

    let contract: Value = serde_json::from_str(include_str!(
        "../../../docs/st3/operational-state/cli-commands.json"
    ))
    .unwrap();
    assert_eq!(
        contract["purposes"]["machines"]["defaults"],
        "current configured fleet including failures"
    );
    assert_eq!(
        contract["purposes"]["attention"]["human_example"],
        "st alerts ls --as person/alex"
    );
    assert_eq!(
        contract["purposes"]["attention"]["json_example"],
        "st alerts ls --as person/alex --json"
    );
}

#[test]
fn machine_and_device_renderers_have_exact_operational_output() {
    assert_eq!(
        render_product_page(
            "MACHINES",
            &fixture_product_page(&["machine"], true),
            "st machines"
        ),
        concat!(
            "MACHINES  1\n",
            "machine/host-a  local\n",
            "  capacity not reported\n",
            "  runtimes 1 running · 1 known\n",
            "  assigned work 1\n",
            "  transport unix local · last success 2026-09-20T11:09:10Z\n",
            "  inspect: st subject show host/host-a\n",
            "More items are available: st machines --cursor cursor/next --limit 100\n",
        )
    );
    assert_eq!(
        render_product_page(
            "DEVICES",
            &fixture_product_page(&["device"], false),
            "st devices --as person/alex"
        ),
        concat!(
            "DEVICES  1\n",
            "device/ios-release  active  person/alex/session/ios-release  scopes 4\n",
            "  action: st devices --as person/alex revoke device/ios-release\n",
        )
    );
}

#[test]
fn activity_renderer_uses_exact_contract_labels_and_cursors() {
    let events: ClientEnvelope<ClientEventPage> = serde_json::from_str(include_str!(
        "../../../docs/st3/client-v0/fixtures/events.json"
    ))
    .unwrap();
    assert_eq!(
        render_activity_page(&events.value),
        concat!(
            "ACTIVITY  2\n",
            "work/release/1/build  upsert  2026-09-20T12:00:01Z  cursor event-cursor/epoch-a/92\n",
            "session/release-agent/9  timeline.delta  2026-09-20T12:00:02Z  cursor event-cursor/epoch-a/93\n",
        )
    );
    assert_eq!(
        render_activity_page(&ClientEventPage {
            kind: "event-page".into(),
            oldest_cursor: "event-cursor/epoch-a/40".into(),
            resume_cursor: "event-cursor/epoch-a/93".into(),
            items: Vec::new(),
            has_more: true,
        }),
        concat!(
            "ACTIVITY  0\n",
            "No material changes. Resume after event-cursor/epoch-a/93.\n",
            "More changes are available after event-cursor/epoch-a/93.\n",
        )
    );
}

#[test]
fn pty_attach_accepts_a_graph_subject() {
    let cli = Cli::try_parse_from([
        "st3",
        "terminals",
        "attach",
        "agent/example/app-web/standing/app-web",
    ])
    .unwrap();
    let Command::Terminals {
        command: PtyCommand::Attach(args),
    } = cli.command
    else {
        panic!("the PTY attach command did not parse");
    };
    assert_eq!(args.subject, "agent/example/app-web/standing/app-web");
    assert!(!args.force);
}

#[test]
fn terminal_history_is_explicit() {
    let cli = Cli::try_parse_from(["st3", "terminals", "ls", "--all"]).unwrap();
    let Command::Terminals {
        command: PtyCommand::Ls { all, .. },
    } = cli.command
    else {
        panic!("the terminal list command did not parse");
    };
    assert!(all);
}

#[test]
fn terminal_screen_accepts_a_remote_owner_subject_and_person() {
    let cli = Cli::try_parse_from([
        "st3",
        "--json",
        "terminals",
        "screen",
        "terminal/agent/example/app-web/standing/app-web",
        "--as",
        "person/alex",
    ])
    .unwrap();
    assert!(cli.json);
    let Command::Terminals {
        command: PtyCommand::Screen(args),
    } = cli.command
    else {
        panic!("the terminal screen command did not parse");
    };
    assert_eq!(
        args.subject,
        "terminal/agent/example/app-web/standing/app-web"
    );
    assert_eq!(args.person.as_deref(), Some("person/alex"));
}

#[test]
fn terminal_screen_renderer_preserves_each_terminal_row() {
    let response: ClientEnvelope<ClientTerminalScreen> = serde_json::from_str(include_str!(
        "../../../docs/st3/client-v0/fixtures/terminal-screen.json"
    ))
    .unwrap();
    assert_eq!(
        render_terminal_screen(&response.value),
        "$ cargo build\nFinished\n$\n"
    );
}

#[test]
fn client_terminal_lifecycle_commands_parse_without_changing_local_attach() {
    let attach = Cli::try_parse_from([
        "st3",
        "terminals",
        "attach-info",
        "terminal/agent/example/app-web/standing/app-web",
    ])
    .unwrap();
    assert!(matches!(
        attach.command,
        Command::Terminals {
            command: PtyCommand::AttachInfo(_)
        }
    ));

    let stream = Cli::try_parse_from([
        "st3",
        "terminals",
        "stream",
        "terminal/agent/example/app-web/standing/app-web",
        "--capability",
        "test-capability",
        "--incarnation",
        "runtime-1",
        "--count",
        "3",
    ])
    .unwrap();
    let Command::Terminals {
        command: PtyCommand::Stream(args),
    } = stream.command
    else {
        panic!("stream did not parse");
    };
    assert_eq!(args.incarnation.as_deref(), Some("runtime-1"));
    assert_eq!(args.count, Some(3));

    let input = Cli::try_parse_from([
        "st3",
        "terminals",
        "input-client",
        "terminal/agent/example/app-web/standing/app-web",
        "hello",
        "--key",
    ])
    .unwrap();
    assert!(matches!(
        input.command,
        Command::Terminals {
            command: PtyCommand::InputClient(PtyClientInputArgs { key: true, .. })
        }
    ));

    let detach = Cli::try_parse_from([
        "st3",
        "terminals",
        "detach-client",
        "terminal-attachment/example",
        "--incarnation",
        "runtime/example:1",
    ])
    .unwrap();
    assert!(matches!(
        detach.command,
        Command::Terminals {
            command: PtyCommand::DetachClient(_)
        }
    ));
}

#[test]
fn terminal_stream_accepts_capabilities_beginning_with_hyphens() {
    for capability in ["-test-capability", "--test-capability"] {
        let stream = Cli::try_parse_from([
            "st3",
            "terminals",
            "stream",
            "terminal/agent/example/worker",
            "--capability",
            capability,
            "--incarnation",
            "runtime-1",
            "--count",
            "3",
        ])
        .unwrap();
        let Command::Terminals {
            command: PtyCommand::Stream(args),
        } = stream.command
        else {
            panic!("stream did not parse");
        };
        assert_eq!(args.capability, capability);
        assert_eq!(args.incarnation.as_deref(), Some("runtime-1"));
        assert_eq!(args.count, Some(3));
    }
}

#[test]
fn admission_failure_projection_names_boundary_and_driver_policy() {
    use st_drivers::driver_diagnostic::{
        Driver, Observed, Publisher, Reason, Source, Stage, Support,
    };
    for (driver, severity, status) in [
        (Driver::Omp, "error", "failed"),
        (Driver::OpenCode, "warning", "degraded"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut publisher = Publisher::new(
            directory.path(),
            driver,
            Some("99.42.7".into()),
            Support::Unsupported,
        );
        publisher.publish(
            Stage::VersionGate,
            Reason::AdmissionNativeConsumption,
            Source::AdmissionProbe,
        );
        let observed = st_drivers::driver_diagnostic::read(
            &st_drivers::driver_diagnostic::path(directory.path()),
        );
        let claim = admission_diagnostic("agent/node.fixture", "first", &observed).unwrap();
        assert_eq!(claim.fields["code"], "harness-admission-failed");
        assert_eq!(claim.fields["severity"], severity);
        assert_eq!(claim.fields["status"], status);
        assert_eq!(claim.fields["incarnation_id"], "first");
        let reason = claim.fields["reason"].as_str().unwrap();
        assert!(reason.contains("admissionNativeConsumption") && reason.contains("99.42.7"));
        assert!(reason.contains(driver.as_str()));
        assert!(reason.contains(&format!("st admission override {} --binary", driver.as_str())));
        assert_eq!(
            claim.idempotency_key,
            admission_diagnostic("agent/node.fixture", "first", &observed)
                .unwrap()
                .idempotency_key
        );
    }
    assert!(admission_diagnostic("agent/node.fixture", "first", &Observed::Absent).is_none());
}

#[test]
fn harness_diagnostic_requires_and_derives_the_agent_identity() {
    let cli = Cli::try_parse_from([
        "st3",
        "diagnostic",
        "--as",
        "agent/run/worker",
        "--code",
        "driver-failed",
        "--reason",
        "the native driver exited",
        "--incarnation",
        "runtime:one",
    ])
    .unwrap();
    let Command::Diagnostic(args) = cli.command else {
        panic!("the harness diagnostic command did not parse");
    };
    assert_eq!(args.actor, "agent/run/worker");
    assert_eq!(args.severity, "error");
    assert_eq!(args.incarnation.as_deref(), Some("runtime:one"));
}

#[test]
fn every_extension_harness_has_a_hidden_native_channel_driver() {
    for driver in ["pi-channel", "omp-channel"] {
        let cli =
            Cli::try_parse_from(["st3", "driver", driver, "--subject", "agent/run/worker"])
                .unwrap_or_else(|error| panic!("{driver} did not parse: {error}"));
        let Command::Driver(args) = cli.command else {
            panic!("{driver} did not select the hidden driver command");
        };
        assert_eq!(args.driver, driver);
        assert_eq!(args.subject.as_deref(), Some("agent/run/worker"));
    }
}

#[test]
fn pty_attach_accepts_an_explicit_nested_override() {
    let cli = Cli::try_parse_from([
        "st3",
        "terminals",
        "attach",
        "agent/example/app-web/standing/app-web",
        "--force",
    ])
    .unwrap();
    let Command::Terminals {
        command: PtyCommand::Attach(args),
    } = cli.command
    else {
        panic!("the PTY attach command did not parse");
    };
    assert!(args.force);
}

#[test]
fn an_agent_wait_stops_for_messages_work_and_an_empty_lease() {
    let actor = "agent/worker";
    assert_eq!(
        wait_interruption_reason(actor, true, &[], &["message/new".into()]),
        Some(
            "the wait stopped because agent/worker has a new message: message/new. Run `st conversations ls`"
                .into()
        )
    );
    assert_eq!(
        wait_interruption_reason(actor, true, &["step-run/new".into()], &[]),
        Some(
            "the wait stopped because agent/worker has ready work: step-run/new. Run `st work ls`"
                .into()
        )
    );
    assert_eq!(
        wait_interruption_reason(
            actor,
            true,
            &["step-run/new".into()],
            &["message/new".into()]
        ),
        Some(
            "the wait stopped because agent/worker has ready work: step-run/new. Run `st work ls`"
                .into()
        )
    );
    assert!(
        wait_interruption_reason(actor, false, &[], &[])
            .unwrap()
            .contains("cannot wait without claimed work")
    );
    assert_eq!(wait_interruption_reason(actor, true, &[], &[]), None);
}

#[tokio::test]
async fn a_send_or_reply_to_a_person_fails_in_the_cli_before_it_reaches_the_daemon() {
    // The refusal needs no daemon: this endpoint has nothing behind it.
    let client = Client::new(Endpoint::Unix(PathBuf::from("/nonexistent/st3.sock")));
    let args = |to: &str| MessageSendArgs {
        to: to.into(),
        body: "Done.".into(),
        subject: None,
        in_reply_to: Some("message/0123456789abcdef".into()),
        tags: Vec::new(),
        from: "agent/example/worker".into(),
        attach: Vec::new(),
        wake: MessageWakeArgs::default(),
        print_kdl: false,
        idempotency_key: Some("refuse-a-person".into()),
    };
    for to in ["person/ada", "requester"] {
        let error = send_message(&client, args(to), Vec::new()).await.unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.starts_with("people do not have inboxes: print your answer in the chat"),
            "{to}: {text}"
        );
        assert!(text.contains("only if the person asked for it"), "{text}");
    }
    // Even a preview of the message is refused.
    let mut preview = args("person/ada");
    preview.print_kdl = true;
    assert!(send_message(&client, preview, Vec::new()).await.is_err());
    // An agent recipient passes the refusal; it fails later, on the sender or the daemon.
    let error = send_message(&client, args("agent/example/other"), Vec::new())
        .await
        .unwrap_err();
    assert!(!format!("{error:#}").contains("people do not have inboxes"));
}

#[test]
fn a_short_message_party_resolves_to_its_current_mission_run() {
    assert_eq!(
        normalize_message_subject_in_run("worker", Some("run-id")),
        "agent/run-id/worker"
    );
    assert_eq!(
        normalize_message_subject_in_run("agent/global.worker", Some("run-id")),
        "agent/global.worker"
    );
    assert_eq!(
        normalize_message_subject_in_run("worker", None),
        "agent/worker"
    );
}

#[test]
fn every_offered_cli_command_has_an_action_coverage_row() {
    fn walk(
        command: &clap::Command,
        prefix: &str,
        output: &mut std::collections::BTreeSet<String>,
    ) {
        let children = command
            .get_subcommands()
            .filter(|child| !child.is_hide_set() && child.get_name() != "help")
            .collect::<Vec<_>>();
        if prefix != "st" && (children.is_empty() || !command.is_subcommand_required_set()) {
            output.insert(prefix.to_owned());
        }
        for child in children {
            walk(child, &format!("{prefix} {}", child.get_name()), output);
        }
    }
    let mut command = Cli::command();
    command.build();
    let mut offered = std::collections::BTreeSet::new();
    walk(&command, "st", &mut offered);
    let inventory: serde_json::Value =
        serde_json::from_str(include_str!("../../../docs/st3/action-coverage.json")).unwrap();
    let documented = inventory["cli"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["command"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        offered, documented,
        "Update the inventory and its end-to-end coverage when the CLI changes"
    );
}

#[test]
fn machine_read_inventory_comes_from_the_live_clap_graph() {
    let inventory = read_contract_inventory_json();
    assert_eq!(inventory["modes"], json!(["human", "json"]));
    assert!(inventory["aliases"].is_object());
    let with_alias = read_contract_inventory_for_command(
        clap::Command::new("st").subcommand(clap::Command::new("invented").alias("i")),
    );
    let changed_alias = read_contract_inventory_for_command(
        clap::Command::new("st").subcommand(clap::Command::new("invented").alias("j")),
    );
    assert_eq!(with_alias["aliases"]["st invented"], json!(["i"]));
    assert_eq!(changed_alias["aliases"]["st invented"], json!(["j"]));
    assert_ne!(with_alias["aliases"], changed_alias["aliases"]);
    assert!(inventory.get("source_head").is_none());
    assert!(inventory.get("source_tree").is_none());
    let reported = inventory["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|command| command.as_str().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
    let visible: Value = serde_json::from_str(include_str!(
        "../../../docs/st3/action-coverage.json"
    ))
    .unwrap();
    for row in visible["cli"].as_array().unwrap() {
        assert!(reported.contains(row["command"].as_str().unwrap()));
    }
    assert!(reported.contains("st claude-channel install-policy"));
    assert!(!reported.contains("st help"));
}

#[test]
fn machine_read_inventory_probe_is_exclusive_before_actions() {
    let args = |words: &[&str]| words.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
    assert_eq!(
        read_contract_inventory_request(&args(&["st", "--read-contract-inventory-json"])),
        Some(true)
    );
    for mixed in [
        vec!["st", "--read-contract-inventory-json", "documents", "put"],
        vec!["st", "documents", "put", "--read-contract-inventory-json"],
        vec!["st", "--read-contract-inventory-json=true"],
    ] {
        assert_eq!(read_contract_inventory_request(&args(&mixed)), Some(false));
    }
    for literal in [
        "--read-contract-inventory-json",
        "--read-contract-inventory-json=true",
    ] {
        assert_eq!(
            read_contract_inventory_request(&args(&["st", "documents", "put", "--", literal])),
            None
        );
        assert_eq!(
            read_contract_inventory_request(&args(&["st", "driver", "run", "--", "codex", literal])),
            None
        );
        assert!(Cli::try_parse_from([
            "st", "documents", "put", "--as", "doc/example", "--", literal,
        ])
        .is_ok());
    }
    assert_eq!(read_contract_inventory_request(&args(&["st", "documents", "put"])), None);
}

#[test]
fn mission_start_help_shows_the_run_subject_an_id_names() {
    use clap::CommandFactory as _;
    let mut command = Cli::command();
    let help = command
        .find_subcommand_mut("missions")
        .and_then(|missions| missions.find_subcommand_mut("start"))
        .expect("missions start")
        .render_help()
        .to_string();
    assert!(help.contains("`mission-run/release/demo/1`"), "{help}");
    assert!(help.contains("A bare ID is placed under the mission"), "{help}");
    assert!(!help.contains("`mission-run/1`"), "{help}");
}

#[test]
fn mission_start_bare_ids_are_scoped_to_each_mission() {
    for mission in ["fleet/smalltalk/daemon-github-sekrets", "release/demo"] {
        for requested in ["2026-10-07", "mission-run/2026-10-07"] {
            let run_id = mission_start_run_id(mission, Some(requested));
            assert_eq!(run_id, format!("{mission}/2026-10-07"));
            let kdl = mission_run_intent(
                &run_id,
                mission,
                &"a".repeat(64),
                Path::new("/work/demo"),
                "agent/operator",
                &BTreeMap::new(),
                "run",
                None,
                None,
                false,
            );
            let intent = st3::graph::parse_intent(&kdl, "node").unwrap();
            assert!(
                intent
                    .mission_runs
                    .contains_key(&format!("mission-run/{mission}/2026-10-07"))
            );
            assert!(!intent.mission_runs.contains_key("mission-run/2026-10-07"));
        }
    }
}

#[test]
fn mission_start_explicit_paths_and_default_ids_are_preserved() {
    for requested in ["release/demo/test", "mission-run/release/demo/test"] {
        assert_eq!(
            mission_start_run_id("other/mission", Some(requested)),
            "release/demo/test"
        );
    }
    let generated = mission_start_run_id("release/demo", None);
    let suffix = generated.strip_prefix("release/demo/").unwrap();
    assert!(uuid::Uuid::parse_str(suffix).is_ok());
}

#[test]
fn mission_start_accepts_an_explicit_run_id() {
    let cli = Cli::try_parse_from([
        "st3",
        "missions",
        "start",
        "release/demo",
        "--id",
        "release/demo/test",
        "--as",
        "agent/operator",
    ])
    .unwrap();
    let Command::Missions {
        command: MissionViewCommand::Start(args),
    } = cli.command
    else {
        panic!("the mission start command did not parse");
    };
    assert_eq!(args.mission, "release/demo");
    assert_eq!(args.id.as_deref(), Some("release/demo/test"));
    assert_eq!(args.actor, "agent/operator");
}

#[test]
fn mission_start_after_names_the_run_to_wait_for() {
    let cli = Cli::try_parse_from([
        "st3",
        "missions",
        "start",
        "release/demo",
        "--after",
        "release/build/1",
        "--as",
        "person/operator",
    ])
    .unwrap();
    let Command::Missions {
        command: MissionViewCommand::Start(args),
    } = cli.command
    else {
        panic!("the mission start command did not parse");
    };
    assert_eq!(args.after.as_deref(), Some("release/build/1"));

    let kdl = mission_run_intent(
        "release/demo/2",
        "release/demo",
        &"a".repeat(64),
        Path::new("/work/demo"),
        "person/operator",
        &BTreeMap::new(),
        "run",
        Some("mission-run/release/build/1"),
        None,
        false,
    );
    assert!(
        kdl.contains("after \"mission-run/release/build/1\""),
        "{kdl}"
    );
    let intent = st3::graph::parse_intent(&kdl, "node").unwrap();
    let creation = intent.mission_runs["mission-run/release/demo/2"]
        .creation
        .as_ref()
        .unwrap();
    assert_eq!(
        creation.after.as_deref(),
        Some("mission-run/release/build/1")
    );
}

#[test]
fn mission_start_report_to_names_an_agent_and_completion_needs_one() {
    let cli = Cli::try_parse_from([
        "st3",
        "missions",
        "start",
        "release/demo",
        "--report-to",
        "agent/ops/watcher",
        "--report-completed",
        "--as",
        "person/operator",
    ])
    .unwrap();
    let Command::Missions {
        command: MissionViewCommand::Start(args),
    } = cli.command
    else {
        panic!("the mission start command did not parse");
    };
    assert_eq!(args.report_to.as_deref(), Some("agent/ops/watcher"));
    assert!(args.report_completed);
    let kdl = mission_run_intent(
        "release/demo/3",
        "release/demo",
        &"a".repeat(64),
        Path::new("/work/demo"),
        "person/operator",
        &BTreeMap::new(),
        "run",
        None,
        args.report_to.as_deref(),
        args.report_completed,
    );
    let intent = st3::graph::parse_intent(&kdl, "node").unwrap();
    let creation = intent.mission_runs["mission-run/release/demo/3"]
        .creation
        .as_ref()
        .unwrap();
    assert_eq!(creation.report_to.as_deref(), Some("agent/ops/watcher"));
    assert!(creation.report_completed);

    assert!(
        Cli::try_parse_from([
            "st3",
            "missions",
            "start",
            "release/demo",
            "--report-completed",
            "--as",
            "person/operator",
        ])
        .is_err(),
        "--report-completed means nothing without --report-to"
    );
    let person = mission_run_intent(
        "release/demo/4",
        "release/demo",
        &"a".repeat(64),
        Path::new("/work/demo"),
        "person/operator",
        &BTreeMap::new(),
        "run",
        None,
        Some("person/ada"),
        false,
    );
    assert_eq!(
        st3::graph::parse_intent(&person, "node").unwrap_err().code,
        "invalid-report-to"
    );
}

#[test]
fn missions_report_to_names_an_agent_or_clears_and_options_need_an_agent() {
    let parse = |extra: &[&str]| {
        let mut words = vec!["st3", "missions", "report-to", "mission-run/release/demo/3"];
        words.extend_from_slice(extra);
        words.extend_from_slice(&["--as", "agent/ops/owner"]);
        Cli::try_parse_from(words)
    };
    let Command::Missions {
        command: MissionViewCommand::ReportTo(args),
    } = parse(&[
        "--agent",
        "agent/ops/watcher",
        "--stalled-after",
        "1h",
        "--report-completed",
        "--report-stalls",
        "false",
    ])
    .unwrap()
    .command
    else {
        panic!("the report-to command did not parse");
    };
    assert_eq!(args.agent.as_deref(), Some("agent/ops/watcher"));
    assert_eq!(args.stalled_after, Some(3_600_000));
    assert!(args.report_completed);
    assert_eq!(args.report_stalls, Some(false));
    assert!(!args.clear);
    assert_eq!(whole_duration(3_600_000), "1h");
    assert_eq!(whole_duration(45 * 60_000), "45m");
    assert!(parse(&["--clear"]).is_ok());
    for (extra, why) in [
        (&[][..], "it names an agent or clears"),
        (
            &["--clear", "--report-stalls", "false"][..],
            "stall policy needs an agent",
        ),
        (
            &["--agent", "agent/ops/watcher", "--report-stalls", "no"][..],
            "stall policy is boolean",
        ),
        (&["--agent", "agent/ops/watcher", "--clear"][..], "not both"),
        (
            &["--clear", "--stalled-after", "1h"][..],
            "a limit needs an agent",
        ),
        (
            &["--clear", "--report-completed"][..],
            "completion needs an agent",
        ),
        (
            &["--agent", "agent/ops/watcher", "--stalled-after", "0m"][..],
            "a limit is positive",
        ),
    ] {
        assert!(parse(extra).is_err(), "{why}");
    }
}

#[test]
fn apply_accepts_plain_files_and_requires_owned_source_flags_together() {
    let plain = [
        "st",
        "apply",
        "seats.kdl",
        "missions.kdl",
        "schedules.kdl",
        "--as",
        "person/operator",
        "--dry-run",
        "--check",
        "--workspace",
        "/tmp",
        "--input",
        "release=1.4.0",
    ];
    let cli = Cli::try_parse_from(plain).unwrap();
    let Command::Apply(args) = cli.command else {
        panic!("expected apply")
    };
    assert!(args.set.is_none());
    assert_eq!(args.files.len(), 3);
    assert!(args.dry_run && args.check);
    assert_eq!(args.inputs, vec![("release".into(), "1.4.0".into())]);
    let owned = [
        "st",
        "apply",
        "seats.kdl",
        "--as",
        "person/operator",
        "--set",
        "garden",
        "--repository",
        "acme/garden",
        "--ref",
        "refs/heads/main",
        "--sha",
        "0123456789abcdef0123456789abcdef01234567",
        "--source-sequence",
        "1",
        "--expect-set",
        "absent",
    ];
    assert!(Cli::try_parse_from(owned).is_ok());
    for flag in [
        "--set",
        "--repository",
        "--ref",
        "--sha",
        "--source-sequence",
        "--expect-set",
    ] {
        let mut incomplete = owned.to_vec();
        let index = incomplete.iter().position(|arg| *arg == flag).unwrap();
        incomplete.drain(index..index + 2);
        assert!(
            Cli::try_parse_from(incomplete).is_err(),
            "{flag} is required for owned publication"
        );
    }
    let mut conflicting = plain.to_vec();
    conflicting.push("--no-gate-check");
    assert!(Cli::try_parse_from(conflicting).is_err());
    let mut owned_at_index = owned.to_vec();
    owned_at_index.extend(["--at-index", "0"]);
    assert!(Cli::try_parse_from(owned_at_index).is_err());
    for option in ["--allow-empty", "--adopt", "--confirm-retire", "--rollout"] {
        let mut unsupported = vec![
            "st",
            "apply",
            "seats.kdl",
            "--as",
            "person/operator",
            option,
        ];
        match option {
            "--allow-empty" => {}
            "--rollout" => unsupported.push("when-idle"),
            _ => unsupported.push("agent/example/helper"),
        }
        assert!(
            Cli::try_parse_from(unsupported).is_err(),
            "{option} needs --set"
        );
    }
}

#[test]
fn apply_rejects_duplicate_stdin_and_unversioned_files() {
    assert!(
        read_apply_files(&["-".into(), "-".into()], false)
            .unwrap_err()
            .to_string()
            .contains("stdin may appear only once")
    );
    assert!(read_apply_files(&[], false).is_err());
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("invalid.kdl");
    fs::write(&file, "agent \"example/helper\" { command \"true\"; }\n").unwrap();
    assert!(
        read_apply_files(&[file], false)
            .unwrap_err()
            .to_string()
            .contains("must begin with version 2")
    );
}

// LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-10-st-apply-aliases — DELETE at contraction — https://www.notion.so/3f5e3d41f4a3818aadb6f4b74030c1da
#[test]
fn mission_publish_requires_an_explicit_person_or_agent_actor() {
    let cli = Cli::try_parse_from([
        "st3",
        "missions",
        "publish",
        "missions/typecase.kdl",
        "--as",
        "agent/example/cos/standing/cos",
    ])
    .unwrap();
    let Command::Missions {
        command: MissionViewCommand::Publish(args),
    } = cli.command
    else {
        panic!("the mission publish command did not parse");
    };
    assert_eq!(args.file, PathBuf::from("missions/typecase.kdl"));
    assert_eq!(args.actor, "agent/example/cos/standing/cos");
    assert!(
        Cli::try_parse_from([
            "st3",
            "missions",
            "publish",
            "missions/typecase.kdl",
            "--as",
            "cos",
        ])
        .is_err()
    );
}
// LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-10-st-apply-aliases

#[test]
fn mission_cancel_requires_an_exact_actor_and_reason() {
    for actor in ["agent/example/operator", "person/alex"] {
        assert!(
            Cli::try_parse_from([
                "st3",
                "missions",
                "cancel",
                "mission-run/example/one",
                "--reason",
                "superseded",
                "--as",
                actor
            ])
            .is_ok()
        );
    }
    assert!(
        Cli::try_parse_from([
            "st3",
            "missions",
            "cancel",
            "mission-run/example/one",
            "--reason",
            "superseded",
            "--as",
            "operator"
        ])
        .is_err()
    );
    let cli = Cli::try_parse_from([
        "st3",
        "missions",
        "cancel",
        "mission-run/release/demo",
        "--reason",
        "the run was superseded",
        "--as",
        "person/alex",
    ])
    .unwrap();
    let Command::Missions {
        command: MissionViewCommand::Cancel(args),
    } = cli.command
    else {
        panic!("the mission cancel command did not parse");
    };
    assert_eq!(args.mission_run, "mission-run/release/demo");
    assert_eq!(args.reason, "the run was superseded");
    assert_eq!(args.actor, "person/alex");
}

#[test]
fn mission_show_accepts_follow() {
    let cli = Cli::try_parse_from([
        "st3",
        "missions",
        "show",
        "mission-run/release/demo",
        "--follow",
    ])
    .unwrap();
    let Command::Missions {
        command: MissionViewCommand::Show(args),
    } = cli.command
    else {
        panic!("the mission show command did not parse");
    };
    assert_eq!(args.mission_or_run, "mission-run/release/demo");
    assert!(args.follow);
    assert_eq!(args.limit, 50);
    assert!(args.cursor.is_none());
    let continued = Cli::try_parse_from([
        "st3", "missions", "show", "mission-run/release/demo", "--cursor", "child-run",
        "--limit", "20",
    ])
    .unwrap();
    let Command::Missions {
        command: MissionViewCommand::Show(continued),
    } = continued.command else {
        panic!("mission show pagination did not parse");
    };
    assert_eq!(continued.cursor.as_deref(), Some("child-run"));
    assert_eq!(continued.limit, 20);
    assert!(Cli::try_parse_from([
        "st3", "missions", "show", "mission-run/release/demo", "--follow", "--cursor", "child-run",
    ])
    .is_err());
}

#[test]
fn cli_exposes_launch_without_removed_planning_aliases() {
    assert!(Cli::try_parse_from(["st3", "plan", "show", "example"]).is_err());
    assert!(Cli::try_parse_from(["st3", "planning", "show", "example"]).is_err());
    let cli = Cli::try_parse_from(["st3", "launch", "show", "example"]).unwrap();
    assert!(matches!(
        cli.command,
        Command::Launch {
            command: LaunchCommand::Show(_)
        }
    ));
}

#[test]
fn wait_timeout_accepts_bounded_units_and_zero() {
    assert_eq!(parse_timeout("250ms").unwrap(), Duration::from_millis(250));
    assert_eq!(parse_timeout("2m").unwrap(), Duration::from_secs(120));
    assert_eq!(parse_timeout("0").unwrap(), Duration::ZERO);
    assert!(parse_timeout("forever").is_err());
}

#[test]
fn wait_reads_resource_status_from_observed_facts() {
    let resource = json!({
        "baseline": true,
        "facts": {"status": "ready"},
        "kind": "filesystem.file"
    });
    assert_eq!(
        st3::model::projected_actual_status(Some(&resource)),
        Some("ready")
    );

    let runtime = json!({"fields": {"status": "running"}});
    assert_eq!(
        st3::model::projected_actual_status(Some(&runtime)),
        Some("running")
    );
}

#[test]
fn wait_accepts_message_delivery() {
    validate_wait_condition("delivered").unwrap();
    let message = serde_json::json!({ "status": "delivered" });
    assert_eq!(
        st3::model::projected_actual_status(Some(&message)),
        Some("delivered")
    );
}

#[test]
fn wait_accepts_a_standing_mission_run() {
    validate_wait_condition("standing").unwrap();
    let run = serde_json::json!({ "status": "standing" });
    assert_eq!(
        st3::model::projected_actual_status(Some(&run)),
        Some("standing")
    );
}

#[test]
fn native_driver_binds_the_local_pty_incarnation_instead_of_a_stale_graph_value() {
    let observations = vec![st_runtime::PtyObservation {
        name: "run.worker".into(),
        status: "running".into(),
        exit_code: None,
        pid: Some(42),
        created_at: Some("2026-09-22T12:00:00.000Z".into()),
        display_name: None,
        tags: BTreeMap::from([("st3.subject".into(), "agent/run/worker".into())]),
    }];

    assert_eq!(
        pty_observation_incarnation("agent/run/worker", &observations).as_deref(),
        Some("42:2026-09-22T12:00:00.000Z")
    );
    assert_eq!(
        pty_observation_incarnation("agent/run/other", &observations),
        None
    );
}

/// A deploy re-executes every driver while the daemon restarts, and each one then waits for its
/// incarnation. Through the outage a driver retries after its spread pause, not the 50 ms poll
/// that would bring every driver to the new daemon in the instant it starts to listen.
#[tokio::test]
async fn an_incarnation_wait_retries_an_outage_after_a_spread_pause() {
    use axum::{Json, Router, routing::get};
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("st3.sock");
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = requests.clone();
    let app = Router::new().route(
        "/v1/status",
        get(move || {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async {
                Json(json!({"api_version": "st3.v1", "value": {
                    "store_index": 1,
                    "pending_actions": [],
                    "subjects": [{
                        "subject": "agent/example/cedar",
                        "actual": {"fields": {"incarnation_id": "cedar-1"}},
                        "conflicts": [],
                        "claims": [],
                        "reachability": "reachable",
                    }],
                }}))
            }
        }),
    );
    let client = Client::new(Endpoint::Unix(socket.clone()));
    let started = std::time::Instant::now();
    let wait = tokio::spawn(async move {
        wait_for_agent_incarnation_from(&client, "agent/example/cedar", false).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let server = tokio::spawn(async move { serve_unix(&socket, app).await.unwrap() });

    assert_eq!(wait.await.unwrap().unwrap(), "cedar-1");
    assert!(started.elapsed() >= Duration::from_millis(500));
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    server.abort();
}

#[test]
fn native_driver_binds_the_terminal_tag_only_under_its_declared_invocation() {
    let binding = st3::model::TerminalBinding {
        subject: "pty/person/avery/019a0000-0000-7000-8000-000000000001".into(),
        incarnation: "42:created".into(),
        id: "019a0000-0000-7000-8000-000000000002".into(),
    };
    let mut observation = st_runtime::PtyObservation {
        name: "fixture".into(),
        status: "running".into(),
        exit_code: None,
        pid: Some(42),
        created_at: Some("created".into()),
        display_name: None,
        tags: BTreeMap::from([("st3.subject".into(), binding.subject.clone())]),
    };
    assert_eq!(
        bound_pty_incarnation(&binding, &[observation.clone()]),
        Some(binding.agent_incarnation())
    );
    observation.pid = Some(43);
    assert_eq!(
        bound_pty_incarnation(&binding, &[observation.clone()]),
        None
    );
    observation.pid = Some(42);
    observation
        .tags
        .insert("st3.subject".into(), "agent/example/other".into());
    assert_eq!(bound_pty_incarnation(&binding, &[observation]), None);
}

#[test]
fn an_ended_harness_uses_the_registered_state() {
    assert_eq!(
        harness_activity_state(st_drivers::harness_state::Activity::Ended),
        "ended"
    );
    st3_schema::registry()
        .validate_claim(
            "agent/run/worker",
            "harness.observed",
            &BTreeMap::from([("state".into(), Value::String("ended".into()))]),
        )
        .unwrap();
}

#[test]
fn typed_claude_accepts_tui_options_and_rejects_headless_protocol_options() {
    reject_noninteractive_claude_argv(&["claude".into(), "--model".into(), "opus".into()])
        .unwrap();
    reject_noninteractive_claude_argv(&[
        "claude".into(),
        "--remote-control".into(),
        "cos".into(),
    ])
    .unwrap();

    for argv in [
        vec!["claude".into(), "-p".into()],
        vec!["claude".into(), "--print".into()],
        vec!["claude".into(), "--input-format=stream-json".into()],
        vec![
            "claude".into(),
            "--output-format".into(),
            "stream-json".into(),
        ],
    ] {
        let error = reject_noninteractive_claude_argv(&argv).unwrap_err();
        assert!(error.to_string().contains("use an `exec` declaration"));
    }
}

#[test]
fn claude_delivery_requires_the_exact_incarnations_prompt_submit_receipt() {
    let root = tempfile::tempdir().unwrap();
    let mut writer = st_drivers::harness_timeline::Writer::new(root.path(), "claude", "inc-2");
    writer
        .append(
            "prompt-1",
            st_drivers::harness_timeline::Role::User,
            st_drivers::harness_timeline::EntryType::Content,
            serde_json::json!({
                "text": "[st3-delivery:1784649988123-abc23z.md]\nhello"
            }),
            true,
        )
        .unwrap();

    assert!(
        claude_channel_consumed_delivery_filenames(root.path(), "inc-1")
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        claude_channel_consumed_delivery_filenames(root.path(), "inc-2").unwrap(),
        BTreeSet::from(["1784649988123-abc23z.md".to_owned()])
    );
}

#[test]
fn native_exit_claims_are_unique_per_incarnation() {
    assert_eq!(
        native_exit_key("agent/node.worker", "node.worker", "one"),
        native_exit_key("agent/node.worker", "node.worker", "one"),
    );
    assert_ne!(
        native_exit_key("agent/node.worker", "node.worker", "one"),
        native_exit_key("agent/node.worker", "node.worker", "two"),
    );
}

#[test]
fn claude_receipts_use_provider_session_not_runtime_incarnation() {
    let root = tempfile::tempdir().unwrap();
    let mut writer =
        st_drivers::harness_timeline::Writer::new(root.path(), "claude", "provider-current");
    writer
        .append(
            "prompt-1",
            st_drivers::harness_timeline::Role::User,
            st_drivers::harness_timeline::EntryType::Content,
            serde_json::json!({"text": "[st3-delivery:1784649988123-abc23z.md] hello"}),
            true,
        )
        .unwrap();
    let receipt_incarnation =
        claude_receipt_incarnation("runtime-current", Some("provider-current"));
    assert_eq!(
        claude_channel_consumed_delivery_filenames(root.path(), receipt_incarnation).unwrap(),
        BTreeSet::from(["1784649988123-abc23z.md".to_owned()]),
    );
}

#[test]
fn successor_never_adopts_the_predecessors_terminal_harness_record() {
    let predecessor = br#"{"state":"ended","exit":"exit 0"}"#;
    assert!(!harness_record_belongs_to_current_session(
        false,
        Some(predecessor),
        Some(predecessor),
    ));
    assert!(!harness_record_belongs_to_current_session(
        false,
        Some(predecessor),
        None,
    ));

    let claim = br#"{"state":"ended","reason":"superseded","incarnation":"next"}"#;
    assert!(harness_record_belongs_to_current_session(
        false,
        Some(predecessor),
        Some(claim),
    ));
    assert!(
        harness_record_belongs_to_current_session(true, Some(predecessor), Some(predecessor)),
        "once the successor fenced ownership, predecessor bytes cannot regain ownership"
    );
}

#[test]
fn extension_channel_state_outlives_its_st2_launch_placeholder() {
    for driver in ["pi", "omp"] {
        assert!(
            !native_file_may_override_channel(driver),
            "{driver} reports harness state through the ST3 extension channel"
        );
    }
    for driver in ["claude", "opencode"] {
        assert!(native_file_may_override_channel(driver));
    }
}

#[test]
fn message_references_round_trip_nested_ids_and_projected_files() {
    assert_eq!(
        normalize_message_reference("message/kickoff/run-1"),
        "kickoff/run-1"
    );
    assert_eq!(
        normalize_message_reference("kickoff%2Frun-1"),
        "kickoff/run-1"
    );
    assert_eq!(
        normalize_message_reference("/tmp/inbox/00000000000000000008-kickoff%2Frun-1.md"),
        "kickoff/run-1"
    );
}

#[test]
fn message_list_requires_one_exact_mailbox_and_explicit_identity_wins() {
    let bare = Cli::try_parse_from(["st3", "conversations", "ls"]).unwrap();
    let Command::Conversations {
        command: MessageCommand::Ls(bare),
    } = bare.command
    else {
        panic!("conversations ls did not parse");
    };
    assert!(bare.identity.is_none());

    assert_eq!(
        message_list_identity(None, Some("agent/from-environment".into())).unwrap(),
        "agent/from-environment"
    );
    assert_eq!(
        message_list_identity(
            Some("person/explicit".into()),
            Some("agent/from-environment".into())
        )
        .unwrap(),
        "person/explicit"
    );
    let error = message_list_identity(None, Some(String::new())).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("refusing to list every fleet message")
    );

    let explicit =
        Cli::try_parse_from(["st3", "conversations", "ls", "agent/explicit", "--archive"])
            .unwrap();
    let Command::Conversations {
        command: MessageCommand::Ls(explicit),
    } = explicit.command
    else {
        panic!("conversations ls with an identity did not parse");
    };
    assert_eq!(explicit.identity.as_deref(), Some("agent/explicit"));
    assert!(explicit.archive);
}

#[test]
fn message_send_and_reply_accept_a_key_for_unconfirmed_retries() {
    let send = Cli::try_parse_from([
        "st3",
        "conversations",
        "send",
        "agent/example/worker",
        "--from",
        "person/ada",
        "--body",
        "Hello",
        "--idempotency-key",
        "retry-a-send",
    ])
    .unwrap();
    let Command::Conversations {
        command: MessageCommand::Send(send),
    } = send.command
    else {
        panic!("send did not parse");
    };
    assert_eq!(send.idempotency_key.as_deref(), Some("retry-a-send"));
    let reply = Cli::try_parse_from([
        "st3",
        "conversations",
        "reply",
        "message/example",
        "--from",
        "person/ada",
        "--body",
        "Hello",
        "--idempotency-key",
        "retry-a-reply",
    ])
    .unwrap();
    let Command::Conversations {
        command: MessageCommand::Reply(reply),
    } = reply.command
    else {
        panic!("reply did not parse");
    };
    assert_eq!(reply.idempotency_key.as_deref(), Some("retry-a-reply"));

    let status = Cli::try_parse_from([
        "st3",
        "conversations",
        "status",
        "--idempotency-key",
        "retry-a-send",
    ])
    .unwrap();
    let Command::Conversations {
        command: MessageCommand::Status(status),
    } = status.command
    else {
        panic!("status did not parse");
    };
    assert_eq!(status.idempotency_key.as_deref(), Some("retry-a-send"));
    assert!(status.reference.is_none());
    assert!(Cli::try_parse_from(["st3", "conversations", "status"]).is_err());
    assert!(
        Cli::try_parse_from([
            "st3",
            "conversations",
            "status",
            "message/example",
            "--idempotency-key",
            "retry-a-send",
        ])
        .is_err()
    );
}

#[test]
fn a_derived_message_key_names_one_message_in_one_hour() {
    let request = MessageSendRequest {
        idempotency_key: String::new(),
        from: "person/avery".into(),
        to: "agent/example/worker".into(),
        content: "The merge train is live.".into(),
        title: Some("Merge train".into()),
        in_reply_to: None,
        tags: Vec::new(),
        attachments: Vec::new(),
    };
    let key = derived_message_key(&request, None, 490_000);
    // The key never changes between builds: a retry after an upgrade finds the first send.
    assert_eq!(key, "st3-message:v1:31bd263335cbfc5fa34d30ccafe56f43");
    assert_eq!(key, derived_message_key(&request.clone(), None, 490_000));
    assert_ne!(key, derived_message_key(&request, None, 490_001));
    assert_ne!(
        key,
        derived_message_key(&request, Some("incarnation/2"), 490_000)
    );
    let mut changes = Vec::new();
    for change in 0..8 {
        let mut changed = request.clone();
        match change {
            0 => changed.from = "person/blair".into(),
            1 => changed.to = "agent/example/other".into(),
            2 => changed.in_reply_to = Some("message/example".into()),
            3 => changed.title = None,
            4 => changed.content = "The merge train is paused.".into(),
            5 => changed.tags = vec!["work".into()],
            6 => {
                changed.attachments = vec![st3::model::AttachmentInput {
                    blob: format!("blob/{}", "a".repeat(64)),
                    media_type: "image/png".into(),
                    name: None,
                }]
            }
            _ => changed.idempotency_key = "ignored".into(),
        }
        changes.push(derived_message_key(&changed, None, 490_000));
    }
    // Everything the message carries names it; the key field it fills does not.
    assert!(changes[..7].iter().all(|changed| *changed != key));
    assert_eq!(changes[7], key);

    assert_eq!(shell_word("st3-message:v1:0f"), "st3-message:v1:0f");
    assert_eq!(shell_word("it's mine"), r"'it'\''s mine'");
}

#[test]
fn message_replies_route_to_the_other_participant() {
    let original = MessageView {
        subject: "message/original".into(),
        from: "agent/h".into(),
        to: "agent/s".into(),
        content: "Hello".into(),
        status: "read".into(),
        title: Some("Greeting".into()),
        in_reply_to: None,
        tags: Vec::new(),
        body_ref: None,
        body_bytes: None,
        body_origin: None,
        created_index: 1,
        attachments: Vec::new(),
    };

    assert_eq!(
        message_reply_recipient(&original, "agent/h").unwrap(),
        "agent/s"
    );
    assert_eq!(
        message_reply_recipient(&original, "agent/s").unwrap(),
        "agent/h"
    );
    let error = message_reply_recipient(&original, "agent/outsider").unwrap_err();
    assert!(
        error
            .to_string()
            .contains("cannot reply as a non-participant")
    );
}

#[test]
fn delegated_commands_require_complete_proof_and_keep_the_agent_actor() {
    for command in [
        vec![
            "work",
            "done",
            "step-run/example/ask",
            "--summary",
            "Friday",
        ],
        vec!["conversations", "archive", "message/notice"],
        vec!["attention", "approve", "step-run/example/build"],
        vec![
            "attention",
            "reject",
            "step-run/example/build",
            "--reason",
            "Stop",
        ],
        vec![
            "attention",
            "resolve",
            "attention/update",
            "--outcome",
            "resolved",
        ],
    ] {
        let mut args = vec!["st3"];
        args.extend(command);
        args.extend(["--as", "agent/example/helper", "--for", "person/avery"]);
        assert!(
            Cli::try_parse_from(&args).is_err(),
            "missing proof: {args:?}"
        );
        args.extend([
            "--policy",
            "claim/policy",
            "--instruction",
            "message/decision",
            "--quote",
            "Friday",
            "--episode",
            "claim/episode",
        ]);
        let parsed = Cli::try_parse_from(&args).unwrap();
        let (proof, actor) = match parsed.command {
            Command::Work {
                command: WorkCommand::Done(args),
            } => (args.delegation, args.actor),
            Command::Conversations {
                command: MessageCommand::Archive(args),
            } => (args.delegation, args.actor.unwrap()),
            Command::Attention {
                command: AttentionCommand::Approve(args) | AttentionCommand::Reject(args),
            } => (args.delegation, args.actor),
            Command::Attention {
                command: AttentionCommand::Resolve(args),
            } => (args.delegation, args.actor),
            _ => panic!("unexpected delegated command"),
        };
        assert_eq!(actor, "agent/example/helper");
        let proof = proof.proof(Some("claim/episode")).unwrap().unwrap();
        assert_eq!(proof.person, "person/avery");
        assert_eq!(proof.message, "message/decision");
    }
    assert!(
        Cli::try_parse_from([
            "st3",
            "work",
            "delegation",
            "--for",
            "person/avery",
            "--as",
            "person/avery",
            "--evidence",
            "claim/decision",
        ])
        .is_ok(),
        "a person can revoke by setting an empty action list"
    );
}

#[test]
fn message_archive_accepts_more_than_one_reference() {
    let cli = Cli::try_parse_from([
        "st3",
        "conversations",
        "archive",
        "first",
        "second",
        "third",
        "--as",
        "agent/sup",
    ])
    .unwrap();
    let Command::Conversations {
        command: MessageCommand::Archive(args),
    } = cli.command
    else {
        panic!("the archive command did not parse");
    };
    assert_eq!(args.references, ["first", "second", "third"]);
    assert_eq!(args.actor.as_deref(), Some("agent/sup"));
}

#[test]
fn message_read_accepts_multiple_canonical_references() {
    let cli = Cli::try_parse_from([
        "st3",
        "conversations",
        "read",
        "message/first",
        "message/second",
        "--as",
        "agent/sup",
        "--archive",
    ])
    .unwrap();
    let Command::Conversations {
        command: MessageCommand::Read(args),
    } = cli.command
    else {
        panic!("the read command did not parse");
    };
    assert_eq!(args.references, ["message/first", "message/second"]);
    assert_eq!(args.actor.as_deref(), Some("agent/sup"));
    assert!(args.archive);
}

#[test]
fn a_provenance_reads_as_one_line() {
    let line = provenance_line(&json!({
        "verdict": "verified", "signer": "person/example",
        "device": "example phone (secure enclave)", "key": "p256:BPLXtCkgqnBglKrxCU_RE"
    }));
    assert_eq!(
        line,
        "verified · person/example · example phone (secure enclave) · p256:BPLXtCkgqnB…"
    );
    assert_eq!(
        provenance_line(&json!({"verdict": "held", "reason": "delegation d1 has not arrived"})),
        "held · delegation d1 has not arrived"
    );
    assert_eq!(provenance_line(&json!({"verdict": "unsigned"})), "unsigned");
}

#[tokio::test]
async fn message_read_returns_the_committed_lifecycle_state() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let state = AppState {
        store: Arc::new(Store::open_memory("message-read-state").unwrap()),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "message-read-state".into(),
        state_dir: root.path().to_path_buf(),
        pty_root: root.path().join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: PlannerSpec::default(),
    };
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        serve_unix(&server_socket, router(state)).await.unwrap();
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(socket.exists(), "the test API socket did not start");
    let client = Client::unix(&socket);

    let first: MessageView = client
        .post(
            "/v1/messages",
            &MessageSendRequest {
                idempotency_key: "message-read-final-state".into(),
                from: "agent/sender".into(),
                to: "agent/sup".into(),
                content: "Read me".into(),
                title: None,
                in_reply_to: None,
                tags: Vec::new(),
                attachments: Vec::new(),
            },
        )
        .await
        .unwrap();
    let read = read_message_after_lifecycle(&client, &first.subject, "agent/sup", false)
        .await
        .unwrap();
    assert_eq!(read.status, "read");

    let second: MessageView = client
        .post(
            "/v1/messages",
            &MessageSendRequest {
                idempotency_key: "message-read-archive-final-state".into(),
                from: "agent/sender".into(),
                to: "agent/sup".into(),
                content: "Archive me".into(),
                title: None,
                in_reply_to: None,
                tags: Vec::new(),
                attachments: Vec::new(),
            },
        )
        .await
        .unwrap();
    let archived = read_message_after_lifecycle(&client, &second.subject, "agent/sup", true)
        .await
        .unwrap();
    assert_eq!(archived.status, "closed");

    server.abort();
}

#[test]
fn agents_requires_an_explicit_subcommand_and_exposes_seat_lifecycle_commands() {
    assert!(Cli::try_parse_from(["st3", "agents"]).is_err());

    let cli = Cli::try_parse_from(["st3", "agents", "ls", "--status", "running", "--enrich"])
        .unwrap();
    let Command::Agents {
        command: AgentsCommand::Ls(args),
    } = cli.command
    else {
        panic!("agents ls did not parse");
    };
    assert_eq!(args.status.as_deref(), Some("running"));
    assert!(args.enrich);

    let cli = Cli::try_parse_from(["st3", "agents", "tree", "--all"]).unwrap();
    let Command::Agents {
        command: AgentsCommand::Tree(args),
    } = cli.command
    else {
        panic!("agents tree did not parse");
    };
    assert!(args.all);

    let cli = Cli::try_parse_from(["st3", "agents", "show", "worker", "--all"]).unwrap();
    let Command::Agents {
        command: AgentsCommand::Show { subject, all },
    } = cli.command
    else {
        panic!("agents show did not parse");
    };
    assert_eq!(subject, "worker");
    assert!(all);

    let cli = Cli::try_parse_from([
        "st3",
        "agents",
        "start",
        "example/cos/standing/cos",
        "--harness",
        "claude",
        "--model",
        "opus",
        "--as",
        "person/alex",
        "--print-kdl",
    ])
    .unwrap();
    let Command::Agents {
        command: AgentsCommand::Start(args),
    } = cli.command
    else {
        panic!("agents start did not parse");
    };
    let kdl = agent_start_document(&args, None).unwrap();
    let intent = st3::parse_intent(&kdl, "node").unwrap();
    assert!(intent.subjects.contains_key("agent/example/cos/standing/cos"));
    assert!(
        intent.subjects["agent/example/cos/standing/cos"]
            .owner_run
            .is_none()
    );

    let cli = Cli::try_parse_from([
        "st3",
        "agents",
        "stop",
        "example/cos/standing/cos",
        "--as",
        "person/alex",
        "--print-kdl",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Command::Agents {
            command: AgentsCommand::Stop(_)
        }
    ));
}

#[test]
fn agent_rename_needs_a_nonempty_label_or_clear() {
    let rename = |label: &[&str]| {
        Cli::try_parse_from(
            ["st3", "agents", "rename", "agent/worker"]
                .into_iter()
                .chain(label.iter().copied())
                .chain(["--as", "person/alex"]),
        )
    };
    assert!(rename(&["Garden"]).is_ok());
    assert!(rename(&["--clear"]).is_ok());
    assert!(rename(&[""]).is_err());
    assert!(rename(&[]).is_err());
}

fn agent_new_args(arguments: &[&str]) -> AgentNewArgs {
    let cli = Cli::try_parse_from(
        ["st3", "agents", "new"]
            .into_iter()
            .chain(arguments.iter().copied()),
    )
    .unwrap();
    let Command::Agents {
        command: AgentsCommand::New(args),
    } = cli.command
    else {
        panic!("agents new did not parse");
    };
    args
}

#[test]
fn agent_new_cli_declares_checkout_and_preserves_plain_workspace() {
    let args = agent_new_args(&[
        "parser",
        "--repo",
        "/work/repo",
        "--base",
        "main",
        "--branch",
        "example/parser",
        "--remove-at-run-end",
    ]);
    let source = agent_new_document(&args, "/work/parser", true);
    let document: KdlDocument = source.parse().unwrap();
    let body = document.get("agent").unwrap().children().unwrap();
    let checkout = body.get("checkout").unwrap();
    assert_eq!(
        checkout.get(0).unwrap().as_string(),
        Some("/work/repo")
    );
    assert_eq!(
        checkout.get("branch").unwrap().as_string(),
        Some("example/parser")
    );
    assert!(body.get("workspace").unwrap().get("create").is_none());
    let args = agent_new_args(&["parser", "--workspace", "/work/plain"]);
    let source = agent_new_document(&args, "/work/plain", true);
    let intent = st3::graph::parse_intent(&source, "example").unwrap();
    assert_eq!(
        intent
            .subjects
            .values()
            .next()
            .unwrap()
            .member
            .as_ref()
            .unwrap()
            .workspace,
        "/work/plain"
    );
    for flag in ["--base", "--branch"] {
        assert!(Cli::try_parse_from(["st", "agents", "new", "parser", flag, "main"]).is_err());
    }
    assert!(
        Cli::try_parse_from(["st", "agents", "new", "parser", "--remove-at-run-end"]).is_err()
    );
    assert!(
        Cli::try_parse_from(["st", "agents", "repos", "--host", "host/example", "--json"])
            .is_ok()
    );
}

#[test]
fn creation_cli_accepts_a_literal_first_message_and_plain_shell_options() {
    let args = agent_new_args(&[
        "worker",
        "--harness",
        "codex",
        "--message",
        "--literal first message",
    ]);
    assert_eq!(args.message.as_deref(), Some("--literal first message"));
    assert_eq!(
        normalize_member_subject(
            "terminal/pty/person/ada/019a0000-0000-7000-8000-000000000001",
            "pty"
        ),
        "pty/person/ada/019a0000-0000-7000-8000-000000000001"
    );
    assert_eq!(
        normalize_member_subject("terminal/agent/test.worker", "pty"),
        "agent/test.worker"
    );
    let source = agent_new_document(&args, "/tmp", false);
    let intent = st3::graph::parse_intent(&source, "test").unwrap();
    assert!(intent.subjects.values().next().unwrap().member.is_some());
    let cli = Cli::try_parse_from([
        "st3",
        "terminals",
        "new",
        "Shell",
        "--host",
        "builder",
        "--cwd",
        "/tmp",
        "--as",
        "person/ada",
    ])
    .unwrap();
    let Command::Terminals {
        command: PtyCommand::New(args),
    } = cli.command
    else {
        panic!()
    };
    assert_eq!(args.name.as_deref(), Some("Shell"));
    assert_eq!(args.host.as_deref(), Some("builder"));
    let cli = Cli::try_parse_from([
        "st3",
        "terminals",
        "end",
        "terminal/pty/person/ada/019a0000-0000-7000-8000-000000000001",
        "--as",
        "person/ada",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Command::Terminals {
            command: PtyCommand::End(_)
        }
    ));
}

#[test]
fn a_new_claude_agent_is_the_fleet_claude_seat() {
    let args = agent_new_args(&[
        "site",
        "--host",
        "builder",
        "--model",
        "claude-opus-5-5",
        "--effort",
        "high",
        "--description",
        "Builds the example site.",
        "--attach",
    ]);
    assert!(args.attach && args.actor.is_none() && args.workspace.is_none());
    let kdl = agent_new_document(&args, "/home/example/st/agents/site", true);
    assert!(kdl.contains(r#"workspace "/home/example/st/agents/site" create=#true"#));
    let intent = st3::parse_intent(&kdl, "laptop").unwrap();
    let seat = &intent.subjects["agent/builder.site"];
    assert!(seat.owner_run.is_none());
    let member = seat.member.as_ref().unwrap();
    assert_eq!(member.host, "builder");
    assert_eq!(member.workspace, "/home/example/st/agents/site");
    assert!(member.workspace_create);
    assert_eq!(member.driver.as_deref(), Some("claude"));
    assert_eq!(member.restart, st3::model::RestartType::Always);
    assert_eq!(member.environment["CLAUDE_CODE_CHILD_SESSION"], "0");
    let st3::model::LaunchSpec::Argv(argv) = &member.launch else {
        panic!("a harness seat launches an argv");
    };
    let joined = argv.join(" ");
    for expected in [
        "--channels plugin:st-channel@st",
        "--model claude-opus-5-5",
        "--effort high",

    ] {
        assert!(
            joined.contains(expected),
            "{expected} is missing from {joined}"
        );
    }
    let render = seat.desired["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|child| child["name"] == "render")
        .unwrap();
    assert_eq!(render["children"][0]["arguments"], json!([".claude/"]));
    assert_eq!(
        render["children"][1]["arguments"],
        json!([".claude/settings.local.json", CLAUDE_SEAT_SETTINGS])
    );
}

#[test]
fn a_new_codex_agent_is_the_fleet_codex_seat() {
    let args = agent_new_args(&[
        "example/codex",
        "--harness",
        "codex",
        "--model",
        "gpt-example",
        "--effort",
        "medium",
        "--workspace",
        "/srv/example",
        "--print-kdl",
        "--as",
        "person/avery",
    ]);
    assert_eq!(args.actor.as_deref(), Some("person/avery"));
    let kdl = agent_new_document(&args, "/srv/example", false);
    assert!(!kdl.contains("create="));
    assert!(!kdl.contains("host "));
    let intent = st3::parse_intent(&kdl, "laptop").unwrap();
    let member = intent.subjects["agent/example/codex"]
        .member
        .as_ref()
        .unwrap();
    assert_eq!(member.host, "laptop");
    assert!(!member.workspace_create);
    assert!(member.environment.is_empty());
    let st3::model::LaunchSpec::Argv(argv) = &member.launch else {
        panic!("a harness seat launches an argv");
    };
    let joined = argv.join(" ");
    for expected in [
        "--model gpt-example",
        "-c model_reasoning_effort=medium",
    ] {
        assert!(
            joined.contains(expected),
            "{expected} is missing from {joined}"
        );
    }
    assert!(!kdl.contains("render") && !kdl.contains(".st3"));
}

#[test]
fn a_new_agent_needs_its_own_identity_inside_a_harness() {
    let command = Cli::try_parse_from(["st3", "agents", "new", "site"])
        .unwrap()
        .command;
    let refused = guard_mutating_cli_actor(&command, Some("agent/seat"), None).unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("needs explicit --as agent/seat")
    );
    let command = Cli::try_parse_from(["st3", "agents", "new", "site", "--as", "agent/seat"])
        .unwrap()
        .command;
    guard_mutating_cli_actor(&command, Some("agent/seat"), None).unwrap();
    assert!(
        Cli::try_parse_from(["st3", "agents", "new", "site", "--attach", "--print-kdl"])
            .is_err()
    );
}

#[test]
fn review_mutations_use_the_explicit_as_actor() {
    let cli = Cli::try_parse_from([
        "st3",
        "attention",
        "approve",
        "attention/item",
        "--as",
        "person/reviewer",
    ])
    .unwrap();
    let Command::Attention {
        command: AttentionCommand::Approve(args),
    } = cli.command
    else {
        panic!("review approve did not parse");
    };
    assert_eq!(args.actor, "person/reviewer");
    assert!(
        Cli::try_parse_from([
            "st3",
            "attention",
            "approve",
            "attention/item",
            "--as",
            "reviewer",
        ])
        .is_err()
    );
    assert!(Cli::try_parse_from(["st3", "attention", "approve", "attention/item",]).is_err());
}

#[test]
fn human_mutations_reject_missing_or_shorthand_identity() {
    let missing = [
        vec![
            "st3",
            "missions",
            "cancel",
            "mission-run/demo",
            "--reason",
            "done",
        ],
        vec!["st3", "launch", "start", "--id", "demo"],
        vec!["st3", "launch", "approve", "launch/demo", "hash"],
        vec!["st3", "launch", "run", "launch/demo"],
        vec!["st3", "launch", "cancel", "launch/demo"],
        vec!["st3", "import", "run", "session/demo"],
    ];
    for argv in missing {
        assert!(
            Cli::try_parse_from(&argv).is_err(),
            "accepted human action without --as: {argv:?}"
        );
    }

    for argv in [
        vec!["st3", "attention", "ls", "--as", "alex"],
        vec!["st3", "devices", "--as", "alex"],
        vec!["st3", "import", "run", "session/demo", "--as", "alex"],
    ] {
        assert!(
            Cli::try_parse_from(&argv).is_err(),
            "accepted shorthand human identity: {argv:?}"
        );
    }
}

#[test]
fn full_control_device_pairing_is_an_explicit_cli_choice() {
    let limited =
        Cli::try_parse_from(["st3", "devices", "--as", "person/alex", "pair", "iPhone"])
            .unwrap();
    let Command::Devices(DevicesArgs {
        command: Some(DevicesCommand::Pair { full_control, .. }),
        ..
    }) = limited.command
    else {
        panic!("expected a device pairing command")
    };
    assert!(!full_control);

    let full = Cli::try_parse_from([
        "st3",
        "devices",
        "--as",
        "person/alex",
        "pair",
        "--full-control",
        "iPhone",
    ])
    .unwrap();
    let Command::Devices(DevicesArgs {
        command: Some(DevicesCommand::Pair { full_control, .. }),
        ..
    }) = full.command
    else {
        panic!("expected a device pairing command")
    };
    assert!(full_control);

    let observer = Cli::try_parse_from([
        "st3",
        "devices",
        "--as",
        "person/alex",
        "pair",
        "--read-only",
        "iPad",
    ])
    .unwrap();
    let Command::Devices(DevicesArgs {
        command:
            Some(DevicesCommand::Pair {
                full_control,
                read_only,
                ..
            }),
        ..
    }) = observer.command
    else {
        panic!("expected a device pairing command")
    };
    assert!(read_only && !full_control);
    assert!(
        Cli::try_parse_from([
            "st3",
            "devices",
            "--as",
            "person/alex",
            "pair",
            "--read-only",
            "--full-control",
            "iPad",
        ])
        .is_err(),
        "read-only and full-control pairing must be mutually exclusive"
    );
}

#[test]
fn legacy_publish_is_not_a_public_alias() {
    assert!(
        Cli::try_parse_from(["st3", "publish", "mission.kdl", "--as", "agent/operator"])
            .is_err()
    );
}

#[test]
fn canonical_intent_helpers_print_current_direct_kdl() {
    let message = message_mission_intent(
        "message/test",
        "test",
        "person/sender",
        "person/recipient",
        "Hello.",
        Some("Greeting"),
        None,
        &["example".into()],
    );
    let message = st3::parse_intent(&message, "node").unwrap();
    assert!(message.missions.contains_key("message/test"));

    let planning = planning_session_intent(
        "planning/example/01990000000070008000000000000000",
        "example",
        &format!("doc/planning/example/request@{}", "a".repeat(64)),
        Path::new("/work/example"),
        "person/operator",
        &PlannerSpec::default(),
        None,
    );
    let planning = st3::parse_intent(&planning, "node").unwrap();
    assert_eq!(planning.planning_sessions.len(), 1);
}

#[test]
fn subscription_request_decisions_take_a_person_or_agent() {
    let cli = Cli::try_parse_from([
        "st3",
        "missions",
        "release",
        "request-id",
        "--as",
        "person/operator",
        "--reason",
        "the held review is real",
    ])
    .unwrap();
    let Command::Missions {
        command: MissionViewCommand::Release(args),
    } = cli.command
    else {
        panic!("the missions release command did not parse");
    };
    assert_eq!(args.actor, "person/operator");
    // Free mode: an agent decides as itself; a bare name is still refused.
    assert!(
        Cli::try_parse_from([
            "st3",
            "missions",
            "cancel-request",
            "request-id",
            "--as",
            "agent/node.triage",
            "--reason",
            "an agent decides too",
        ])
        .is_ok()
    );
    assert!(
        Cli::try_parse_from([
            "st3",
            "missions",
            "cancel-request",
            "request-id",
            "--as",
            "operator",
            "--reason",
            "a bare name is not an actor",
        ])
        .is_err()
    );
}

#[test]
fn work_revise_accepts_print_only_mode() {
    let cli = Cli::try_parse_from([
        "st3",
        "work",
        "revise",
        "mission-run/release",
        "release.kdl",
        "--reason",
        "add a gate",
        "--as",
        "person/operator",
        "--print-kdl",
    ])
    .unwrap();
    let Command::Work {
        command: WorkCommand::Revise(args),
    } = cli.command
    else {
        panic!("the work revise command did not parse");
    };
    assert!(args.print_kdl);
}

#[test]
fn work_revise_counts_only_top_level_missions() {
    let source = r#"
version 2
mission "review" state="ready" {
  goal "Review the source."
  loop "rounds" {
    max-rounds 2
    round {
      completion { when "all-steps-exhausted" }
      step "write" {
        goal "Write the review."
      }
    }
  }
}
"#;
    let parsed = st3::parse_intent(source, "local").unwrap();
    assert!(parsed.missions.len() > 1);
    assert_eq!(
        st3::mission::top_level_mission_ids(&parsed.missions),
        std::collections::BTreeSet::from(["review".to_owned()])
    );
}

#[test]
fn work_wake_is_explicit_and_terminal_ding_is_not_a_driver() {
    let cli = Cli::try_parse_from([
        "st3",
        "work",
        "wake",
        "step-run/example/work",
        "--as",
        "person/operator",
        "--reason",
        "retry native delivery",
    ])
    .unwrap();
    let Command::Work {
        command: WorkCommand::Wake(args),
    } = cli.command
    else {
        panic!("the work wake command did not parse");
    };
    assert_eq!(args.subject, "step-run/example/work");
    assert_eq!(args.actor.as_deref(), Some("person/operator"));
    assert!(Cli::try_parse_from(["st3", "driver", "ding"]).is_err());
}

#[test]
fn review_commands_parse_a_filter_and_an_owner_target() {
    let list =
        Cli::try_parse_from(["st3", "attention", "ls", "--as", "person/alex"]).unwrap();
    let Command::Attention {
        command: AttentionCommand::Ls { actor, .. },
    } = list.command
    else {
        panic!("the review list command did not parse");
    };
    assert_eq!(actor.as_deref(), Some("person/alex"));

    let approve = Cli::try_parse_from([
        "st3",
        "attention",
        "approve",
        "mission-run/release/one",
        "--as",
        "person/alex",
    ])
    .unwrap();
    let Command::Attention {
        command: AttentionCommand::Approve(args),
    } = approve.command
    else {
        panic!("the review approve command did not parse");
    };
    assert_eq!(args.target, "mission-run/release/one");
}

#[test]
fn attention_commands_parse_list_request_and_resolution() {
    let list =
        Cli::try_parse_from(["st3", "attention", "ls", "--as", "person/alex"]).unwrap();
    let Command::Attention {
        command: AttentionCommand::Ls { actor, .. },
    } = list.command
    else {
        panic!("the attention list command did not parse");
    };
    assert_eq!(actor.as_deref(), Some("person/alex"));

    let request = Cli::try_parse_from([
        "st3",
        "attention",
        "request",
        "--for",
        "person/alex",
        "--title",
        "Fabric needs review",
        "--reason",
        "The queue did not recover.",
        "--target",
        "mission-run/fabric",
        "--as",
        "agent/fabric/worker",
        "--idempotency-key",
        "fabric-fault",
    ])
    .unwrap();
    let Command::Attention {
        command: AttentionCommand::Request(args),
    } = request.command
    else {
        panic!("the attention request command did not parse");
    };
    assert_eq!(args.severity, "error");
    assert_eq!(args.targets, ["mission-run/fabric"]);
    assert_eq!(args.until, None);

    let until = Cli::try_parse_from([
        "st3",
        "attention",
        "request",
        "--for",
        "person/alex",
        "--title",
        "Publish this revision",
        "--reason",
        "Publish the prepared revision as a person.",
        "--target",
        "mission-run/release/one",
        "--until",
        "completed",
        "--as",
        "agent/release/worker",
    ])
    .unwrap();
    let Command::Attention {
        command: AttentionCommand::Request(args),
    } = until.command
    else {
        panic!("the attention request with until did not parse");
    };
    assert_eq!(args.until.as_deref(), Some("completed"));

    let resolve = Cli::try_parse_from([
        "st3",
        "attention",
        "resolve",
        "attention/fabric",
        "--outcome",
        "dismissed",
        "--as",
        "person/alex",
    ])
    .unwrap();
    let Command::Attention {
        command: AttentionCommand::Resolve(args),
    } = resolve.command
    else {
        panic!("the attention resolve command did not parse");
    };
    assert_eq!(args.subject, "attention/fabric");
    assert_eq!(args.outcome, "dismissed");
}

#[test]
fn a_native_driver_has_explicit_paths_without_catalogs() {
    let root = tempfile::tempdir().unwrap();
    for subject in [
        "agent/node.worker",
        "agent/example/app-web/standing/app-web",
    ] {
        let (driver_root, agent_dir, identity, runtime_id) =
            prepare_native_driver_in(subject, root.path()).unwrap();
        assert_eq!(agent_dir, driver_root.join("observations"));
        assert!(agent_dir.is_dir());
        assert!(!driver_root.join("catalog").exists());
        assert!(!driver_root.join("catalog.kdl").exists());
        assert!(!agent_dir.join("agent.kdl").exists());
        assert_eq!(identity, subject.strip_prefix("agent/").unwrap());
        assert_eq!(runtime_id, identity);
        assert_eq!(
            prepare_native_driver_in(subject, root.path()).unwrap().1,
            agent_dir
        );
    }
}

#[test]
fn predecessor_layout_survives_resume_without_rewriting_catalogs() {
    let drivers = tempfile::tempdir().unwrap();
    let subject = "agent/example/worker";
    let old = NativePaths::legacy_in(subject, "codex", drivers.path()).unwrap();
    fs::create_dir_all(&old.agent_dir).unwrap();
    fs::write(old.driver_root.join("catalog.kdl"), "predecessor bytes").unwrap();
    fs::write(old.agent_dir.join("agent.kdl"), "predecessor declaration").unwrap();
    fs::create_dir_all(&old.session_dir).unwrap();
    fs::write(old.session_dir.join("binding.json"), "provider binding").unwrap();
    let state = NativeLoopState {
        paths: Some(old.resolved()),
        ..NativeLoopState::default()
    };
    let encoded = serde_json::to_value(&state).unwrap();
    let back: NativeLoopState = serde_json::from_value(encoded.clone()).unwrap();
    let adopted = NativePaths::resumed(subject, "codex", back.paths).unwrap();
    assert_eq!(adopted.resolved(), old.resolved());
    assert_eq!(
        fs::read(adopted.session_dir.join("binding.json")).unwrap(),
        b"provider binding"
    );
    assert_eq!(
        fs::read(old.driver_root.join("catalog.kdl")).unwrap(),
        b"predecessor bytes"
    );
    assert!(!old.state_root().join("observations").exists());
    let mut predecessor = encoded;
    predecessor.as_object_mut().unwrap().remove("paths");
    assert!(
        serde_json::from_value::<NativeLoopState>(predecessor)
            .unwrap()
            .paths
            .is_none()
    );
}

#[test]
fn native_timeline_fences_provider_session_but_claims_runtime_session() {
    let record = st_drivers::harness_timeline::Record {
        schema: "st2.harness-timeline.v1".into(),
        driver: "claude".into(),
        incarnation_id: "provider-current".into(),
        next_sequence: 2,
        operations: Vec::new(),
    };
    assert!(timeline_record_is_current(
        &record,
        "claude",
        Some("provider-current")
    ));
    assert!(!timeline_record_is_current(
        &record,
        "claude",
        Some("provider-old")
    ));
    assert!(!timeline_record_is_current(
        &record,
        "codex",
        Some("provider-current")
    ));
    let fields = timeline_claim_fields(
        st_drivers::harness_timeline::Operation {
            operation: "append".into(),
            entry_id: "timeline-entry/test".into(),
            sequence: 1,
            revision: 1,
            role: "assistant".into(),
            entry_type: "content".into(),
            final_entry: true,
            body: json!({"text":"answer"}),
            driver: "claude".into(),
            incarnation_id: "provider-current".into(),
            observed_at_unix_ms: 1,
            source_id: None,
        },
        "runtime-current",
    );
    assert_eq!(fields["incarnation_id"], "runtime-current");
    assert!(fields.get("evidence_incarnation").is_none());
    assert_eq!(fields["body"]["text"], "answer");
}

#[test]
fn an_unread_native_message_is_ready_for_a_delivery_claim() {
    let root = tempfile::tempdir().unwrap();
    let inbox = root.path().join("inbox");
    let archive = root.path().join("archive");
    st_drivers::message::send_to_inbox(
        &inbox,
        "requester",
        Some("Start"),
        None,
        &["st3-message:message/kickoff".into()],
        "Do the work.",
    )
    .unwrap();

    assert_eq!(
        projected_message_subjects(&inbox, &archive).unwrap(),
        BTreeSet::from(["message/kickoff".into()])
    );
}

#[test]
fn graph_delivery_waits_for_the_exact_consumed_native_file() {
    let first = "1786380000000-aaa111.md";
    let second = "1786380000001-bbb222.md";
    let transport_accepted_only = BTreeSet::new();
    assert!(!native_delivery_receipted(&transport_accepted_only, first));

    let consumed = BTreeSet::from([first.to_owned()]);
    assert!(native_delivery_receipted(&consumed, first));
    assert!(!native_delivery_receipted(&consumed, second));
}

#[test]
fn a_graph_archive_moves_the_native_delivery_file() {
    let root = tempfile::tempdir().unwrap();
    let inbox = root.path().join("inbox");
    let archive = root.path().join("archive");
    let filename = st_drivers::message::send_to_inbox(
        &inbox,
        "requester",
        Some("Start"),
        None,
        &["st3-message:message/kickoff/run-1".into()],
        "Do the work.",
    )
    .unwrap();
    let closed = BTreeSet::from(["message/kickoff/run-1".into()]);
    sync_consumed_projected_messages(&inbox, &archive, &closed).unwrap();

    assert!(!inbox.join(&filename).exists());
    assert!(archive.join(filename).is_file());
}

#[test]
fn graph_delivery_idle_and_deliberate_hold_keep_mailbox_ready() {
    let gate = st_drivers::session_control::DeliveryGate::default();
    gate.unavailable();
    for transport in ["app-server", "opencode-server"] {
        assert_eq!(
            native_delivery_control_report(transport, &gate)["ready"],
            false
        );
    }
    gate.idle();
    assert!(gate.held(), "idle must close any handoff permission");
    for transport in ["app-server", "opencode-server"] {
        let report = native_delivery_control_report(transport, &gate);
        assert_eq!(
            report["ready"], true,
            "idle must clear an obsolete control failure"
        );
        assert!(report["reason"].is_null());
    }
    gate.update(true, DELIVERY_CONTROL_LEASE);
    assert!(gate.held(), "a deliberate hold must still block native input");
    for transport in ["app-server", "opencode-server"] {
        let report = native_delivery_control_report(transport, &gate);
        assert_eq!(report["ready"], true, "a deliberate hold is healthy control");
        assert!(report["reason"].is_null());
    }
}

#[tokio::test]
async fn graph_delivery_control_retries_once_and_keeps_late_reads_closed() {
    use axum::{Json, Router, routing::get};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use st_drivers::session_control::{DeliveryBlockReason, DeliveryGate};

    for delayed_reads in [1, usize::MAX] {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("control.sock");
        let reads = Arc::new(AtomicUsize::new(0));
        let count = reads.clone();
        let gate = DeliveryGate::default();
        gate.update(false, DELIVERY_CONTROL_LEASE);
        let handler_gate = gate.clone();
        let retry_held = Arc::new(AtomicBool::new(false));
        let handler_retry_held = retry_held.clone();
        let app = Router::new().route("/v1/delivery/hold", get(move || {
            let count = count.clone();
            let gate = handler_gate.clone();
            let retry_held = handler_retry_held.clone();
            async move {
                let attempt = count.fetch_add(1, Ordering::SeqCst);
                if attempt == 1 {
                    retry_held.store(gate.held(), Ordering::SeqCst);
                }
                if attempt < delayed_reads {
                    tokio::time::sleep(Duration::from_millis(350)).await;
                }
                Json(json!({"api_version":"st3.v1", "value": {
                    "subject":"agent/eval/gated", "active":false,
                    "until_unix_ms":null, "reason":null, "actor":null, "claim":null,
                }}))
            }
        }));
        let server_socket = socket.clone();
        let server = tokio::spawn(async move { serve_unix(&server_socket, app).await.unwrap() });
        for _ in 0..100 {
            if socket.exists() { break; }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(socket.exists());
        let client = Client::unix(&socket);
        let mut reported = Vec::new();
        let started = Instant::now();
        let result = refresh_graph_delivery_gate_with_report(
            &client, "agent/eval/gated", &gate,
            |gate| reported.push(gate.blocked_reason()),
        ).await;
        assert!(started.elapsed() < DELIVERY_CONTROL_LEASE);
        assert_eq!(reads.load(Ordering::SeqCst), 2, "exactly one retry");
        assert!(retry_held.load(Ordering::SeqCst), "native input must be closed during the retry");
        if delayed_reads == 1 {
            result.unwrap();
            assert_eq!(reported, [None], "a recovered retry must not flap health");
            assert!(!gate.held());
        } else {
            assert!(result.is_err());
            assert_eq!(reported, [Some(DeliveryBlockReason::ControlUnavailable)]);
            assert!(gate.held());
            tokio::time::sleep(Duration::from_millis(400)).await;
            assert!(gate.held(), "cancelled reads must never grant a permit later");
        }
        server.abort();
    }
}

#[tokio::test]
async fn graph_delivery_gate_closes_on_hold_outage_and_mismatched_subject() {
    use axum::{Json, Router, routing::get};
    let active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let wrong_subject = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handler_active = active.clone();
    let handler_wrong = wrong_subject.clone();
    let app = Router::new().route(
        "/v1/delivery/hold",
        get(move || {
            let active = handler_active.clone();
            let wrong = handler_wrong.clone();
            async move {
                Json(
                    json!({"api_version": "st3.v1", "value": st3::delivery_hold::HoldView {
                        subject: if wrong.load(std::sync::atomic::Ordering::SeqCst) {
                            "agent/eval/other"
                        } else {
                            "agent/eval/gated"
                        }
                        .into(),
                        active: active.load(std::sync::atomic::Ordering::SeqCst),
                        until_unix_ms: None,
                        reason: None,
                        actor: None,
                        claim: None,
                    }}),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = Client::new(Endpoint::Http(format!("http://{address}")));
    let gate = st_drivers::session_control::DeliveryGate::default();
    assert!(gate.held());
    refresh_graph_delivery_gate(&client, "agent/eval/gated", &gate)
        .await
        .unwrap();
    assert!(!gate.held());
    active.store(true, std::sync::atomic::Ordering::SeqCst);
    refresh_graph_delivery_gate(&client, "agent/eval/gated", &gate)
        .await
        .unwrap();
    assert!(gate.held());
    active.store(false, std::sync::atomic::Ordering::SeqCst);
    wrong_subject.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(
        refresh_graph_delivery_gate(&client, "agent/eval/gated", &gate)
            .await
            .is_err()
    );
    assert!(gate.held());
    wrong_subject.store(false, std::sync::atomic::Ordering::SeqCst);
    refresh_graph_delivery_gate(&client, "agent/eval/gated", &gate)
        .await
        .unwrap();
    assert!(!gate.held());
    let root = tempfile::tempdir().unwrap();
    let request = st3::delivery_hold::HoldRequest {
        subject: "agent/eval/gated".into(),
        actor: "agent/eval/gated".into(),
        held: true,
        until_unix_ms: u64::try_from(current_unix_ms().unwrap()).unwrap() + 60_000,
        reason: "adopted hold".into(),
        idempotency_key: "adopt-test".into(),
        legacy_adoption: true,
    };
    let deadline = request.until_unix_ms;
    let mut paths = NativePaths {
        driver_root: root.path().into(),
        session_dir: root.path().join("sessions"),
        agent_dir: root.path().into(),
        identity: "h.gated".into(),
        runtime_id: "gated".into(),
        delivery_gate: gate.clone(),
        pending_hold_adoption: Some(request),
    };
    assert!(
        refresh_native_delivery_control(&client, "agent/eval/gated", &mut paths, |_| {})
            .await
            .is_err()
    );
    assert!(gate.held());
    assert_eq!(
        paths.pending_hold_adoption.as_ref().unwrap().until_unix_ms,
        deadline
    );
    paths.pending_hold_adoption.as_mut().unwrap().until_unix_ms = 0;
    refresh_native_delivery_control(&client, "agent/eval/gated", &mut paths, |_| {})
        .await
        .unwrap();
    assert!(paths.pending_hold_adoption.is_none());
    assert!(!gate.held());
    server.abort();
    assert!(
        refresh_graph_delivery_gate(&client, "agent/eval/gated", &gate)
            .await
            .is_err()
    );
    assert!(gate.held());
}

#[test]
fn a_graph_read_releases_the_native_delivery_fifo() {
    let root = tempfile::tempdir().unwrap();
    let inbox = root.path().join("inbox");
    let archive = root.path().join("archive");
    let old = st_drivers::message::send_to_inbox(
        &inbox,
        "requester",
        Some("Already read"),
        None,
        &["st3-message:message/old".into()],
        "The recipient read this through the graph.",
    )
    .unwrap();
    let next = st_drivers::message::send_to_inbox(
        &inbox,
        "requester",
        Some("Still staged"),
        None,
        &["st3-message:message/next".into()],
        "This still needs native delivery.",
    )
    .unwrap();
    sync_consumed_projected_messages(&inbox, &archive, &BTreeSet::from(["message/old".into()]))
        .unwrap();
    sync_consumed_projected_messages(&inbox, &archive, &BTreeSet::from(["message/old".into()]))
        .unwrap();

    assert!(!inbox.join(&old).exists());
    assert!(archive.join(old).is_file());
    assert!(inbox.join(next).is_file());
    assert_eq!(st_drivers::message::list_dir(&archive).unwrap().len(), 1);
}

#[tokio::test]
async fn a_closed_message_missing_from_the_active_page_archives_its_projected_file() {
    use axum::{Json, Router, routing::get};

    let app = Router::new()
        .route(
            "/v1/messages/page",
            get(|| async {
                Json(serde_json::json!({
                    "api_version": "st3.v1",
                    "value": { "items": [], "has_more": false, "next_cursor": null, "limit": 100 }
                }))
            }),
        )
        .route(
            "/v1/messages/read/{*subject}",
            get(|| async {
                Json(serde_json::json!({
                    "api_version": "st3.v1",
                    "value": {
                        "subject": "message/closed", "from": "agent/sender", "to": "agent/test",
                        "content": "done", "status": "closed", "created_index": 1
                    }
                }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = Client::new(Endpoint::Http(format!("http://{address}")));
    let root = tempfile::tempdir().unwrap();
    let inbox = root.path().join("inbox");
    let archive = root.path().join("archive");
    let filename = st_drivers::message::send_to_inbox(
        &inbox,
        "agent/sender",
        Some("done"),
        None,
        &["st3-message:message/closed".into()],
        "done",
    )
    .unwrap();

    forward_projected_messages(
        &client,
        "agent/test",
        &inbox,
        &archive,
        "claude-channel",
        NativeDeliveryReceipts::ClaudeChannel {
            agent_dir: root.path(),
            incarnation: "one",
        },
    )
    .await
    .unwrap();
    assert!(!inbox.join(&filename).exists());
    assert!(archive.join(filename).is_file());
    server.abort();
}

#[tokio::test]
async fn legacy_native_mailbox_replays_delivered_unread_once_and_keeps_read_closed_final() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open_memory("replay").unwrap());
    let seat = "agent/eval.worker";
    for final_status in ["delivered", "read", "closed"] {
        let subject = format!("message/{final_status}");
        store
            .append_claim(&ClaimInput {
                subject: subject.clone(),
                kind: "message.sent".into(),
                actor: Some("person/eval".into()),
                fields: BTreeMap::from([
                    ("from".into(), json!("person/eval")),
                    ("to".into(), json!(seat)),
                    ("content".into(), json!("Recovered brief")),
                    ("status".into(), json!("sent")),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        for status in ["delivered", "read", "closed"] {
            store
                .append_claim(&ClaimInput {
                    subject: subject.clone(),
                    kind: format!("message.{status}"),
                    actor: Some(seat.into()),
                    fields: BTreeMap::from([("status".into(), json!(status))]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            if status == final_status {
                break;
            }
        }
    }
    let (client, server) = serve_test_store(store.clone(), root.path(), "replay").await;
    let inbox = root.path().join("inbox");
    let archive = root.path().join("archive");
    let mut first_filename = None;
    for _ in 0..3 {
        forward_projected_messages(
            &client,
            seat,
            &inbox,
            &archive,
            "app-server",
            NativeDeliveryReceipts::Codex {
                state_dir: root.path(),
                identity: "eval.worker",
                runtime_id: "worker",
            },
        )
        .await
        .unwrap();
        let messages = st_drivers::message::list_dir(&inbox).unwrap();
        let subjects = messages
            .iter()
            .flat_map(|message| &message.tags)
            .filter_map(|tag| tag.strip_prefix("st3-message:"))
            .collect::<Vec<_>>();
        assert_eq!(subjects, ["message/delivered"]);
        let filename = &messages[0].filename;
        if let Some(first) = &first_filename {
            assert_eq!(filename, first, "replay must retain the native handoff identity");
        } else {
            first_filename = Some(filename.clone());
        }
    }
    assert_eq!(store.message("message/delivered").unwrap().unwrap().status, "delivered");
    assert_eq!(store.message("message/read").unwrap().unwrap().status, "read");
    assert_eq!(store.message("message/closed").unwrap().unwrap().status, "closed");
    server.abort();
}

#[tokio::test]
async fn consumed_delivered_unread_mail_posts_no_lifecycle_claim_on_any_poll() {
    use axum::{
        Json, Router,
        extract::Path as AxumPath,
        routing::{get, post},
    };

    // #1085: a seat holding many delivered-unread messages re-posted `delivered` for every
    // one of them on every poll, and each repeat woke every mailbox reader in the daemon.
    let posts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let recorded = posts.clone();
    let app = Router::new()
        .route(
            "/v1/messages/page",
            get(|| async {
                let message = |subject: &str, status: &str, index: u64| {
                    json!({"subject": subject, "from": "agent/sender", "to": "agent/test",
                        "content": "Signal", "status": status, "created_index": index})
                };
                Json(json!({"api_version": "st3.v1", "value": {
                    "items": [message("message/delivered", "delivered", 1),
                              message("message/staged", "staged", 2)],
                    "has_more": false, "next_cursor": null, "limit": 200
                }}))
            }),
        )
        .route(
            "/v1/messages/{message_id}/claims",
            post(move |AxumPath(message_id): AxumPath<String>| {
                let recorded = recorded.clone();
                async move {
                    recorded.lock().unwrap().push(message_id.clone());
                    Json(json!({"api_version": "st3.v1", "value": {
                        "id": "claim/1", "store_index": 1, "batch_id": "batch/1",
                        "subject": message_id, "kind": "message.delivered", "origin": "test",
                        "actor": "agent/test", "body": {}, "predecessors": [],
                        "accepted_at_unix_ms": 1
                    }}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = Client::new(Endpoint::Http(format!("http://{address}")));
    let root = tempfile::tempdir().unwrap();
    let inbox = root.path().join("inbox");
    let archive = root.path().join("archive");
    let mut writer = st_drivers::harness_timeline::Writer::new(root.path(), "claude", "one");
    for subject in ["message/delivered", "message/staged"] {
        let filename = st_drivers::message::send_to_inbox(
            &inbox,
            "agent/sender",
            None,
            None,
            &[format!("st3-message:{subject}")],
            "Signal",
        )
        .unwrap();
        writer
            .append(
                subject,
                st_drivers::harness_timeline::Role::User,
                st_drivers::harness_timeline::EntryType::Content,
                json!({"text": format!("[st3-delivery:{filename}]\nSignal")}),
                true,
            )
            .unwrap();
    }
    for _ in 0..5 {
        forward_projected_messages(
            &client,
            "agent/test",
            &inbox,
            &archive,
            "claude-channel",
            NativeDeliveryReceipts::ClaudeChannel {
                agent_dir: root.path(),
                incarnation: "one",
            },
        )
        .await
        .unwrap();
    }
    let posts = posts.lock().unwrap().clone();
    // A consumed message still in `staged` records its delivery (here the mock never
    // settles it, so it is posted on each poll); an already-delivered one is never posted.
    assert!(posts.iter().all(|id| id.contains("staged")), "{posts:?}");
    assert_eq!(posts.len(), 5, "{posts:?}");
    server.abort();
}

#[tokio::test]
async fn a_projected_message_envelope_names_the_graph_recipient_and_exact_body_hash() {
    use axum::{Json, Router, routing::get};

    let app = Router::new().route(
        "/v1/messages/page",
        get(|| async {
            Json(serde_json::json!({
                "api_version": "st3.v1",
                "value": {
                    "items": [{
                        "subject": "message/fact", "from": "agent/run-1/wake.left",
                        "to": "agent/run-1/wake.right", "content": "FACT <b>QUARTZ</b>",
                        "status": "staged", "title": "Fact", "created_index": 1
                    }],
                    "has_more": false, "next_cursor": null, "limit": 100
                }
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = Client::new(Endpoint::Http(format!("http://{address}")));
    let root = tempfile::tempdir().unwrap();
    let inbox = root.path().join("inbox");
    let archive = root.path().join("archive");

    forward_projected_messages(
        &client,
        "agent/run-1/wake.right",
        &inbox,
        &archive,
        "codex",
        NativeDeliveryReceipts::ClaudeChannel {
            agent_dir: root.path(),
            incarnation: "one",
        },
    )
    .await
    .unwrap();
    server.abort();

    let projected = st_drivers::message::list_inbox(&inbox).unwrap();
    assert_eq!(projected.len(), 1);
    // The inbox file appends a newline, so the hash must come from the graph content.
    assert_eq!(projected[0].body, "FACT <b>QUARTZ</b>\n");
    let catalog = tempfile::tempdir().unwrap();
    assert_eq!(
        st_drivers::ding::poke_text(catalog.path(), "h", "run-1/wake.right", &projected[0]),
        format!(
            "<smalltalk-message id=\"fact\" from=\"agent/run-1/wake.left\" \
                 to=\"agent/run-1/wake.right\" subject=\"Fact\" sha256=\"{}\" \
                 graph=\"message/fact\">\nFACT &lt;b&gt;QUARTZ&lt;/b&gt;\n</smalltalk-message>",
            st_drivers::ding::st3_body_sha256("FACT <b>QUARTZ</b>")
        )
    );
}

/// A renewal race, where the step's claim ended between reading the work and renewing it, is
/// not a reason to end the driver. Any other API error still is.
#[tokio::test]
async fn a_renewal_that_lost_its_claim_is_skipped_rather_than_ending_the_driver() {
    use axum::{Router, http::StatusCode, response::IntoResponse as _, routing::post};

    let app = Router::new()
        .route(
            "/v1/work/renew/step-run/lost",
            post(|| async {
                (
                    StatusCode::CONFLICT,
                    axum::Json(serde_json::json!({
                        "code": "work-not-claimed",
                        "message": "the step is not claimed"
                    })),
                )
                    .into_response()
            }),
        )
        .route(
            "/v1/work/renew/step-run/broken",
            post(|| async {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({
                        "code": "internal",
                        "message": "the store failed"
                    })),
                )
                    .into_response()
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = Client::new(Endpoint::Http(format!("http://{address}")));
    let renew = |step: &'static str| {
        let client = &client;
        async move {
            client
                .post::<_, Value>(
                    &format!("/v1/work/renew/step-run/{step}"),
                    &serde_json::json!({}),
                )
                .await
                .unwrap_err()
        }
    };
    assert!(renewal_lost_its_claim(&renew("lost").await));
    assert!(!renewal_lost_its_claim(&renew("broken").await));
    server.abort();
}

#[tokio::test]
async fn one_message_that_cannot_be_forwarded_does_not_hold_back_the_next() {
    use axum::{Json, Router, routing::get};

    let app = Router::new().route(
        "/v1/messages/page",
        get(|| async {
            Json(serde_json::json!({
                "api_version": "st3.v1",
                "value": {
                    "items": [
                        {
                            "subject": "message/missing", "from": "agent/sender",
                            "to": "agent/test", "content": "doc/notes/missing@abc",
                            "status": "staged", "created_index": 1
                        },
                        {
                            "subject": "message/next", "from": "agent/sender",
                            "to": "agent/test", "content": "the next message",
                            "status": "staged", "created_index": 2
                        }
                    ],
                    "has_more": false, "next_cursor": null, "limit": 100
                }
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = Client::new(Endpoint::Http(format!("http://{address}")));
    let root = tempfile::tempdir().unwrap();
    let inbox = root.path().join("inbox");
    let archive = root.path().join("archive");

    let unforwarded = forward_projected_messages(
        &client,
        "agent/test",
        &inbox,
        &archive,
        "claude-channel",
        NativeDeliveryReceipts::ClaudeChannel {
            agent_dir: root.path(),
            incarnation: "one",
        },
    )
    .await
    .unwrap();
    server.abort();

    assert_eq!(
        unforwarded
            .iter()
            .map(|(message, _)| message.as_str())
            .collect::<Vec<_>>(),
        ["message/missing"]
    );
    let projected = st_drivers::message::list_inbox(&inbox).unwrap();
    assert_eq!(projected.len(), 1);
    assert_eq!(projected[0].body, "the next message\n");
}

#[tokio::test]
async fn a_malformed_message_page_degrades_and_recovers_without_ending_delivery() {
    use axum::{Json, Router, response::IntoResponse as _, routing::get};

    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let get_calls = calls.clone();
    let post_observed = observed.clone();
    let app = Router::new()
        .route(
            "/v1/messages/page",
            get(move || {
                let calls = get_calls.clone();
                async move {
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        axum::response::Response::builder()
                            .status(200)
                            .body(axum::body::Body::from("{\"api_version\":\"st3.v1\",\"value\":\""))
                            .unwrap()
                    } else {
                        Json(serde_json::json!({
                            "api_version": "st3.v1",
                            "value": {"items": [], "has_more": false, "next_cursor": null, "limit": 100}
                        }))
                        .into_response()
                    }
                }
            }),
        )
        .route(
            "/v1/claims",
            axum::routing::post(move |Json(body): Json<Value>| {
                let observed = post_observed.clone();
                async move {
                    observed.lock().unwrap().push(
                        body["fields"]["code"].as_str().unwrap().to_owned(),
                    );
                    Json(serde_json::json!({
                        "api_version": "st3.v1",
                        "value": {
                            "id":"claim/test", "store_index":1, "batch_id":"batch/test",
                            "subject":"agent/test", "kind":"harness.diagnostic", "origin":"test",
                            "actor":"agent/test", "body":{}, "predecessors":[],
                            "accepted_at_unix_ms":1
                        }
                    }))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = Client::new(Endpoint::Http(format!("http://{address}")));
    let root = tempfile::tempdir().unwrap();
    let inbox = root.path().join("inbox");
    let archive = root.path().join("archive");
    let mut supervisor = NativeDeliverySupervisor::default();

    supervise_native_delivery(
        &client,
        "agent/test",
        &inbox,
        &archive,
        "claude-channel",
        NativeDeliveryReceipts::ClaudeChannel {
            agent_dir: root.path(),
            incarnation: "one",
        },
        "one",
        &mut supervisor,
    )
    .await;
    assert_eq!(supervisor.failures, 1);
    assert!(!supervisor.ready());
    supervisor.retry_after = None;
    supervise_native_delivery(
        &client,
        "agent/test",
        &inbox,
        &archive,
        "claude-channel",
        NativeDeliveryReceipts::ClaudeChannel {
            agent_dir: root.path(),
            incarnation: "one",
        },
        "one",
        &mut supervisor,
    )
    .await;
    assert_eq!(supervisor.failures, 0);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        *observed.lock().unwrap(),
        ["native-delivery-degraded", "native-delivery-recovered"]
    );
    server.abort();
}

#[test]
fn mission_follow_stops_for_completed_and_standing_runs() {
    assert!(mission_run_follow_succeeded("completed"));
    assert!(mission_run_follow_succeeded("standing"));
    assert!(!mission_run_follow_succeeded("running"));
    assert!(!mission_run_follow_succeeded("failed"));
}

#[test]
fn a_runtime_driver_retries_a_transient_st3_api_outage() {
    let mut last_warning = None;
    tolerate_driver_api_outage(
        "agent/run/worker",
        anyhow::anyhow!("incomplete HTTP response"),
        &mut last_warning,
    )
    .unwrap();
    assert!(last_warning.is_some());
}

#[test]
fn a_runtime_driver_retries_a_truncated_json_envelope() {
    let mut last_warning = None;
    let parse_error = serde_json::from_str::<Value>("\"").unwrap_err();
    tolerate_driver_api_outage(
        "agent/run/worker",
        anyhow::Error::new(parse_error).context("decode the st API response envelope"),
        &mut last_warning,
    )
    .unwrap();
    assert!(last_warning.is_some());
}

#[test]
fn a_runtime_driver_does_not_retry_a_semantic_api_error() {
    let mut last_warning = None;
    let error = tolerate_driver_api_outage(
        "agent/run/worker",
        anyhow::anyhow!("the work claim is stale"),
        &mut last_warning,
    )
    .unwrap_err();
    assert!(error.to_string().contains("work claim is stale"));
    assert!(last_warning.is_none());
}

async fn serve_test_store(
    store: Arc<Store>,
    root: &Path,
    node: &str,
) -> (Client, tokio::task::JoinHandle<()>) {
    let socket = root.join("st3.sock");
    let state = AppState {
        store,
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: node.into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: PlannerSpec::default(),
    };
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        serve_unix(&server_socket, router(state)).await.unwrap();
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(socket.exists(), "the test API socket did not start");
    (Client::unix(&socket), server)
}

async fn publish_test_mission(client: &Client, root: &Path, goal: &str) {
    let file = root.join("mission.kdl");
    fs::write(
        &file,
        format!(
            "version 2\nmission \"arrival\" state=\"ready\" {{\n  goal \"{goal}\"\n  step \"work\" {{ }}\n}}\n"
        ),
    )
    .unwrap();
    run_apply(
        client,
        ApplyArgs {
            files: vec![file],
            set: None,
            repository: None,
            source_ref: None,
            sha: None,
            source_sequence: None,
            expect_set: None,
            rollout: None,
            rollout_deadline: "30m".into(),
            force_after_deadline: false,
            adopt: vec![],
            allow_empty: false,
            confirm_retire: None,
            dry_run: false,
            at_index: None,
            actor: "person/test".into(),
            workspace: root.to_owned(),
            no_gate_check: false,
            check: false,
            inputs: vec![],
        },
        true,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn mission_start_same_bare_id_creates_distinct_runs() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open_memory("single").unwrap());
    let (client, server) = serve_test_store(store.clone(), root.path(), "single").await;
    for mission in ["fleet/first", "fleet/second"] {
        publish_text(
            &client,
            format!(
                "version 2\nmission \"{mission}\" state=\"ready\" {{ goal \"Test run names\"; step \"work\" {{ }} }}\n"
            ),
            "test mission".into(),
            "person/test".into(),
        )
        .await
        .unwrap();
        start_mission_run(
            &client,
            MissionRunStartArgs {
                mission: format!("mission/{mission}"),
                revision: None,
                id: Some("2026-10-07".into()),
                workspace: root.path().to_path_buf(),
                inputs: Vec::new(),
                after: None,
                report_to: None,
                report_completed: false,
                follow: false,
                actor: "person/test".into(),
                print_kdl: false,
            },
            true,
        )
        .await
        .unwrap();
        let subject = format!("mission-run/{mission}/2026-10-07");
        assert_eq!(
            store.mission_run(&subject).unwrap().unwrap().subject,
            subject
        );
    }
    assert!(
        store
            .mission_run("mission-run/2026-10-07")
            .unwrap()
            .is_none()
    );
    server.abort();
}

#[tokio::test]
async fn missions_start_waits_for_a_revision_published_on_another_host() {
    const FLEET: &str = "5d0c1c52-3f7a-4b0e-9b61-1f2d3c4b5a69";
    let publisher_root = tempfile::tempdir().unwrap();
    let starter_root = tempfile::tempdir().unwrap();
    let publisher = Arc::new(Store::open_memory("publisher").unwrap());
    let starter = Arc::new(Store::open_memory("starter").unwrap());
    publisher.bind_fleet(FLEET).unwrap();
    starter.bind_fleet(FLEET).unwrap();
    let (publisher_client, publisher_server) =
        serve_test_store(publisher.clone(), publisher_root.path(), "publisher").await;
    let (starter_client, starter_server) =
        serve_test_store(starter.clone(), starter_root.path(), "starter").await;
    publish_test_mission(
        &publisher_client,
        publisher_root.path(),
        "Start after replication.",
    )
    .await;
    let revision = publisher
        .mission_spec("arrival", None)
        .unwrap()
        .unwrap()
        .revision;

    let replicate = tokio::spawn({
        let publisher = publisher.clone();
        let starter = starter.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(600)).await;
            let exchange = publisher
                .export_replication_exchange(
                    FLEET,
                    &st3::model::ReplicationInventory::default(),
                )
                .unwrap();
            starter
                .receive_replication_exchange("publisher", FLEET, &exchange)
                .unwrap();
            starter.validate_replication_backlog().unwrap();
            starter.apply_replication_repairs().unwrap();
            starter.project_replication_backlog().unwrap();
        }
    });
    let waited = Instant::now();
    start_mission_run(
        &starter_client,
        MissionRunStartArgs {
            mission: "arrival".into(),
            revision: Some(revision.clone()),
            id: Some("arrival/after-replication".into()),
            workspace: starter_root.path().to_path_buf(),
            inputs: Vec::new(),
            after: None,
            report_to: None,
            report_completed: false,
            follow: false,
            actor: "person/test".into(),
            print_kdl: false,
        },
        true,
    )
    .await
    .unwrap();
    assert!(
        waited.elapsed() >= Duration::from_millis(500),
        "start waited for the publish to replicate instead of failing"
    );
    replicate.await.unwrap();
    let run: MissionRunView = starter_client
        .get("/v1/mission-runs/mission-run%2Farrival%2Fafter-replication")
        .await
        .unwrap();
    assert_eq!(run.revision, revision);
    publisher_server.abort();
    starter_server.abort();
}

#[tokio::test]
async fn missions_start_names_a_replaced_or_missing_revision() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open_memory("single").unwrap());
    let (client, server) = serve_test_store(store.clone(), root.path(), "single").await;
    publish_test_mission(&client, root.path(), "First revision.").await;
    let first = store
        .mission_spec("arrival", None)
        .unwrap()
        .unwrap()
        .revision;
    publish_test_mission(&client, root.path(), "Second revision.").await;
    let second = store
        .mission_spec("arrival", None)
        .unwrap()
        .unwrap()
        .revision;
    assert_ne!(first, second);

    let replaced = Instant::now();
    let error = startable_mission(&client, "arrival", Some(&first), Duration::from_secs(10))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains(&format!(
            "revision {first} was replaced by revision {second}"
        )),
        "{error}"
    );
    assert!(replaced.elapsed() < Duration::from_secs(5));

    let error = startable_mission(
        &client,
        "arrival",
        Some(&"0".repeat(64)),
        Duration::from_secs(1),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("has not reached this host yet after 1s"),
        "{error}"
    );
    let error = startable_mission(&client, "absent", None, Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("mission/absent has not reached this host yet after 1s"),
        "{error}"
    );

    let publications = mission_publications(&client, "arrival").await.unwrap();
    assert_eq!(publications.len(), 2);
    assert!(
        started_revision_note("arrival", &second, &publications)
            .ends_with("on single. 1 older revision shares this mission name.")
    );
    assert!(
        started_revision_note("arrival", &first, &publications)
            .ends_with("on single. 1 other revision shares this mission name.")
    );
    assert_eq!(
        started_revision_note("arrival", &second, &publications[..1]),
        format!("Started mission/arrival revision {second}.")
    );
    server.abort();
}
#[tokio::test]
async fn committed_observations_survive_daemon_outage_lost_ack_and_driver_reexec() {
    use axum::{Json, Router, http::StatusCode, response::IntoResponse as _, routing::post};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    let root = tempfile::tempdir().unwrap();
    st_drivers::harness_events::enable(root.path(), "runtime-a").unwrap();
    let seq =
        st_drivers::harness_state::claim(root.path(), "example/seat", "claude", "provider-a")
            .unwrap();
    let mut writer = st_drivers::harness_state::Writer::new(
        root.path(),
        "example/seat",
        "claude",
        Some("pty".into()),
    )
    .with_ownership("provider-a", seq);
    writer
        .observe(st_drivers::harness_state::Observation::new(
            st_drivers::harness_state::Activity::Active,
            st_drivers::harness_state::BlockedOn::None,
            st_drivers::harness_state::InputBuffer::Unknown,
        ))
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let client = Client::new(st3::client::Endpoint::Http(format!("http://{address}")));
    let mut observations = NativeObservations::start(root.path(), "runtime-a").unwrap();
    let mut ready = false;
    assert!(
        observations
            .drain(&client, "agent/example/seat", "claude", &mut ready)
            .await
            .is_err()
    );
    assert_eq!(
        st_drivers::harness_events::pending(root.path(), 100)
            .unwrap()
            .len(),
        1
    ); // Only the non-observation claim placeholder was acknowledged locally.
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let store = Arc::new(Store::open_memory("amber").unwrap());
    let refuse_once = Arc::new(AtomicBool::new(true));
    let app = Router::new().route(
        "/v1/harness-events",
        post({
            let captured = captured.clone();
            let store = store.clone();
            move |Json(request): Json<st3::harness_events::Publication>| {
                let captured = captured.clone();
                let store = store.clone();
                let refuse_once = refuse_once.clone();
                async move {
                    captured
                        .lock()
                        .unwrap()
                        .push(serde_json::to_value(&request).unwrap());
                    let record = store.append_claim(&request.claim).unwrap();
                    if refuse_once.swap(false, Ordering::SeqCst) {
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({"error":"acknowledgement lost"})),
                        )
                            .into_response()
                    } else {
                        Json(json!({"api_version":"st3.v1","value":record})).into_response()
                    }
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(address).await.unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    assert!(
        observations
            .drain(&client, "agent/example/seat", "claude", &mut ready)
            .await
            .is_err()
    );
    assert_eq!(
        st_drivers::harness_events::pending(root.path(), 100)
            .unwrap()
            .len(),
        1
    );
    drop(observations); // A replacement image reconstructs all pending work from the outbox.
    let mut replacement = NativeObservations::start(root.path(), "runtime-a").unwrap();
    replacement
        .drain(&client, "agent/example/seat", "claude", &mut ready)
        .await
        .unwrap();
    assert!(ready);
    assert!(
        st_drivers::harness_events::pending(root.path(), 100)
            .unwrap()
            .is_empty()
    );
    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0], captured[1]);
    assert_eq!(store.local_observations_tail(100).unwrap().len(), 1);
    server.abort();
}
#[tokio::test(start_paused = true)]
async fn resumed_driver_retry_ticks_wait_for_settle_without_delaying_delivery() {
    let started = tokio::time::Instant::now();
    let settle = ResumeSettle::after(Duration::from_secs(30));
    let mut control = st3::client::DriverRetry::default();
    let mut work = st3::client::DriverRetry::default();
    let mut control_interval = tokio::time::interval(Duration::from_secs(10));
    let mut work_interval = tokio::time::interval(Duration::from_secs(1));
    control_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    work_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut attempts = 0;
    for delivery_tick in 0..3 {
        // This is the production control-arm wake and eligibility result: local delivery
        // keeps ticking, but the report body cannot run or record success/failure yet.
        let settled = settle.control_tick(&mut control, &mut control_interval).await;
        let expected = Duration::from_secs(delivery_tick * 10);
        let elapsed = tokio::time::Instant::now() - started;
        assert!(elapsed >= expected && elapsed <= expected + Duration::from_millis(5));
        assert!(!settled);
        if settled && control.ready() {
            attempts += 1;
            control.failed(&anyhow::anyhow!("control deadline"));
        }
    }
    assert_eq!(attempts, 0);
    // Even the work interval's initial immediate tick stays unconsumed before eligibility.
    assert!(tokio::time::timeout(
        Duration::from_secs(9), settle.retry_tick(&mut work, &mut work_interval)
    ).await.is_err());
    assert!(!settle.passed());
    tokio::time::advance(Duration::from_secs(1)).await;
    let settled = settle.control_tick(&mut control, &mut control_interval).await;
    assert!(settled && control.ready());
    attempts += 1;
    assert_eq!(attempts, 1);
    settle.retry_tick(&mut work, &mut work_interval).await;
    let elapsed = tokio::time::Instant::now() - started;
    assert!(elapsed >= Duration::from_secs(30) && elapsed <= Duration::from_millis(30_005));
    // Once eligible, the exact retry deadline wakes between ordinary interval ticks.
    let pause = control.failed(&anyhow::anyhow!("control deadline"));
    assert!((Duration::from_millis(500)..=Duration::from_millis(1500)).contains(&pause));
    let failed_at = tokio::time::Instant::now();
    assert!(tokio::time::timeout(
        pause / 2, settle.control_tick(&mut control, &mut control_interval)
    ).await.is_err());
    assert!(settle.control_tick(&mut control, &mut control_interval).await);
    let elapsed = tokio::time::Instant::now() - failed_at;
    assert!(elapsed >= pause && elapsed <= pause + Duration::from_millis(5));
}

#[tokio::test(start_paused = true)]
async fn resumed_driver_settle_does_not_fire_or_reset_an_existing_retry() {
    let settle = ResumeSettle::after(Duration::from_secs(30));
    let mut retry = st3::client::DriverRetry::default();
    let mut interval = tokio::time::interval(Duration::from_secs(10));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;
    // Seed two prior failures. Their retry falls due before the next delivery interval;
    // polling it during settle would make this timeout return early.
    retry.failed(&anyhow::anyhow!("control deadline"));
    retry.failed(&anyhow::anyhow!("control deadline"));
    assert!(tokio::time::timeout(
        Duration::from_secs(4), settle.control_tick(&mut retry, &mut interval)
    ).await.is_err());
    assert!(!settle.passed());
    tokio::time::advance(Duration::from_secs(26)).await;
    assert!(settle.control_tick(&mut retry, &mut interval).await);
    // Settle is eligibility, not operation success: the next failure uses base four.
    // This range cannot overlap a reset-to-base-one retry's 0.5–1.5 s range.
    let pause = retry.failed(&anyhow::anyhow!("control deadline"));
    assert!((Duration::from_secs(2)..=Duration::from_secs(6)).contains(&pause));
}

/// A deploy re-executes every driver while the daemon restarts. A resumed driver keeps the
/// harness events its hooks write meanwhile in the outbox and sends them once its settle
/// passes, not in the new daemon's first instant. A launched driver sends them at once.
#[tokio::test]
async fn a_resumed_driver_sends_its_outbox_only_after_its_settle() {
    use axum::{Json, Router, routing::post};
    use std::sync::{Arc, Mutex};
    let root = tempfile::tempdir().unwrap();
    st_drivers::harness_events::enable(root.path(), "runtime-a").unwrap();
    let seq =
        st_drivers::harness_state::claim(root.path(), "example/seat", "claude", "provider-a")
            .unwrap();
    let mut writer = st_drivers::harness_state::Writer::new(
        root.path(),
        "example/seat",
        "claude",
        Some("pty".into()),
    )
    .with_ownership("provider-a", seq);
    let active = || {
        st_drivers::harness_state::Observation::new(
            st_drivers::harness_state::Activity::Active,
            st_drivers::harness_state::BlockedOn::None,
            st_drivers::harness_state::InputBuffer::Unknown,
        )
    };
    writer.observe(active()).unwrap();
    let posted = Arc::new(Mutex::new(Vec::<tokio::time::Instant>::new()));
    let store = Arc::new(Store::open_memory("amber").unwrap());
    let app = Router::new().route(
        "/v1/harness-events",
        post({
            let posted = posted.clone();
            move |Json(request): Json<st3::harness_events::Publication>| {
                let posted = posted.clone();
                let store = store.clone();
                async move {
                    posted.lock().unwrap().push(tokio::time::Instant::now());
                    let record = store.append_claim(&request.claim).unwrap();
                    Json(json!({"api_version":"st3.v1","value":record}))
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::new(st3::client::Endpoint::Http(format!(
        "http://{}",
        listener.local_addr().unwrap()
    )));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let pause = Duration::from_millis(600);
    let settle = ResumeSettle::after(pause);
    let until = tokio::time::Instant::now() + pause;
    let mut observations = NativeObservations::start(root.path(), "runtime-a")
        .unwrap()
        .settle(settle);
    assert!(!observations.retry_due());
    // A hook writes while the driver settles; its wake does not end the wait.
    writer
        .observe(st_drivers::harness_state::Observation::new(
            st_drivers::harness_state::Activity::Idle,
            st_drivers::harness_state::BlockedOn::None,
            st_drivers::harness_state::InputBuffer::Unknown,
        ))
        .unwrap();
    assert!(
        tokio::time::timeout(pause / 2, observations.recv())
            .await
            .is_err(),
        "a wake ended the settle early"
    );
    assert!(posted.lock().unwrap().is_empty());
    observations.recv().await.unwrap();
    assert!(tokio::time::Instant::now() >= until);
    assert!(observations.retry_due());
    let mut ready = false;
    observations
        .drain_live(&client, "agent/example/seat", "claude", &mut ready)
        .await
        .unwrap();
    let first = posted.lock().unwrap().first().copied().unwrap();
    assert!(first >= until, "an event went before the settle passed");
    assert!(
        st_drivers::harness_events::pending(root.path(), 100)
            .unwrap()
            .is_empty()
    );

    writer.observe(active()).unwrap();
    let mut launched = NativeObservations::start(root.path(), "runtime-a").unwrap();
    assert!(launched.retry_due());
    tokio::time::timeout(Duration::from_millis(100), launched.recv())
        .await
        .expect("a launched driver drains at once")
        .unwrap();
    server.abort();
}
#[tokio::test]
async fn initial_snapshot_requires_the_providers_runtime_binding() {
    let root = tempfile::tempdir().unwrap();
    st_drivers::harness_events::enable(root.path(), "runtime-old").unwrap();
    let seq =
        st_drivers::harness_state::claim(root.path(), "example/seat", "claude", "provider-old")
            .unwrap();
    let observations = NativeObservations::start(root.path(), "runtime-old").unwrap();
    assert_eq!(
        observations.provider_incarnation.as_deref(),
        Some("provider-old")
    );
    assert!(observations.evidence_deadline.is_some());
    drop(observations);

    // Enabling the successor changes the runtime metadata, not the snapshot's producer.
    // Even a delayed write by the predecessor belongs to its original runtime.
    st_drivers::harness_events::enable(root.path(), "runtime-new").unwrap();
    st_drivers::harness_state::Writer::new(
        root.path(),
        "example/seat",
        "claude",
        Some("pty".into()),
    )
    .with_ownership("provider-old", seq)
    .observe(st_drivers::harness_state::Observation::new(
        st_drivers::harness_state::Activity::Active,
        st_drivers::harness_state::BlockedOn::None,
        st_drivers::harness_state::InputBuffer::Unknown,
    ))
    .unwrap();
    let observations = NativeObservations::start(root.path(), "runtime-new").unwrap();
    assert_eq!(observations.provider_incarnation, None);
    assert_eq!(observations.evidence_deadline, None);
    drop(observations);
    assert!(
        st_drivers::harness_events::read_snapshot(root.path(), "harness-state")
            .unwrap()
            .is_some()
    );
    assert_eq!(
        st_drivers::harness_events::pending(root.path(), 100).unwrap()[0].runtime_incarnation,
        "runtime-old"
    );

    st_drivers::harness_state::claim(root.path(), "example/seat", "claude", "provider-new")
        .unwrap();
    let observations = NativeObservations::start(root.path(), "runtime-new").unwrap();
    assert_eq!(
        observations.provider_incarnation.as_deref(),
        Some("provider-new")
    );
    assert!(observations.evidence_deadline.is_some());
}

#[tokio::test]
async fn admission_outbox_waits_for_current_runtime_and_retries_failed_publication() {
    use axum::{Json, Router, routing::post};
    use st_drivers::driver_diagnostic::{Driver, Publisher, Reason, Source, Stage, Support};
    use std::sync::{Arc, Mutex};

    let root = tempfile::tempdir().unwrap();
    st_drivers::harness_events::enable(root.path(), "old-runtime").unwrap();
    st_drivers::harness_state::claim(root.path(), "example/seat", "opencode", "old-provider")
        .unwrap();
    st_drivers::harness_events::enable(root.path(), "current-runtime").unwrap();
    let mut publisher = Publisher::new(
        root.path(),
        Driver::OpenCode,
        Some("99.42.7".into()),
        Support::Unsupported,
    );
    publisher.publish(
        Stage::VersionGate,
        Reason::AdmissionIdleEdge,
        Source::AdmissionProbe,
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let client = Client::new(st3::client::Endpoint::Http(format!("http://{address}")));
    let mut observations = NativeObservations::start(root.path(), "current-runtime").unwrap();
    let mut ready = false;
    // A predecessor's claim must neither publish the diagnostic nor require the daemon.
    observations
        .drain(&client, "agent/example/seat", "opencode", &mut ready)
        .await
        .unwrap();
    st_drivers::harness_state::claim(root.path(), "example/seat", "opencode", "current-provider")
        .unwrap();
    assert!(
        observations
            .drain(&client, "agent/example/seat", "opencode", &mut ready)
            .await
            .is_err()
    );
    assert_eq!(
        st_drivers::harness_events::pending(root.path(), 10)
            .unwrap()
            .len(),
        1
    );

    let captured = Arc::new(Mutex::new(Vec::<ClaimInput>::new()));
    let app = Router::new().route(
        "/v1/claims",
        post({
            let captured = captured.clone();
            let store = Arc::new(Store::open_memory("amber").unwrap());
            move |Json(input): Json<ClaimInput>| {
                captured.lock().unwrap().push(input.clone());
                let record = store.append_claim(&input).unwrap();
                async move { Json(json!({"api_version":"st3.v1", "value":record})) }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(address).await.unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    observations
        .drain(&client, "agent/example/seat", "opencode", &mut ready)
        .await
        .unwrap();
    assert!(
        st_drivers::harness_events::pending(root.path(), 10)
            .unwrap()
            .is_empty()
    );
    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].fields["incarnation_id"], "current-runtime");
    assert_eq!(captured[0].fields["status"], "degraded");
    assert!(
        captured[0].fields["reason"]
            .as_str()
            .unwrap()
            .contains("admissionIdleEdge")
    );
    assert!(!ready);
    server.abort();
}

#[tokio::test]
async fn reading_the_outbox_does_not_wake_an_idle_driver() {
    let root = tempfile::tempdir().unwrap();
    st_drivers::harness_events::enable(root.path(), "runtime").unwrap();
    let mut observations = NativeObservations::start(root.path(), "runtime").unwrap();
    observations.recv().await.unwrap();
    assert!(
        st_drivers::harness_events::pending(root.path(), 100)
            .unwrap()
            .is_empty()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(200), observations.recv())
            .await
            .is_err()
    );
    st_drivers::harness_events::write_channel_todo(root.path(), "runtime", &json!({
        "harness":"omp", "session_id":"native", "incarnation_id":"runtime",
        "observed_at":"2026-10-03T15:00:00Z", "source_op":"hydrate", "phases":[],
        "totals":{"pending":0,"in_progress":0,"completed":0,"blocked":0}, "truncated":false,
    })).unwrap();
    tokio::time::timeout(Duration::from_secs(1), observations.recv()).await.unwrap().unwrap();
}
