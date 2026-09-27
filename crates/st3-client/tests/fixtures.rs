use st3_client::*;
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/st3/client-v0/fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn decode<T: serde::de::DeserializeOwned>(name: &str) -> T {
    serde_json::from_slice(&fixture(name)).unwrap_or_else(|error| panic!("decode {name}: {error}"))
}

#[test]
fn generated_models_decode_every_stream_fixture() {
    assert!(
        std::mem::size_of::<TimelineBody>() <= 128,
        "the discriminated timeline body must keep large variants boxed"
    );
    let capabilities: Envelope<Capabilities> = decode("capabilities.json");
    assert_eq!(capabilities.value.limits.max_page_items, 200);
    let events: Envelope<EventPage> = decode("events.json");
    assert_eq!(events.value.items.len(), 2);
    let timeline: Envelope<TimelinePage> = decode("timeline.json");
    assert_eq!(timeline.value.items.len(), 10);
    assert!(matches!(
        timeline.value.items[0].body,
        TimelineBody::Status(_)
    ));
    assert!(matches!(
        timeline.value.items[1].body,
        TimelineBody::Message(_)
    ));
    assert!(matches!(
        timeline.value.items[2].body,
        TimelineBody::Content(_)
    ));
    assert!(matches!(
        timeline.value.items[4].body,
        TimelineBody::ToolCall(_)
    ));
    assert!(matches!(
        timeline.value.items[5].body,
        TimelineBody::ToolResult(_)
    ));
    let TimelineBody::Usage(usage) = &timeline.value.items[6].body else {
        panic!("usage discriminator was not preserved");
    };
    assert_eq!(usage.total_tokens, Some(500));
    assert_eq!(usage.attribution.agent_id, "agent/release-agent");
    assert!(matches!(
        timeline.value.items[7].body,
        TimelineBody::Redaction(_)
    ));
    assert!(matches!(
        timeline.value.items[8].body,
        TimelineBody::Truncation(_)
    ));
    assert!(matches!(
        timeline.value.items[9].body,
        TimelineBody::Error(_)
    ));
    let screen: Envelope<TerminalScreen> = decode("terminal-screen.json");
    assert_eq!(screen.value.lines.len(), 3);
    let frames: Envelope<TerminalFramePage> = decode("terminal-frames.json");
    assert_eq!(frames.value.frames.len(), 2);
    let pairing: Envelope<PairedSession> = decode("pairing.json");
    assert!(pairing.value.credential.len() >= 32);
}

#[test]
fn generated_resource_union_decodes_all_kinds() {
    let resources: Vec<Resource> = serde_json::from_slice(&fixture("resources.json")).unwrap();
    assert_eq!(resources.len(), 15);
    assert!(
        resources
            .iter()
            .all(|resource| resource.header().id.contains('/'))
    );
    let launch = resources
        .iter()
        .find_map(|resource| match resource {
            Resource::Launch(launch) => Some(launch),
            _ => None,
        })
        .unwrap();
    let visualization = launch.visualization.as_ref().unwrap();
    assert_eq!(visualization.nodes[0].goals, ["Build artifacts"]);
    assert_eq!(
        visualization.decisions[0].decision_type,
        DecisionType::SingleChoice
    );
    assert_eq!(visualization.diffs[0].changes.len(), 1);
    assert_eq!(visualization.swimlanes[0].nodes, ["step/build"]);
    let work = resources
        .iter()
        .find_map(|resource| match resource {
            Resource::Work(work) => Some(work),
            _ => None,
        })
        .unwrap();
    assert_eq!(work.blocked_reason, None);
    assert!(work.blockers.is_empty());
    let agent = resources
        .iter()
        .find_map(|resource| match resource {
            Resource::Agent(agent) => Some(agent),
            _ => None,
        })
        .unwrap();
    assert_eq!(agent.current_work_ids, ["step-run/release/build"]);
    assert_eq!(
        agent.next_work_id.as_deref(),
        Some("step-run/release/deploy")
    );
    assert_eq!(agent.queued_work_count, 1);
    let machine = resources
        .iter()
        .find_map(|resource| match resource {
            Resource::Machine(machine) => Some(machine),
            _ => None,
        })
        .unwrap();
    assert_eq!(machine.host_id, "host/host-a");
    assert_eq!(machine.capacity.state, "unknown");
    assert_eq!(machine.occupancy.running_runtimes, 1);
    assert!(machine.projects.is_empty());
    let device = resources
        .iter()
        .find_map(|resource| match resource {
            Resource::Device(device) => Some(device),
            _ => None,
        })
        .unwrap();
    assert_eq!(device.person_id, "person/nathan");
    assert_eq!(device.state, "active");
}

