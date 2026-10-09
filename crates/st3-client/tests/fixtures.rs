#[macro_use]
#[path = "../../../scripts/ci-test-paths.rs"]
mod ci_test_paths;

use st3_client::*;
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(test_env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/st3/client-v0/fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn decode<T: serde::de::DeserializeOwned>(name: &str) -> T {
    serde_json::from_slice(&fixture(name)).unwrap_or_else(|error| panic!("decode {name}: {error}"))
}
#[test]
fn agent_lifecycle_future_value_keeps_the_roster_row() {
    let row = serde_json::json!({
        "kind": "agent", "id": "agent/example", "revision": "r1",
        "updated_at": "2026-10-04T08:00:00Z", "name": "Example",
        "state": "running", "reachability": "local", "runtime_ids": [],
    });
    for (value, expected) in [
        ("standing", AgentLifecycle::Standing),
        ("owner", AgentLifecycle::Owner),
        ("bounded", AgentLifecycle::Bounded),
        ("future-kind", AgentLifecycle::Unknown),
    ] {
        let mut declared = row.clone();
        declared["lifecycle"] = serde_json::json!(value);
        let page: Page = serde_json::from_value(serde_json::json!({
            "kind": "page", "collection": "agents", "filters": {},
            "items": [declared], "page": {"limit": 50, "has_more": false},
        })).unwrap();
        let [Resource::Agent(agent)] = page.items.as_slice() else {
            panic!("lifecycle must not discard the agent row: {:?}", page.items);
        };
        assert_eq!(agent.header.id, "agent/example");
        assert_eq!(agent.name, "Example");
        assert_eq!(agent.lifecycle, Some(expected));
    }
    let absent: Agent = serde_json::from_value(row).unwrap();
    assert_eq!(absent.lifecycle, None);
    assert!(serde_json::to_value(absent).unwrap().get("lifecycle").is_none());
}


#[test]
fn search_fixture_preserves_result_targets_and_incomplete_history() {
    let search: Envelope<ConversationSearch> = decode("conversation-search.json");
    assert_eq!(search.value.items[0].entry_id, "timeline-entry/note");
    assert_eq!(search.value.items[0].agent_id.as_deref(), Some("agent/scribe"));
    assert_eq!(search.value.incomplete_sources, ["session/older: truncation"]);
    assert!(!search.value.page.has_more);
}

#[test]
fn glass_groups_preserve_nested_tabs_and_empty_groups_on_round_trip() {
    let put: GlassPut = decode("glass-put.json");
    let GlassLayout::Split {
        split: GlassSplit::Right,
        children,
        ratio,
    } = &put.body.layout
    else {
        panic!("expected right split");
    };
    assert_eq!(*ratio, Some(0.3));
    let GlassLayout::Group { tabs } = &*children[0] else {
        panic!("expected tab group");
    };
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs[0].title.as_deref(), Some("Work"));
    assert_eq!(tabs[0].pane, "agent:agent/example/worker");
    let GlassLayout::Split {
        split: GlassSplit::Below,
        children,
        ratio: None,
    } = &*children[1]
    else {
        panic!("expected nested below split");
    };
    assert!(matches!(&*children[1], GlassLayout::Group { tabs } if tabs.is_empty()));
    assert_eq!(
        serde_json::to_value(&put).unwrap(),
        serde_json::from_slice::<serde_json::Value>(&fixture("glass-put.json")).unwrap()
    );
    let empty: GlassPut = decode("glass-put-empty.json");
    assert!(matches!(empty.body.layout, GlassLayout::Group { tabs } if tabs.is_empty()));
    assert!(
        serde_json::from_value::<GlassBody>(serde_json::json!({"name":"Old", "tabs":[]})).is_err()
    );
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
    match &timeline.value.items[1].body {
        TimelineBody::Message(message) => assert_eq!(message.tags, ["dictated"]),
        other => panic!("message body lost: {other:?}"),
    }
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
    assert_eq!(screen.value.cursor.style, TerminalCursorStyle::Block);
    assert!(screen.value.modes.bracketed_paste);
    for line in &screen.value.lines {
        let spelled = line
            .runs
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>();
        assert_eq!(spelled.trim_end_matches(' '), line.text);
    }
    assert_eq!(
        screen.value.lines[0].runs[0],
        TerminalRun {
            text: "$ ".into(),
            cells: Some(2),
            fg: Some(TerminalColor::Palette(2)),
            bold: true,
            ..TerminalRun::default()
        }
    );
    assert_eq!(
        screen.value.lines[1].runs[0].fg,
        Some(TerminalColor::Rgb("#5fd75f".into()))
    );
    let facts = screen.value.facts.as_ref().expect("the fixture carries facts");
    assert_eq!(facts.clients.as_ref().unwrap().read_only, 1);
    assert_eq!(facts.uptime_s, Some(3120));
    let relay = screen.value.relay.as_ref().expect("the fixture carries provenance");
    assert_eq!(relay.owner_host_id, "host/host-b");
    assert!(!relay.direct);
    assert_eq!(relay.capability_ttl_s, 60);
    let changed: Envelope<TerminalScreen> = decode("terminal-screen-changed.json");
    assert_eq!(changed.value.terminal_id, screen.value.terminal_id);
    assert_ne!(changed.value.revision, screen.value.revision);
    let ended: ErrorEnvelope = decode("terminal-stale-fence-error.json");
    assert_eq!(ended.code, ErrorCode::StaleFence);
    let pairing: Envelope<PairedSession> = decode("pairing.json");
    assert!(pairing.value.credential.len() >= 32);
}

#[test]
fn generated_resource_union_decodes_all_kinds() {
    let resources: Vec<Resource> = serde_json::from_slice(&fixture("resources.json")).unwrap();
    assert_eq!(resources.len(), 18);
    let lane = resources
        .iter()
        .find_map(|resource| match resource {
            Resource::Lane(lane) => Some(lane),
            _ => None,
        })
        .unwrap();
    assert_eq!(lane.entries[0].label, "42");
    assert_eq!(lane.recent[0].placement, Some(AgentQueuePlacement::Bottom));
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
    assert_eq!(agent.workspace.as_deref(), Some("/srv/example/release"));
    assert_eq!(agent.checkout.as_ref().unwrap().branch, "release");
    assert_eq!(agent.current_work_ids, ["step-run/release/build"]);
    assert_eq!(
        agent.next_work_id.as_deref(),
        Some("step-run/release/deploy")
    );
    assert_eq!(agent.queued_work_count, 1);
    let [subagent] = agent.subagents.as_slice() else {
        panic!("the release agent runs one subagent: {:?}", agent.subagents);
    };
    assert_eq!(
        subagent.description.as_deref(),
        Some("review the changelog")
    );
    assert_eq!(subagent.work_id.as_deref(), Some("step-run/release/build"));
    assert_eq!(subagent.lease_expires_at, "2026-09-20T11:17:10Z");
    assert!(!agent.extra.contains_key("subagents"));
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
    assert_eq!(device.person_id, "person/alex");
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
fn timeline_tool_result_preserves_timing_metadata_and_decodes_legacy_results() {
    let mut wire = serde_json::json!({
        "id": "timeline-entry/tool-timing",
        "sequence": 1,
        "revision": 1,
        "timestamp": "2026-10-05T12:00:00Z",
        "role": "tool",
        "type": "tool_result",
        "final": true,
        "body": {
            "call_id": "call/timing",
            "status": "success",
            "media_type": "text/plain",
            "content": "finished",
            "blocks": [{
                "id": "tool-timing/0",
                "kind": "tool_output",
                "source_type": "tool_result",
                "payload": {"body_ref": true},
                "metadata": {
                    "wallTimeMs": 1250.5,
                    "timeoutSeconds": 0,
                    "future": {"unit": "ticks", "value": 0.125}
                }
            }]
        }
    });
    let entry: TimelineEntry = serde_json::from_value(wire.clone()).unwrap();
    let TimelineBody::ToolResult(body) = entry.body else {
        panic!("tool result discriminator was not preserved");
    };
    assert_eq!(
        body.blocks[0].metadata.as_ref(),
        Some(&wire["body"]["blocks"][0]["metadata"])
    );
    assert_eq!(
        serde_json::to_value(&body).unwrap()["blocks"][0]["metadata"],
        wire["body"]["blocks"][0]["metadata"]
    );

    wire["body"]["blocks"][0].as_object_mut().unwrap().remove("metadata");
    let untimed: TimelineEntry = serde_json::from_value(wire.clone()).unwrap();
    assert!(matches!(
        untimed.body,
        TimelineBody::ToolResult(body) if body.blocks[0].metadata.is_none()
    ));
    wire["body"].as_object_mut().unwrap().remove("blocks");
    let legacy: TimelineEntry = serde_json::from_value(wire).unwrap();
    assert!(matches!(
        legacy.body,
        TimelineBody::ToolResult(body) if body.blocks.is_empty()
    ));
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
        TimelineBody::ToolResult(result) if result.status == TimelineToolStatus::Unknown
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

#[test]
fn creation_actions_round_trip_typed_parameters() {
    for name in ["agent-create.json", "terminal-create.json", "terminal-end.json"] {
        let action: ActionRequest = decode(name);
        let mut expected: serde_json::Value = decode(name);
        let fence: Fence = serde_json::from_value(expected["fence"].clone()).unwrap();
        expected["fence"] = serde_json::to_value(fence).unwrap();
        assert_eq!(serde_json::to_value(action).unwrap(), expected);
    }
    let action: ActionRequest = decode("agent-create.json");
    assert_eq!(action.action_type(), &ActionType::AgentCreate);
    let value: serde_json::Value = decode("agent-create.json");
    let parameters: AgentCreateParameters = serde_json::from_value(value["parameters"].clone()).unwrap();
    assert_eq!(parameters.name, "worker"); assert!(parameters.message.as_deref().unwrap().starts_with("--First"));
    let generated = ActionRequest::agent_create("action/agent-create", "fixture-agent-create-0001",
        serde_json::from_value(value["fence"].clone()).unwrap(), parameters).unwrap();
    assert_eq!(serde_json::to_value(generated).unwrap(), serde_json::to_value(action).unwrap());
}

#[test]
fn a_kind_this_client_does_not_know_reads_as_unknown_and_the_page_still_reads() {
    // A newer member sends a kind this build has never heard of, beside one it knows.
    let page: Page = serde_json::from_value(serde_json::json!({
        "kind": "page", "collection": "resources", "filters": {},
        "items": [
            {"kind": "example-arrangement", "id": "example-arrangement/person/avery/1",
             "revision": "r1", "updated_at": "2026-10-04T08:00:00Z",
             "name": "Pinned", "folders": {"inbox": {"name": "Inbox"}}},
            {"kind": "device", "id": "device/0011", "revision": "r2",
             "updated_at": "2026-10-04T08:00:00Z", "person_id": "person/avery",
             "name": "Pocket", "session_actor": "person/avery/session/0011", "state": "active",
             "scopes": ["read.projections"], "expires_at": "2026-11-04T08:00:00Z"}
        ],
        "page": {"limit": 50, "has_more": false}
    }))
    .unwrap();
    let Resource::Unknown(unknown) = &page.items[0] else {
        panic!("an unknown kind is kept as unknown: {:?}", page.items[0])
    };
    assert_eq!(unknown.kind, "example-arrangement");
    assert_eq!(page.items[0].header().id, "example-arrangement/person/avery/1");
    assert_eq!(unknown.fields["name"], "Pinned");
    assert!(matches!(page.items[1], Resource::Device(_)));
    // It goes back out as it came.
    let again = serde_json::to_value(&page.items[0]).unwrap();
    assert_eq!(again["kind"], "example-arrangement");
    assert_eq!(again["folders"]["inbox"]["name"], "Inbox");
    assert_eq!(again["revision"], "r1");
    // A known kind that does not match its model is still an error, never unknown.
    assert!(
        serde_json::from_value::<Resource>(serde_json::json!({
            "kind": "device", "id": "device/0011", "revision": "r2",
            "updated_at": "2026-10-04T08:00:00Z"
        }))
        .is_err()
    );
}

#[test]
fn relay_omission_fixture_round_trips_without_inventing_nonnullable_fields() {
    let bytes = fixture("timeline-relay-omission.json");
    let raw: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let typed: Envelope<TimelinePage> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(serde_json::to_value(&typed).unwrap(), raw);
    let changes = serde_json::json!({"kind":"conversation-changes", "session_id":raw["value"]["session_id"],
        "items":raw["value"]["items"], "next_cursor":"conversation-cursor/relay/next"});
    let typed: ConversationChanges = serde_json::from_value(changes.clone()).unwrap();
    assert_eq!(serde_json::to_value(typed).unwrap(), changes);
}