#[test]
fn session_usage_context_decodes_through_flattened_resource() {
    let resource: Resource = serde_json::from_value(serde_json::json!({
        "id": "session/compatibility-test",
        "kind": "session",
        "revision": "s1",
        "updated_at": "2026-09-23T12:00:00Z",
        "owner_id": "agent/test",
        "state": "running",
        "started_at": "2026-09-23T11:00:00Z",
        "ended_at": null,
        "timeline_cursor": "timeline-cursor/compatibility-test/latest",
        "usage": {
            "total_tokens": 1200,
            "input_tokens": 900,
            "output_tokens": 300,
            "cached_tokens": 100,
            "cost": null,
            "currency": null,
            "incarnation_count": 2,
            "aggregation": "response-deltas",
            "context": {
                "used_tokens": 800,
                "window_tokens": 200000,
                "used_percent": 0.4,
                "model": "future-model",
                "observed_at_unix_ms": 1_790_163_492_730_u64,
                "new_harness_field": true
            }
        },
        "new_session_field": {"preserved": true}
    }))
    .expect("session usage emitted by a newer harness must decode");

    let Resource::Session(session) = resource else {
        panic!("resource discriminator was not preserved");
    };
    assert_eq!(
        session
            .usage
            .and_then(|usage| usage.context)
            .map(|context| context.observed_at_unix_ms),
        Some(1_790_163_492_730)
    );
    assert_eq!(session.extra["new_session_field"]["preserved"], true);
}

#[test]
fn timeline_models_tolerate_future_discriminators() {
    fn entry(entry_type: &str, body: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "id": format!("timeline-entry/{entry_type}"),
            "sequence": 1,
            "revision": 1,
            "timestamp": "2026-09-23T12:00:00Z",
            "role": "future_role",
            "type": entry_type,
            "final": true,
            "body": body
        })
    }

    let unknown: TimelineEntry = serde_json::from_value(entry(
        "future_event",
        serde_json::json!({"future": "payload"}),
    ))
    .expect("a newer harness timeline type must not break an older client");
    assert_eq!(unknown.role, TimelineRole::Unknown);
    assert!(matches!(
        &unknown.body,
        TimelineBody::Unknown { entry_type, body }
            if entry_type == "future_event" && body["future"] == "payload"
    ));
    assert_eq!(unknown.body.entry_type(), TimelineType::Unknown);
    assert_eq!(
        serde_json::to_value(&unknown).unwrap()["type"],
        "future_event",
        "JSON output must preserve a future timeline discriminator"
    );

    let status: TimelineEntry = serde_json::from_value(entry(
        "status",
        serde_json::json!({"status": "future_status", "detail": null}),
    ))
    .expect("a newer harness status must not break an older client");
    assert!(matches!(
        status.body,
        TimelineBody::Status(TimelineStatusBody {
            status: TimelineStatus::Unknown,
            ..
        })
    ));

    let tool: TimelineEntry = serde_json::from_value(entry(
        "tool_result",
        serde_json::json!({
            "call_id": "call/future",
            "status": "future_status",
            "media_type": "application/json",
            "content": null
        }),
    ))
    .expect("a newer tool status must not break an older client");
    assert!(matches!(
        tool.body,
        TimelineBody::ToolResult(TimelineToolResultBody {
            status: TimelineToolStatus::Unknown,
            ..
        })
    ));

    let usage: TimelineEntry = serde_json::from_value(entry(
        "usage",
        serde_json::json!({
            "semantics": "future_semantics",
            "driver": "future-driver",
            "attribution": {
                "agent_id": "agent/test",
                "mission_run_id": null,
                "generation_id": null,
                "step_id": null
            }
        }),
    ))
    .expect("newer usage semantics must not break an older client");
    assert!(matches!(
        usage.body,
        TimelineBody::Usage(body) if body.semantics == TimelineUsageSemantics::Unknown
    ));
}

#[test]
fn generated_action_union_and_machine_manifest_stay_in_lockstep() {
    let action: ActionRequest = decode("action.json");
    let operations: serde_json::Value = serde_json::from_str(include_str!(
        "../../../docs/st3/client-v0/schemas/operations.json"
    ))
    .unwrap();
    assert_eq!(action.action_type(), &ActionType::WorkComplete);
    assert_eq!(
        ACTION_NAMES.len(),
        operations["actions"].as_object().unwrap().len()
    );
    assert_eq!(
        READ_OPERATIONS.len(),
        operations["reads"].as_array().unwrap().len()
    );
    assert_eq!(CONTRACT_SHA256.len(), 64);
}

#[test]
fn crate_exposes_both_transport_constructors() {
    let _ = Client::unix(Path::new("/tmp/st3.sock"));
    let _ = Client::fabric_loopback("http://127.0.0.1:8787", "credential");
}

#[test]
fn authority_error_codes_are_typed_and_round_trip() {
    for (raw, expected) in [
        ("runtime-not-local", ErrorCode::RuntimeNotLocal),
        (
            "runtime-authority-indeterminate",
            ErrorCode::RuntimeAuthorityIndeterminate,
        ),
    ] {
        let decoded: ErrorCode = serde_json::from_str(&format!("\"{raw}\"")).unwrap();
        assert_eq!(decoded, expected);
        assert_eq!(
            serde_json::to_string(&decoded).unwrap(),
            format!("\"{raw}\"")
        );
    }
}
