use super::*;

/// The stop flag is process-global, so every test that exercises a reader of it —
/// [`initialize_control`] above all — holds this lock against the one test that flips
/// the flag: parallel readers would otherwise observe the raised flag and fail their
/// `no stop raised in tests` expectations.
fn stop_flag_tests() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
        std::sync::LazyLock::new(std::sync::Mutex::default);
    match LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[test]
fn a_stop_during_the_websocket_handshake_ends_startup_gracefully() {
    let _stop_exclusive = stop_flag_tests();
    let tmp = tempfile::tempdir().unwrap();
    let socket_path = tmp.path().join("control.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    let silent_server = std::thread::spawn(move || listener.accept().map(|(stream, _)| stream));
    let stream = UnixStream::connect(&socket_path).unwrap();
    let stopper = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(300));
        crate::provider_session::STOP.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let started = Instant::now();
    let result = initialize_control(stream);
    // Join before resetting: on an early failure return the stopper has not fired yet,
    // and resetting first would let it re-poison the global flag for every later test.
    stopper.join().unwrap();
    crate::provider_session::STOP.store(false, std::sync::atomic::Ordering::SeqCst);
    let _held_open = silent_server.join().unwrap().unwrap();
    assert!(
        result.unwrap().is_none(),
        "a stop while the server sits silent mid-handshake must return the graceful None"
    );
    assert!(
        started.elapsed() < STARTUP_TIMEOUT,
        "the stop must unblock the handshake well before the startup timeout"
    );
}
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;

#[cfg(target_os = "linux")]
fn linux_process_state(pid: i32) -> Option<char> {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()?
        .rsplit_once(") ")?
        .1
        .chars()
        .next()
}

fn process_can_retain_cleanup_resources(pid: i32) -> bool {
    #[cfg(target_os = "linux")]
    if linux_process_state(pid) == Some('Z') {
        return false;
    }
    crate::host_lock::process_alive(pid)
}

fn object_schema(required: &[&str], properties: &[(&str, Value)]) -> Value {
    json!({
        "type": "object",
        "required": required,
        "properties": properties
            .iter()
            .map(|(name, schema)| ((*name).to_string(), schema.clone()))
            .collect::<serde_json::Map<String, Value>>()
    })
}

fn reference(name: &str) -> Value {
    json!({ "$ref": format!("#/definitions/{name}") })
}

fn array_of(items: Value) -> Value {
    json!({ "type": "array", "items": items })
}

fn tagged_variant(name: &str, required: &[&str], properties: &[(&str, Value)]) -> Value {
    let mut all_required = vec!["type"];
    all_required.extend(required);
    let mut all_properties = vec![("type", json!({ "type": "string", "enum": [name] }))];
    all_properties.extend(properties.iter().cloned());
    object_schema(&all_required, &all_properties)
}

fn method_schema(methods: &[&str]) -> Value {
    json!({
        "oneOf": methods
            .iter()
            .map(|method| object_schema(
                &["method"],
                &[("method", json!({ "type": "string", "enum": [method] }))],
            ))
            .collect::<Vec<_>>()
    })
}

fn compatible_protocol_schemas() -> CodexProtocolSchemas {
    let mut definitions = serde_json::Map::new();
    definitions.insert(
        "ThreadActiveFlag".into(),
        json!({
            "type": "string",
            "enum": ["waitingOnApproval", "waitingOnUserInput"]
        }),
    );
    definitions.insert(
        "ThreadStatus".into(),
        json!({
            "oneOf": [
                tagged_variant("notLoaded", &[], &[]),
                tagged_variant("idle", &[], &[]),
                tagged_variant("systemError", &[], &[]),
                tagged_variant(
                    "active",
                    &["activeFlags"],
                    &[("activeFlags", array_of(reference("ThreadActiveFlag")))],
                )
            ]
        }),
    );
    definitions.insert(
        "ThreadItem".into(),
        json!({
            "oneOf": [
                tagged_variant("contextCompaction", &[], &[]),
                tagged_variant("enteredReviewMode", &[], &[]),
                tagged_variant("exitedReviewMode", &[], &[]),
                tagged_variant(
                    "userMessage",
                    &[],
                    &[("clientId", json!({ "type": ["string", "null"] }))],
                )
            ]
        }),
    );
    definitions.insert("TextElement".into(), object_schema(&[], &[]));
    definitions.insert(
        "UserInput".into(),
        json!({
            "oneOf": [tagged_variant(
                "text",
                &["text"],
                &[
                    ("text", json!({ "type": "string" })),
                    ("text_elements", array_of(reference("TextElement"))),
                ],
            )]
        }),
    );
    definitions.insert(
        "ClientInfo".into(),
        object_schema(
            &["name", "version"],
            &[
                ("name", json!({ "type": "string" })),
                ("title", json!({ "type": ["string", "null"] })),
                ("version", json!({ "type": "string" })),
            ],
        ),
    );
    definitions.insert(
        "InitializeCapabilities".into(),
        object_schema(&[], &[("experimentalApi", json!({ "type": "boolean" }))]),
    );
    definitions.insert(
        "InitializeParams".into(),
        object_schema(
            &["clientInfo"],
            &[
                ("clientInfo", reference("ClientInfo")),
                ("capabilities", reference("InitializeCapabilities")),
            ],
        ),
    );
    definitions.insert(
        "Thread".into(),
        object_schema(
            &["id", "status", "turns"],
            &[
                ("id", json!({ "type": "string" })),
                ("status", reference("ThreadStatus")),
                ("turns", array_of(reference("Turn"))),
            ],
        ),
    );
    definitions.insert(
        "Turn".into(),
        object_schema(
            &["id", "items", "status"],
            &[
                ("id", json!({ "type": "string" })),
                ("items", array_of(reference("ThreadItem"))),
                ("status", reference("TurnStatus")),
                (
                    "error",
                    json!({ "anyOf": [reference("TurnError"), { "type": "null" }] }),
                ),
            ],
        ),
    );
    definitions.insert(
        "TurnStatus".into(),
        json!({
            "type": "string",
            "enum": ["completed", "interrupted", "failed", "inProgress"]
        }),
    );
    definitions.insert(
        "TurnError".into(),
        object_schema(
            &["message"],
            &[
                ("message", json!({ "type": "string" })),
                (
                    "codexErrorInfo",
                    json!({ "anyOf": [reference("CodexErrorInfo"), { "type": "null" }] }),
                ),
            ],
        ),
    );
    definitions.insert(
        "CodexErrorInfo".into(),
        json!({
            "oneOf": [
                {
                    "type": "string",
                    "enum": [
                        "usageLimitExceeded",
                        "rateLimitExceeded",
                        "unauthorized",
                        "other"
                    ]
                },
                object_schema(
                    &["httpConnectionFailed"],
                    &[("httpConnectionFailed", object_schema(&[], &[]))],
                )
            ]
        }),
    );
    for notification in ["TurnStartedNotification", "TurnCompletedNotification"] {
        definitions.insert(
            notification.into(),
            object_schema(
                &["threadId", "turn"],
                &[
                    ("threadId", json!({ "type": "string" })),
                    ("turn", reference("Turn")),
                ],
            ),
        );
    }
    for notification in ["ItemStartedNotification", "ItemCompletedNotification"] {
        definitions.insert(
            notification.into(),
            object_schema(
                &["threadId", "turnId", "item"],
                &[
                    ("threadId", json!({ "type": "string" })),
                    ("turnId", json!({ "type": "string" })),
                    ("item", reference("ThreadItem")),
                ],
            ),
        );
    }
    definitions.insert(
        "ThreadStartedNotification".into(),
        object_schema(&["thread"], &[("thread", reference("Thread"))]),
    );
    definitions.insert(
        "ThreadStatusChangedNotification".into(),
        object_schema(
            &["threadId", "status"],
            &[
                ("threadId", json!({ "type": "string" })),
                ("status", reference("ThreadStatus")),
            ],
        ),
    );
    definitions.insert(
        "ThreadResumeParams".into(),
        object_schema(&["threadId"], &[("threadId", json!({ "type": "string" }))]),
    );
    definitions.insert(
        "ThreadResumeResponse".into(),
        object_schema(&["thread"], &[("thread", reference("Thread"))]),
    );
    definitions.insert(
        "TurnStartParams".into(),
        object_schema(
            &["threadId", "input"],
            &[
                ("threadId", json!({ "type": "string" })),
                ("input", array_of(reference("UserInput"))),
                ("clientUserMessageId", json!({ "type": ["string", "null"] })),
            ],
        ),
    );
    definitions.insert(
        "TurnSteerParams".into(),
        object_schema(
            &["threadId", "expectedTurnId", "input"],
            &[
                ("threadId", json!({ "type": "string" })),
                ("expectedTurnId", json!({ "type": "string" })),
                ("input", array_of(reference("UserInput"))),
                ("clientUserMessageId", json!({ "type": ["string", "null"] })),
            ],
        ),
    );
    definitions.insert(
        "TurnStartResponse".into(),
        object_schema(&["turn"], &[("turn", reference("Turn"))]),
    );
    definitions.insert(
        "TurnSteerResponse".into(),
        object_schema(&["turnId"], &[("turnId", json!({ "type": "string" }))]),
    );
    definitions.insert(
        "ThreadLoadedListResponse".into(),
        object_schema(
            &["data"],
            &[("data", array_of(json!({ "type": "string" })))],
        ),
    );
    definitions.insert(
        "HooksListParams".into(),
        object_schema(&[], &[("cwds", array_of(json!({ "type": "string" })))]),
    );
    definitions.insert(
        "HooksListResponse".into(),
        object_schema(
            &["data"],
            &[("data", array_of(reference("HooksListEntry")))],
        ),
    );
    definitions.insert(
        "HooksListEntry".into(),
        object_schema(
            &["hooks"],
            &[("hooks", array_of(reference("HookMetadata")))],
        ),
    );
    definitions.insert(
        "HookMetadata".into(),
        object_schema(
            &["currentHash", "isManaged", "key", "trustStatus"],
            &[
                ("currentHash", json!({ "type": "string" })),
                ("isManaged", json!({ "type": "boolean" })),
                ("key", json!({ "type": "string" })),
                ("trustStatus", reference("HookTrustStatus")),
            ],
        ),
    );
    definitions.insert(
        "HookTrustStatus".into(),
        json!({
            "type": "string",
            "enum": ["managed", "modified", "trusted", "untrusted"]
        }),
    );
    CodexProtocolSchemas {
        protocol: json!({ "definitions": definitions }),
        client_requests: method_schema(REQUIRED_CODEX_CLIENT_REQUESTS),
        client_notifications: method_schema(REQUIRED_CODEX_CLIENT_NOTIFICATIONS),
        server_requests: method_schema(&["currentTime/read"]),
        server_notifications: method_schema(REQUIRED_CODEX_SERVER_NOTIFICATIONS),
    }
}

fn write_fake_codex(
    root: &Path,
    name: &str,
    version: &str,
    schemas: &CodexProtocolSchemas,
) -> PathBuf {
    let fixture = root.join(format!("{name}-schemas"));
    fs::create_dir(&fixture).unwrap();
    for (filename, schema) in [
        (
            "codex_app_server_protocol.v2.schemas.json",
            &schemas.protocol,
        ),
        ("ClientRequest.json", &schemas.client_requests),
        ("ClientNotification.json", &schemas.client_notifications),
        ("ServerRequest.json", &schemas.server_requests),
        ("ServerNotification.json", &schemas.server_notifications),
    ] {
        fs::write(fixture.join(filename), serde_json::to_vec(schema).unwrap()).unwrap();
    }
    let path = root.join(name);
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '%s\\n' '{version}'; exit 0; fi\nout=\nwhile [ \"$#\" -gt 0 ]; do if [ \"$1\" = \"--out\" ]; then out=$2; break; fi; shift; done\n[ -n \"$out\" ] || exit 2\ncp '{fixture}/'*.json \"$out/\"\n",
            fixture = fixture.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn protocol_schema_gate_accepts_a_compatible_release_and_rejects_shape_drift() {
    let tmp = tempfile::tempdir().unwrap();
    let compatible = compatible_protocol_schemas();
    let patch = write_fake_codex(
        tmp.path(),
        "codex-compatible-patch",
        "codex-cli 0.150.0",
        &compatible,
    );
    ensure_supported_protocol(patch.to_str().unwrap()).unwrap();

    let mut incompatible = compatible_protocol_schemas();
    incompatible
        .protocol
        .pointer_mut("/definitions/ThreadActiveFlag/enum")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .push(Value::String("waitingOnFutureInput".into()));
    let incompatible = write_fake_codex(
        tmp.path(),
        "codex-incompatible-schema",
        "codex-cli 0.150.1",
        &incompatible,
    );
    let error = ensure_supported_protocol(incompatible.to_str().unwrap()).unwrap_err();
    assert!(format!("{error:#}").contains("ThreadActiveFlag changed"));
}

#[test]
fn protocol_schema_gate_accepts_additive_items_and_server_requests() {
    let mut schemas = compatible_protocol_schemas();
    schemas
        .server_requests
        .get_mut("oneOf")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .push(
            method_schema(&["future/request"])
                .get_mut("oneOf")
                .unwrap()
                .as_array_mut()
                .unwrap()
                .remove(0),
        );
    schemas
        .protocol
        .pointer_mut("/definitions/ThreadItem/oneOf")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .push(tagged_variant("futureItem", &[], &[]));

    verify_codex_protocol_schemas(&schemas).unwrap();
}

/// The classifier reads one word out of `Turn.error.codexErrorInfo` and depends on it being
/// distinct from the stable quota word. A release that dropped or merged it must refuse the
/// launch rather than let st2 report an exhausted allowance as a rejected credential. Codex
/// 0.146 does not expose the later `rateLimitExceeded` word, so that word is optional.
#[test]
fn protocol_schema_gate_requires_distinct_credential_and_stable_quota_error_words() {
    let mut schemas = compatible_protocol_schemas();
    let words = schemas
        .protocol
        .pointer_mut("/definitions/CodexErrorInfo/oneOf/0/enum")
        .unwrap()
        .as_array_mut()
        .unwrap();
    words.retain(|word| word.as_str() != Some("unauthorized"));
    let error = verify_codex_protocol_schemas(&schemas).unwrap_err();
    assert!(
        format!("{error:#}").contains("CodexErrorInfo has no 'unauthorized' word"),
        "{error:#}"
    );

    let mut no_later_rate_limit_word = compatible_protocol_schemas();
    no_later_rate_limit_word
        .protocol
        .pointer_mut("/definitions/CodexErrorInfo/oneOf/0/enum")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .retain(|word| word.as_str() != Some("rateLimitExceeded"));
    verify_codex_protocol_schemas(&no_later_rate_limit_word).unwrap();

    let mut merged = compatible_protocol_schemas();
    merged
        .protocol
        .pointer_mut("/definitions/CodexErrorInfo/oneOf/0/enum")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .retain(|word| word.as_str() != Some("usageLimitExceeded"));
    let error = verify_codex_protocol_schemas(&merged).unwrap_err();
    assert!(
        format!("{error:#}").contains("CodexErrorInfo has no 'usageLimitExceeded' word"),
        "{error:#}"
    );

    let mut untyped = compatible_protocol_schemas();
    untyped
        .protocol
        .pointer_mut("/definitions/Turn/properties")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("error");
    let error = verify_codex_protocol_schemas(&untyped).unwrap_err();
    assert!(
        format!("{error:#}").contains("Turn has no error property"),
        "{error:#}"
    );
}

#[test]
fn protocol_rejection_reaches_the_declared_supervisor_once() {
    let tmp = tempfile::tempdir().unwrap();
    let worker = tmp.path().join("agents/h/worker/agent.kdl");
    let supervisor = tmp.path().join("agents/h/cos/agent.kdl");
    fs::create_dir_all(worker.parent().unwrap()).unwrap();
    fs::create_dir_all(supervisor.parent().unwrap()).unwrap();
    fs::write(
        &worker,
        r#"agent "worker" {
  host "h"
  supervisor "h.cos"
  command "true"
}
"#,
    )
    .unwrap();
    fs::write(
        &supervisor,
        r#"agent "cos" {
  host "h"
  command "true"
}
"#,
    )
    .unwrap();
    let mut incompatible = compatible_protocol_schemas();
    incompatible
        .protocol
        .pointer_mut("/definitions/ThreadActiveFlag/enum")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .push(Value::String("waitingOnFutureInput".into()));
    let codex = write_fake_codex(
        tmp.path(),
        "codex-rejected",
        "codex-cli 0.150.1",
        &incompatible,
    );
    let argv = vec![codex.display().to_string()];

    for _ in 0..2 {
        let error = run_controlled(
            tmp.path(),
            "h.worker".into(),
            "h.worker".into(),
            argv.clone(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("ThreadActiveFlag changed"));
    }

    let inbox = message::list_inbox(&message::inbox_dir(supervisor.parent().unwrap())).unwrap();
    assert_eq!(inbox.len(), 1, "the rejection report was not idempotent");
    assert_eq!(inbox[0].from.as_deref(), Some("h.worker"));
    assert_eq!(
        inbox[0].subject.as_deref(),
        Some("Codex protocol rejected: h.worker")
    );
    assert!(inbox[0].body.contains("Native delivery did not start"));
    assert!(inbox[0].body.contains("ThreadActiveFlag changed"));
}

#[test]
fn unknown_thread_status_remains_a_hold_not_a_terminal_system_error() {
    let mut state = subscribed_state(CodexObservedState::Idle);
    state.observe_thread_status("futureStatus", None);
    assert_eq!(
        state.observed(),
        &CodexObservedState::Held {
            reason: CodexHoldReason::UnknownStatus,
            turn_id: None,
        }
    );
    state.observe_turn_completed("turn-future", CodexTurnOutcome::Indeterminate);
    assert_eq!(
        state.observed(),
        &CodexObservedState::Held {
            reason: CodexHoldReason::UnknownStatus,
            turn_id: None,
        }
    );
}

#[test]
fn tui_loaded_deadline_precedes_the_outer_binding_deadline() {
    assert!(TUI_LOADED_TIMEOUT < STARTUP_TIMEOUT);
}

/// An agent directory with a parent to stage into, and a producer over it carrying a fixed
/// incarnation so the record's provenance is assertable.
fn context_producer(root: &Path) -> (PathBuf, CodexContextProducer) {
    let agent_dir = root.join("agents/h/worker");
    fs::create_dir_all(&agent_dir).unwrap();
    let writer =
        harness_context::Writer::new(&agent_dir, "h.worker", harness_context::Harness::Codex)
            .unwrap()
            .with_session("codex-incarnation");
    (agent_dir, CodexContextProducer::new(writer))
}

fn context_record(agent_dir: &Path) -> Option<harness_context::Observed> {
    harness_context::read(&harness_context::harness_context_path(agent_dir))
}

fn token_usage_frame(last_total: i64, window: Value) -> Value {
    json!({
        "method": "thread/tokenUsage/updated",
        "params": {
            "threadId": "thread-main",
            "turnId": "turn-1",
            "tokenUsage": {
                "last": { "totalTokens": last_total },
                "total": { "totalTokens": last_total },
                "modelContextWindow": window
            }
        }
    })
}

fn compaction_item_frame(method: &str, turn_id: &str, item_id: &str) -> Value {
    json!({
        "method": method,
        "params": {
            "threadId": "thread-main",
            "turnId": turn_id,
            "item": { "id": item_id, "type": "contextCompaction" }
        }
    })
}

/// HC-R13's Codex fixture. The frames are a transposition, and the comment says which half came
/// from where: the SHAPE is codex-cli 0.151.0's own app-server schema dump
/// (`ThreadTokenUsageUpdatedNotification`, `AccountRateLimitsUpdatedNotification`), while the
/// NUMBERS are verbatim from a real rollout captured on 2026-08-29 from a 0.150.1 session
/// (`session_meta.payload.cli_version = "0.150.1"`) — its first and last `token_count` events
/// and the `rate_limits` snapshot riding them. Fields the capture elided are omitted rather
/// than invented; this producer reads three numbers and must not need the rest.
///
/// What must fail here when a codex bump moves something: the 12,000 baseline (the percent
/// changes), the numerator (`total` reads 100 and `last.inputTokens` without the baseline reads
/// 36 against this very capture, both asserted below), and the version literal itself, which is
/// the only thing tying this arithmetic to a build whose source was actually read.
#[test]
fn codex_context_recomputes_the_captured_reading_and_pins_its_verified_version() {
    assert_eq!(CODEX_CONTEXT_VERIFIED_VERSION, "0.151.0");
    assert_eq!(CODEX_BASELINE_TOKENS, 12_000);

    let frames = include_str!("../../tests/fixtures/codex_token_usage_inbound.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 3);

    let tmp = tempfile::tempdir().unwrap();
    let (agent_dir, mut producer) = context_producer(tmp.path());

    // The session's FIRST reading: 32,237 of 258,400 with the baseline normalized out is 8%,
    // and no rate-limit notification has arrived yet, so both windows are honestly absent.
    assert!(producer.observe(&frames[0], "thread-main").unwrap());
    let first = context_record(&agent_dir).unwrap();
    assert_eq!(first.used_tokens, Some(32_237));
    assert_eq!(first.window_tokens, Some(258_400));
    assert_eq!(first.used_percent, Some(8.0));
    assert_eq!(first.rate_limits, harness_context::RateLimits::default());

    // The account-scoped snapshot carries no occupancy, so it writes nothing on its own and is
    // held for the next reading (HC-T06).
    assert!(!producer.observe(&frames[1], "thread-main").unwrap());
    let mut unchanged = context_record(&agent_dir).unwrap();
    // `age_ms` is derived at read time, not stored, so it moves between two reads of one
    // record. Everything the record itself carries — including `observed_at_ms`, which is what
    // proves no write happened — must be identical.
    assert!(unchanged.age_ms >= first.age_ms);
    unchanged.age_ms = first.age_ms;
    assert_eq!(unchanged, first);

    assert!(producer.observe(&frames[2], "thread-main").unwrap());
    let observed = context_record(&agent_dir).unwrap();
    assert_eq!(observed.harness, harness_context::Harness::Codex);
    assert_eq!(observed.used_tokens, Some(92_283));
    assert_eq!(observed.window_tokens, Some(258_400));
    // 100 − Codex's displayed "67% context left" for this exact capture.
    assert_eq!(observed.used_percent, Some(33.0));
    assert_eq!(observed.session_total_tokens, Some(2_235_329));
    // The channel carries neither: `Thread` has `modelProvider` and no model identifier, and
    // Codex reports no session cost anywhere in the protocol.
    assert_eq!(observed.model, None);
    assert_eq!(observed.cost_usd, None);
    // Only the seven-day window was ever captured on this harness; the five-hour leg is not
    // inferred from a field name (see `observe_rate_limits`).
    assert_eq!(
        observed.rate_limits,
        harness_context::RateLimits {
            five_hour: None,
            seven_day: Some(44.0),
        }
    );
    assert_eq!(observed.compactions, 0);
    assert_eq!(observed.last_compaction_ms, None);

    // The trap, asserted rather than described: the cumulative session total is 2,235,329
    // against a 258,400-token window. A producer that used it as the numerator would publish a
    // saturated 100 for a window that is a third full.
    assert_eq!(codex_used_percent(Some(258_400), 2_235_329), Some(100.0));
    assert_ne!(
        codex_used_percent(Some(258_400), 2_235_329),
        observed.used_percent
    );
    // And the baseline-free percent over the same operands is 36 — close enough to look right.
    let baseline_free = (92_283.0_f64 / 258_400.0 * 100.0).round();
    assert_eq!(baseline_free, 36.0);
    assert_ne!(Some(baseline_free), observed.used_percent);

    // Mirroring is not the same function as rounding the used percentage: at an exact half
    // they disagree. Effective window 200, used 101 — Codex displays 50% left, so st2 publishes
    // 50; rounding `used/effective` would publish 51.
    assert_eq!(codex_used_percent(Some(12_200), 12_101), Some(50.0));
    assert_eq!((101.0_f64 / 200.0 * 100.0).round(), 51.0);
}

/// HC-R02/HC-R03: the operands are the harness's and are published as they arrive; only the
/// percent is withheld, and only where Codex's own normalization cannot run. A window at or
/// below the baseline is the sharp case — Codex itself returns "0% remaining" there, which
/// mirrored blindly would publish a fabricated 100% used.
#[test]
fn a_missing_or_unnormalizable_window_withholds_the_percent_but_not_the_operands() {
    for (window, expected_window) in [
        (Value::Null, None),
        (json!(12_000), Some(12_000)),
        (json!(0), None),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let (agent_dir, mut producer) = context_producer(tmp.path());
        assert!(
            producer
                .observe(&token_usage_frame(92_283, window.clone()), "thread-main")
                .unwrap()
        );
        let observed = context_record(&agent_dir).unwrap();
        assert_eq!(observed.used_tokens, Some(92_283), "window {window}");
        assert_eq!(observed.window_tokens, expected_window, "window {window}");
        assert_eq!(observed.used_percent, None, "window {window}");
    }

    // A window key that is absent rather than null reads the same way.
    let tmp = tempfile::tempdir().unwrap();
    let (agent_dir, mut producer) = context_producer(tmp.path());
    assert!(
        producer
            .observe(
                &json!({
                    "method": "thread/tokenUsage/updated",
                    "params": {
                        "threadId": "thread-main",
                        "turnId": "turn-1",
                        "tokenUsage": {
                            "last": { "totalTokens": 92_283 },
                            "total": { "totalTokens": 92_283 }
                        }
                    }
                }),
                "thread-main",
            )
            .unwrap()
    );
    let observed = context_record(&agent_dir).unwrap();
    assert_eq!(observed.window_tokens, None);
    assert_eq!(observed.used_percent, None);
}

/// Codex speaks once per model response — roughly 10-15 times a turn, and again on resume or
/// re-attach. The core's quantization is the ONLY thing deciding what lands (HC-R09): this
/// producer holds no reading of its own and imposes no cadence. The shape that catches a second
/// guard is the last frame here — a bucket crossing arriving immediately after two suppressed
/// readings, which any time floor in the producer would swallow.
#[test]
fn every_reading_reaches_the_core_guard_and_the_producer_imposes_no_cadence_of_its_own() {
    let tmp = tempfile::tempdir().unwrap();
    let (agent_dir, mut producer) = context_producer(tmp.path());
    let window = json!(258_400);

    assert!(
        producer
            .observe(&token_usage_frame(92_283, window.clone()), "thread-main")
            .unwrap()
    );
    assert_eq!(context_record(&agent_dir).unwrap().used_percent, Some(33.0));

    // Both still round to 33% used, so both sit in the written bucket and neither lands.
    for moved in [93_000, 94_000] {
        assert_eq!(codex_used_percent(Some(258_400), moved), Some(33.0));
        assert!(
            !producer
                .observe(&token_usage_frame(moved, window.clone()), "thread-main")
                .unwrap()
        );
        assert_eq!(
            context_record(&agent_dir).unwrap().used_tokens,
            Some(92_283)
        );
    }

    // The crossing lands at once, with no elapsed time behind it.
    assert!(
        producer
            .observe(&token_usage_frame(95_000, window.clone()), "thread-main")
            .unwrap()
    );
    let observed = context_record(&agent_dir).unwrap();
    assert_eq!(observed.used_percent, Some(34.0));
    assert_eq!(observed.used_tokens, Some(95_000));

    // A reading for another thread is not this seat's.
    assert!(
        !producer
            .observe(&token_usage_frame(200_000, window), "thread-other")
            .unwrap()
    );
    assert_eq!(
        context_record(&agent_dir).unwrap().used_tokens,
        Some(95_000)
    );
}

/// HC-R12: one compaction is one count, however many of its spellings arrive. Codex publishes
/// the live edge as an `item/started` AND an `item/completed` over the same
/// `ContextCompactionThreadItem` id, and the protocol still carries a deprecated
/// `thread/compacted` notification for the same event that names only the turn.
#[test]
fn one_compaction_is_counted_once_across_every_spelling_of_its_edge() {
    let tmp = tempfile::tempdir().unwrap();
    let (agent_dir, mut producer) = context_producer(tmp.path());

    assert!(
        producer
            .observe(
                &compaction_item_frame("item/started", "turn-1", "item-a"),
                "thread-main"
            )
            .unwrap()
    );
    let first = context_record(&agent_dir).unwrap();
    assert_eq!(first.compactions, 1);
    assert_eq!(
        first.last_compaction_trigger,
        Some(harness_context::CompactionTrigger::Unknown),
        "the item carries an id and a type and no reason at all"
    );
    assert!(first.last_compaction_ms.is_some());

    // The same compaction's closing edge, and the deprecated notification for the same event.
    assert!(
        !producer
            .observe(
                &compaction_item_frame("item/completed", "turn-1", "item-a"),
                "thread-main"
            )
            .unwrap()
    );
    assert!(
        !producer
            .observe(
                &json!({
                    "method": "thread/compacted",
                    "params": { "threadId": "thread-main", "turnId": "turn-1" }
                }),
                "thread-main",
            )
            .unwrap()
    );
    assert_eq!(context_record(&agent_dir).unwrap().compactions, 1);

    // A genuinely second compaction inside the same turn is a second count.
    assert!(
        producer
            .observe(
                &compaction_item_frame("item/started", "turn-1", "item-b"),
                "thread-main"
            )
            .unwrap()
    );
    assert_eq!(context_record(&agent_dir).unwrap().compactions, 2);

    // Interleaved lifecycles: two starts before either completion still count exactly two, so
    // the dedupe cannot be a single last-key memory.
    for (method, item) in [
        ("item/started", "item-c"),
        ("item/started", "item-d"),
        ("item/completed", "item-c"),
        ("item/completed", "item-d"),
    ] {
        producer
            .observe(
                &compaction_item_frame(method, "turn-2", item),
                "thread-main",
            )
            .unwrap();
    }
    assert_eq!(context_record(&agent_dir).unwrap().compactions, 4);

    // The deprecated notification arriving FIRST also claims the compaction, so the item that
    // follows it does not count a second time.
    assert!(
        producer
            .observe(
                &json!({
                    "method": "thread/compacted",
                    "params": { "threadId": "thread-main", "turnId": "turn-3" }
                }),
                "thread-main",
            )
            .unwrap()
    );
    assert!(
        !producer
            .observe(
                &compaction_item_frame("item/started", "turn-3", "item-e"),
                "thread-main"
            )
            .unwrap()
    );
    assert_eq!(context_record(&agent_dir).unwrap().compactions, 5);

    // Another thread's compaction is not this seat's, and a non-compaction item is not an edge.
    assert!(
        !producer
            .observe(
                &compaction_item_frame("item/started", "turn-9", "item-z"),
                "thread-other"
            )
            .unwrap()
    );
    assert!(
        !producer
            .observe(
                &json!({
                    "method": "item/started",
                    "params": {
                        "threadId": "thread-main",
                        "turnId": "turn-4",
                        "item": { "id": "item-y", "type": "agentMessage" }
                    }
                }),
                "thread-main",
            )
            .unwrap()
    );
    assert_eq!(context_record(&agent_dir).unwrap().compactions, 5);
}

#[test]
fn rollout_fallback_counts_only_new_context_compaction_completions() {
    let tmp = tempfile::tempdir().unwrap();
    let (agent_dir, mut producer) = context_producer(tmp.path());
    let baseline = vec![json!({
        "ordinal": 10,
        "type": "event_msg",
        "payload": {
            "type": "task_complete",
            "thread_id": "thread-main",
            "turn_id": "turn-before"
        }
    })];
    assert!(
        !producer
            .observe_transcript(&baseline, "thread-main")
            .unwrap()
    );
    assert!(context_record(&agent_dir).is_none());

    let completed = vec![
        baseline[0].clone(),
        json!({
            "ordinal": 11,
            "type": "event_msg",
            "payload": {
                "type": "item_completed",
                "thread_id": "thread-main",
                "turn_id": "turn-compact",
                "item": { "id": "compact-1", "type": "ContextCompaction" }
            }
        }),
    ];
    assert!(
        producer
            .observe_transcript(&completed, "thread-main")
            .unwrap()
    );
    let observed = context_record(&agent_dir).unwrap();
    assert_eq!(observed.compactions, 1);
    assert_eq!(
        observed.last_compaction_trigger,
        Some(harness_context::CompactionTrigger::Unknown)
    );
    assert!(
        !producer
            .observe_transcript(&completed, "thread-main")
            .unwrap()
    );

    let foreign = vec![json!({
        "ordinal": 12,
        "type": "event_msg",
        "payload": {
            "type": "item_completed",
            "thread_id": "thread-other",
            "turn_id": "turn-foreign",
            "item": { "id": "compact-2", "type": "ContextCompaction" }
        }
    })];
    assert!(
        !producer
            .observe_transcript(&foreign, "thread-main")
            .unwrap()
    );
    assert_eq!(context_record(&agent_dir).unwrap().compactions, 1);

    let fresh_tmp = tempfile::tempdir().unwrap();
    let (fresh_dir, mut fresh) = context_producer(fresh_tmp.path());
    let first_live_snapshot = vec![json!({
        "ordinal": 20,
        "type": "event_msg",
        "payload": {
            "type": "item_completed",
            "thread_id": "thread-main",
            "turn_id": "turn-live",
            "completed_at_ms": u64::MAX,
            "item": { "id": "compact-live", "type": "ContextCompaction" }
        }
    })];
    assert!(
        fresh
            .observe_transcript(&first_live_snapshot, "thread-main")
            .unwrap()
    );
    assert_eq!(context_record(&fresh_dir).unwrap().compactions, 1);
}

/// The producer runs beside a live delivery loop and sees every frame that loop sees. Replaying
/// the captured #263 session — 23 real inbound frames, none of them a token count — must leave
/// no record at all: absence here is "never observed", and a producer that manufactured a
/// reading from a turn boundary would break exactly the HC-R03 rule the record exists for.
#[test]
fn captured_delivery_frames_carrying_no_token_count_publish_no_record() {
    let frames = include_str!("../../tests/fixtures/codex_usage_limit_inbound.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 23);

    let tmp = tempfile::tempdir().unwrap();
    let (agent_dir, mut producer) = context_producer(tmp.path());
    for frame in &frames {
        assert!(
            !producer.observe(frame, "thread-main").unwrap(),
            "no captured delivery frame carries a context reading: {frame}"
        );
    }
    assert!(context_record(&agent_dir).is_none());
}

fn delivery_config(root: &Path) -> CodexDeliveryConfig {
    let agent_dir = root.join("agents/h/worker");
    CodexDeliveryConfig {
        catalog_root: root.to_path_buf(),
        inbox: message::inbox_dir(&agent_dir),
        agent_dir,
        identity: "h.worker".into(),
        this_host: "h".into(),
        supervisor: None,
        producer_version: Some("codex-cli 0.153.0".into()),
        model: None,
    }
}

#[test]
fn rollout_turn_context_selects_the_effective_model_for_the_exact_turn() {
    let frames = vec![
        json!({"type":"turn_context","payload":{"turn_id":"turn-a","model":"gpt-example-a"}}),
        json!({"type":"turn_context","payload":{"turn_id":"turn-b","model":"gpt-example-b"}}),
        json!({"type":"event_msg","payload":{"type":"token_count","turn_id":"turn-b"}}),
    ];
    assert_eq!(
        model_from_codex_frames(&frames, "turn-a").as_deref(),
        Some("gpt-example-a")
    );
    assert_eq!(
        model_from_codex_frames(&frames, "turn-b").as_deref(),
        Some("gpt-example-b")
    );
    assert_eq!(model_from_codex_frames(&frames, "turn-c"), None);
}

#[test]
fn configured_codex_model_is_a_fallback_when_the_rollout_is_unavailable() {
    assert_eq!(
        declared_codex_model(&["--model".into(), "gpt-example".into()]).as_deref(),
        Some("gpt-example")
    );
    assert_eq!(
        declared_codex_model(&["--model=first".into(), "-m".into(), "second".into()]).as_deref(),
        Some("second")
    );
    assert_eq!(
        declared_codex_model(&["--".into(), "--model".into(), "prompt".into()]),
        None
    );
}

#[test]
fn replayed_codex_token_usage_is_context_only_until_its_turn_is_active() {
    let usage = json!({"method":"thread/tokenUsage/updated","params":{"turnId":"turn-old"}});
    assert!(!should_track_timeline_usage(&usage, None));
    assert!(!should_track_timeline_usage(&usage, Some("turn-new")));
    assert!(should_track_timeline_usage(&usage, Some("turn-old")));
    assert!(should_track_timeline_usage(
        &json!({"method":"item/completed"}),
        None
    ));
}

fn subscribed_state(observed: CodexObservedState) -> CodexControlState {
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let mut state = CodexControlState::new(&runtime, "thread-main".into());
    state.subscribed = true;
    state.observed = observed;
    state
}

fn inbox_delivery(root: &Path, config: CodexDeliveryConfig) -> CodexInboxDelivery {
    CodexInboxDelivery::new(
        config,
        root.join("state").join(delivery_ledger::LEDGER_FILE),
        CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap(),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap()
    .without_snapshot_requirement()
}

/// Read the ledger back through its own loader and the real correlation derivation: a test
/// that read the bytes directly would not notice a record the pump itself would refuse.
fn ledger_entry(root: &Path, filename: &str) -> Option<delivery_ledger::Entry> {
    delivery_ledger::Ledger::open(
        &root.join("state").join(delivery_ledger::LEDGER_FILE),
        delivery_ledger::Harness::Codex.profile(),
        "h.worker",
        "h.worker",
        |thread, file| stable_client_user_message_id("h.worker", thread, file),
    )
    .entry(filename)
    .cloned()
}

fn acknowledge_tui_thread_loaded(events: &Receiver<ControlEvent>) {
    let ControlEvent::TuiThreadLoaded(acknowledge) =
        events.recv_timeout(Duration::from_secs(10)).unwrap()
    else {
        panic!("control did not report the TUI-loaded gate");
    };
    acknowledge.send(()).unwrap();
}

#[test]
fn delivery_request_uses_typed_start_and_exact_turn_steer() {
    let start = codex_delivery_request(
        2,
        "thread-main",
        "st2:client",
        "notice",
        &CodexDeliveryMethod::Start,
    );
    assert_eq!(start["method"], "turn/start");
    assert_eq!(start["params"]["threadId"], "thread-main");
    assert_eq!(start["params"]["clientUserMessageId"], "st2:client");
    assert_eq!(start["params"]["input"][0]["type"], "text");
    assert_eq!(start["params"]["input"][0]["text"], "notice");
    assert!(start["params"].get("expectedTurnId").is_none());

    let steer = codex_delivery_request(
        3,
        "thread-main",
        "st2:client",
        "notice",
        &CodexDeliveryMethod::Steer {
            turn_id: "turn-current".into(),
        },
    );
    assert_eq!(steer["method"], "turn/steer");
    assert_eq!(steer["params"]["expectedTurnId"], "turn-current");
    assert!(steer["params"].get("model").is_none());
    assert!(steer["params"].get("approvalPolicy").is_none());
}

/// Behavioral oracle for the #268 §B projection: a projection that withheld every row — or
/// that reported the two misclassified rows as indeterminate — fails here, because each
/// emitting row is asserted positively.
#[test]
fn harness_projection_is_faithful_and_withholds_only_unprovable_rows() {
    use crate::harness_state::{Activity, Ask, BlockedOn, InputBuffer};
    let held = |reason| CodexObservedState::Held {
        reason,
        turn_id: None,
    };

    // Rows with no provable observation are withheld — and no absence may derive idle.
    for state in [
        CodexObservedState::AwaitingStatus,
        held(CodexHoldReason::NotLoaded),
        held(CodexHoldReason::SystemError),
    ] {
        assert_eq!(state.harness_observation(), None, "{state:?}");
    }

    // Codex positively reported work: active, even where st2 cannot name a steerable turn
    // (the two rows a naive steerability decomposition reported as unknown) or where the
    // delivery gate holds.
    for state in [
        CodexObservedState::Active {
            turn_id: "turn-current".into(),
        },
        held(CodexHoldReason::ActiveWithoutTurn),
        held(CodexHoldReason::ConflictingTurn),
        held(CodexHoldReason::Compaction),
        // Review's edges are model-emitted items inside a running turn: plain activity,
        // no human, no ask — the delivery hold is a separate axis.
        held(CodexHoldReason::Review),
    ] {
        let observation = state
            .harness_observation()
            .unwrap_or_else(|| panic!("{state:?} must emit"));
        assert_eq!(observation.state, Activity::Active, "{state:?}");
        assert_eq!(observation.blocked_on, BlockedOn::None, "{state:?}");
        assert_eq!(observation.input_buffer, InputBuffer::Unknown, "{state:?}");
    }

    // The holds a human resolves set the blocked axis instead of disappearing into active,
    // and each names its machine-readable ask kind so consumers never branch on `reason`.
    for (reason, ask) in [
        (CodexHoldReason::WaitingOnApproval, Ask::Permission),
        (CodexHoldReason::WaitingOnUserInput, Ask::Question),
    ] {
        let observation = held(reason)
            .harness_observation()
            .unwrap_or_else(|| panic!("{reason:?} must emit"));
        assert_eq!(observation.state, Activity::Active, "{reason:?}");
        assert_eq!(observation.blocked_on, BlockedOn::Human, "{reason:?}");
        assert_eq!(observation.ask, ask, "{reason:?}");
    }

    let idle = CodexObservedState::Idle.harness_observation().unwrap();
    assert_eq!(idle.state, Activity::Idle);
    assert_eq!(idle.blocked_on, BlockedOn::None);

    let ended = CodexObservedState::TerminalError {
        reason: CodexTerminalError::SystemError,
    }
    .harness_observation()
    .unwrap();
    assert_eq!(ended.state, Activity::Ended);
    assert_eq!(ended.reason.as_deref(), Some("systemError"));
}

#[test]
#[cfg(unix)]
fn a_failed_transition_write_is_retried_before_any_heartbeat() {
    use crate::harness_state::{self, Activity};
    use std::os::unix::fs::PermissionsExt as _;
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    let agent_dir = config.agent_dir.clone();
    let record_path = harness_state::harness_state_path(&agent_dir);
    let mut delivery = inbox_delivery(tmp.path(), config);

    delivery.observe_harness(&CodexObservedState::Active {
        turn_id: "turn-current".into(),
    });
    assert_eq!(
        harness_state::read(&record_path, None).unwrap().state,
        Activity::Active
    );

    // The transition to idle fails to land: the agent dir is briefly unwritable.
    let live = fs::metadata(&agent_dir).unwrap().permissions();
    fs::set_permissions(&agent_dir, fs::Permissions::from_mode(0o555)).unwrap();
    delivery.observe_harness(&CodexObservedState::Idle);
    fs::set_permissions(&agent_dir, live).unwrap();
    assert_eq!(
        harness_state::read(&record_path, None).unwrap().state,
        Activity::Active,
        "the failed write cannot have landed"
    );

    // No heartbeat may re-stamp the contradicted on-disk state; the retry lands the pending
    // transition on the NEXT pump pass — deliberately without advancing the presence
    // cadence, which gates only heartbeats.
    let stale_active = fs::read(&record_path).unwrap();
    delivery.next_presence_refresh = Instant::now() + status::STATUS_REFRESH;
    delivery.refresh_if_due().unwrap();
    let after = harness_state::read(&record_path, None).unwrap();
    assert_eq!(after.state, Activity::Idle, "pending transition retried");
    assert_ne!(fs::read(&record_path).unwrap(), stale_active);
}

#[test]
fn pump_publishes_observations_and_stops_heartbeating_on_evidence_loss() {
    use crate::harness_state::{self, Activity};
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    let agent_dir = config.agent_dir.clone();
    let record_path = harness_state::harness_state_path(&agent_dir);
    let mut delivery = inbox_delivery(tmp.path(), config);

    delivery.observe_harness(&CodexObservedState::Active {
        turn_id: "turn-current".into(),
    });
    let observed = harness_state::read(&record_path, None).expect("record written");
    assert_eq!(observed.state, Activity::Active);
    assert_eq!(observed.harness.as_deref(), Some("codex"));

    // An indeterminate projection writes nothing and stops the heartbeat: the presence
    // refresh still runs, but the record's bytes stay untouched and age toward unknown.
    delivery.observe_harness(&CodexObservedState::Held {
        reason: CodexHoldReason::NotLoaded,
        turn_id: None,
    });
    let before = fs::read(&record_path).unwrap();
    delivery.refresh_if_due().unwrap();
    assert!(
        status::read_state(&status::status_path(&agent_dir)) != status::State::Offline,
        "presence refresh must still run"
    );
    assert_eq!(
        fs::read(&record_path).unwrap(),
        before,
        "no heartbeat without evidence"
    );

    // Evidence returning resumes both observation and heartbeat.
    delivery.observe_harness(&CodexObservedState::Idle);
    assert_eq!(
        harness_state::read(&record_path, None).unwrap().state,
        Activity::Idle
    );
    delivery.next_presence_refresh = Instant::now();
    delivery.refresh_if_due().unwrap();
    assert_ne!(
        fs::read(&record_path).unwrap(),
        before,
        "heartbeat resumes with evidence"
    );
}

#[test]
fn evidence_loss_marks_the_stream_discontinuous_for_a_restated_state() {
    use crate::harness_state::{self, Activity};
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    let record_path = harness_state::harness_state_path(&config.agent_dir);
    let mut delivery = inbox_delivery(tmp.path(), config);

    delivery.observe_harness(&CodexObservedState::Active {
        turn_id: "turn-a".into(),
    });
    let before = fs::read(&record_path).unwrap();

    // The same tuple restated across an unproven interval must not coalesce into the
    // pre-gap record — continuity was not observed, so a fresh transition opens.
    delivery.observe_harness(&CodexObservedState::Held {
        reason: CodexHoldReason::SystemError,
        turn_id: None,
    });
    delivery.observe_harness(&CodexObservedState::Active {
        turn_id: "turn-a".into(),
    });
    assert_ne!(
        fs::read(&record_path).unwrap(),
        before,
        "a restated state after an evidence gap must open a fresh transition"
    );
    assert_eq!(
        harness_state::read(&record_path, None).unwrap().state,
        Activity::Active
    );
}

#[test]
fn delivery_client_id_is_stable_and_binds_every_identity_component() {
    let id = stable_client_user_message_id("h.worker", "thread-main", "1786380000000-abc123.md");
    assert_eq!(
        id,
        stable_client_user_message_id("h.worker", "thread-main", "1786380000000-abc123.md")
    );
    assert!(id.starts_with("st2:"));
    assert_ne!(
        id,
        stable_client_user_message_id("h.other", "thread-main", "1786380000000-abc123.md")
    );
    assert_ne!(
        id,
        stable_client_user_message_id("h.worker", "thread-other", "1786380000000-abc123.md")
    );
    assert_ne!(
        id,
        stable_client_user_message_id("h.worker", "thread-main", "1786380000000-def456.md")
    );
}

#[test]
fn review_compaction_and_dnd_hold_the_unread_fifo_head() {
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    let filename =
        message::send_to_inbox(&config.inbox, "h.sender", Some("held"), None, &[], "body").unwrap();
    let mut delivery = inbox_delivery(tmp.path(), config.clone());
    for reason in [CodexHoldReason::Review, CodexHoldReason::Compaction] {
        let state = subscribed_state(CodexObservedState::Held {
            reason,
            turn_id: Some("turn-current".into()),
        });
        assert_eq!(delivery.maybe_request(&state).unwrap(), None);
        assert!(config.inbox.join(&filename).is_file());
    }

    status::set_state(&status::status_path(&config.agent_dir), status::State::Dnd).unwrap();
    delivery.next_inbox_refresh = Instant::now();
    assert_eq!(
        delivery
            .maybe_request(&subscribed_state(CodexObservedState::Idle))
            .unwrap(),
        None
    );
    assert_eq!(message::list_inbox(&config.inbox).unwrap().len(), 1);
}

#[test]
fn failed_turn_without_idle_allows_next_native_delivery_and_preserves_system_error() {
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    message::send_to_inbox(
        &config.inbox,
        "h.sender",
        Some("after error"),
        None,
        &[],
        "body",
    )
    .unwrap();
    let mut delivery = inbox_delivery(tmp.path(), config);
    let mut state = subscribed_state(CodexObservedState::Idle);

    state
        .observe(&json!({
            "method": "turn/started",
            "params": {
                "threadId": "thread-main",
                "turn": { "id": "turn-failed" }
            }
        }))
        .unwrap();
    state
        .observe(&json!({
            "method": "thread/status/changed",
            "params": {
                "threadId": "thread-main",
                "status": { "type": "systemError" }
            }
        }))
        .unwrap();
    state
        .observe(&json!({
            "method": "turn/completed",
            "params": {
                "threadId": "thread-main",
                "turn": { "id": "turn-failed", "status": "failed" }
            }
        }))
        .unwrap();

    assert_eq!(
        state.observed(),
        &CodexObservedState::TerminalError {
            reason: CodexTerminalError::SystemError,
        }
    );

    let request = delivery
        .maybe_request(&state)
        .unwrap()
        .expect("a terminal system error must not block the next native delivery");
    assert_eq!(request["method"], "turn/start");
}

#[test]
fn captured_usage_limit_boundary_is_retryable_capacity_and_allows_next_native_delivery() {
    // This fixture is a payload-minimized projection of all 23 inbound frames from the
    // #263 trivial capture. It preserves their order and methods while removing fields this
    // observer never reads. The second capture has the same method sequence. The recorder
    // stops at turn completion, so this test pins the boundary state only. The provider
    // source establishes that no later idle notification follows the capacity error.
    let frames = include_str!("../../tests/fixtures/codex_usage_limit_inbound.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 23);

    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    message::send_to_inbox(
        &config.inbox,
        "h.sender",
        Some("after capture"),
        None,
        &[],
        "body",
    )
    .unwrap();
    let mut delivery = inbox_delivery(tmp.path(), config);
    let mut state = subscribed_state(CodexObservedState::AwaitingStatus);

    for frame in &frames {
        state.observe(frame).unwrap();
    }

    assert_eq!(
        frames
            .last()
            .and_then(|frame| frame.get("method"))
            .and_then(Value::as_str),
        Some("turn/completed")
    );
    assert_eq!(
        state.observed(),
        &CodexObservedState::TerminalError {
            reason: CodexTerminalError::ProviderCapacity,
        }
    );
    let request = delivery
        .maybe_request(&state)
        .unwrap()
        .expect("a captured provider-capacity error must permit the next native delivery");
    assert_eq!(request["method"], "turn/start");
}

/// The credential class and the quota class arrive through the SAME frame sequence, differing
/// only in one word of `Turn.error.codexErrorInfo`. This replays the auth-rejected shape and
/// asserts the fork: `providerAuth` on the observed record, a native-driver diagnostic, and
/// delivery still permitted — while the captured usage-limit fixture beside it is retryable
/// capacity rather than an authentication failure.
#[test]
fn a_rejected_codex_credential_reads_provider_auth_while_a_quota_failure_does_not() {
    let rejected = include_str!("../../tests/fixtures/codex_provider_auth_inbound.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let quota = include_str!("../../tests/fixtures/codex_usage_limit_inbound.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();

    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    let agent_dir = config.agent_dir.clone();
    message::send_to_inbox(
        &config.inbox,
        "h.sender",
        Some("after rejection"),
        None,
        &[],
        "body",
    )
    .unwrap();
    let mut delivery = inbox_delivery(tmp.path(), config);
    let mut state = subscribed_state(CodexObservedState::AwaitingStatus);
    for frame in &rejected {
        state.observe(frame).unwrap();
        delivery.observe_provider_auth(frame, "thread-main");
    }

    assert_eq!(
        state.observed(),
        &CodexObservedState::TerminalError {
            reason: CodexTerminalError::ProviderAuthRejected,
        }
    );
    let observation = state.observed().harness_observation().unwrap();
    assert_eq!(observation.state, harness_state::Activity::Ended);
    assert_eq!(
        observation.reason.as_deref(),
        Some("providerAuth"),
        "the same word OpenCode's ProviderAuthError already publishes"
    );
    let record = driver_diagnostic::path(&agent_dir);
    let driver_diagnostic::Observed::Failure(failure) = driver_diagnostic::read(&record) else {
        panic!("a rejected credential must publish a native-driver diagnostic")
    };
    assert_eq!(failure.driver, driver_diagnostic::Driver::Codex);
    assert_eq!(failure.stage, driver_diagnostic::Stage::ProviderAuth);
    assert_eq!(
        failure.reason,
        driver_diagnostic::Reason::ProviderAuthRejected
    );
    assert_eq!(failure.source, driver_diagnostic::Source::TurnResult);
    assert_eq!(
        failure.producer_version.as_deref(),
        Some("codex-cli 0.153.0")
    );
    assert_eq!(failure.support, driver_diagnostic::Support::Supported);
    let request = delivery
        .maybe_request(&state)
        .unwrap()
        .expect("a rejected credential must not block the next native delivery");
    assert_eq!(request["method"], "turn/start");

    // A turn that reaches its ordinary end is the recovery edge.
    delivery.observe_provider_auth(
        &json!({
            "method": "turn/completed",
            "params": {
                "threadId": "thread-main",
                "turn": { "id": "turn-ok", "status": "completed" }
            }
        }),
        "thread-main",
    );
    assert_eq!(
        driver_diagnostic::read(&record),
        driver_diagnostic::Observed::Absent
    );

    // The quota capture walks the same methods and must remain distinct from authentication.
    let quota_tmp = tempfile::tempdir().unwrap();
    let quota_config = delivery_config(quota_tmp.path());
    let quota_agent_dir = quota_config.agent_dir.clone();
    let mut quota_delivery = inbox_delivery(quota_tmp.path(), quota_config);
    let mut quota_state = subscribed_state(CodexObservedState::AwaitingStatus);
    for frame in &quota {
        quota_state.observe(frame).unwrap();
        quota_delivery.observe_provider_auth(frame, "thread-main");
    }
    assert_eq!(
        quota_state.observed(),
        &CodexObservedState::TerminalError {
            reason: CodexTerminalError::ProviderCapacity,
        }
    );
    let quota_observation = quota_state.observed().harness_observation().unwrap();
    assert_eq!(quota_observation.state, harness_state::Activity::Idle);
    assert_eq!(
        quota_observation.reason.as_deref(),
        Some("providerCapacity")
    );
    assert_eq!(
        driver_diagnostic::read(&driver_diagnostic::path(&quota_agent_dir)),
        driver_diagnostic::Observed::Absent,
        "an exhausted allowance is not a rejected credential"
    );
}

#[test]
fn selected_model_capacity_is_retryable_in_the_same_codex_session() {
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let mut state = CodexControlState::new(&runtime, "thread-main".into());
    state.subscribed = true;
    state
        .observe(&json!({
            "method": "turn/started",
            "params": { "threadId": "thread-main", "turn": { "id": "turn-1" } }
        }))
        .unwrap();
    state
        .observe(&json!({
            "method": "turn/completed",
            "params": {
                "threadId": "thread-main",
                "turn": {
                    "id": "turn-1",
                    "status": "failed",
                    "error": {
                        "message": "⚠ Selected model is at capacity. Please try a different model."
                    }
                }
            }
        }))
        .unwrap();

    assert_eq!(
        state.observed(),
        &CodexObservedState::TerminalError {
            reason: CodexTerminalError::ProviderCapacity,
        }
    );
    let observed = state.observed().harness_observation().unwrap();
    assert_eq!(observed.state, harness_state::Activity::Idle);
    assert_eq!(observed.reason.as_deref(), Some("providerCapacity"));

    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    message::send_to_inbox(
        &config.inbox,
        "daemon/runtime",
        Some("capacity retry"),
        None,
        &[],
        "resume",
    )
    .unwrap();
    let mut delivery = inbox_delivery(tmp.path(), config);
    let request = delivery
        .maybe_request(&state)
        .unwrap()
        .expect("capacity backoff may resume the existing session");
    assert_eq!(request["method"], "turn/start");
    assert_eq!(request["params"]["threadId"], "thread-main");
}

#[test]
fn idle_session_refreshes_stale_presence_without_inbox_activity() {
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    let presence = status::status_path(&config.agent_dir);
    std::fs::create_dir_all(&config.agent_dir).unwrap();
    std::fs::write(&presence, "available\n").unwrap();
    std::fs::File::open(&presence)
        .unwrap()
        .set_modified(SystemTime::now() - status::STATUS_STALE - Duration::from_secs(1))
        .unwrap();
    assert_eq!(status::read_state(&presence), status::State::Unknown);

    let mut delivery = inbox_delivery(tmp.path(), config);
    delivery.refresh_if_due().unwrap();

    assert_eq!(status::read_state(&presence), status::State::Available);
    assert!(
        std::fs::read_to_string(&presence)
            .unwrap()
            .contains("\nv1 ")
    );
    assert!(delivery.head.is_none());
}

#[test]
fn inbox_fallback_does_not_write_a_fifteen_second_presence_heartbeat() {
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    let presence = status::status_path(&config.agent_dir);
    status::set_state(&presence, status::State::Available).unwrap();
    let before = std::fs::read_to_string(&presence).unwrap();
    let mut delivery = inbox_delivery(tmp.path(), config);
    delivery.next_inbox_refresh = Instant::now();
    delivery.next_presence_refresh = Instant::now() + status::STATUS_REFRESH;

    delivery.refresh_if_due().unwrap();

    assert_eq!(std::fs::read_to_string(&presence).unwrap(), before);
}

#[test]
fn a_rejected_exact_steer_has_no_fallback_and_remains_retryable_after_state_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    let filename =
        message::send_to_inbox(&config.inbox, "h.sender", Some("retry"), None, &[], "body")
            .unwrap();
    let mut delivery = inbox_delivery(tmp.path(), config.clone());
    let active = subscribed_state(CodexObservedState::Active {
        turn_id: "turn-current".into(),
    });
    let steer = delivery.maybe_request(&active).unwrap().unwrap();
    assert_eq!(steer["method"], "turn/steer");
    assert_eq!(steer["params"]["expectedTurnId"], "turn-current");
    let request_id = steer["id"].clone();
    let client_id = steer["params"]["clientUserMessageId"].clone();

    assert!(
        !delivery
            .accept_response(
                &json!({
                    "id": request_id,
                    "method": "item/commandExecution/requestApproval",
                    "params": {}
                }),
                active.observed(),
            )
            .unwrap()
    );
    assert!(
        delivery
            .accept_response(
                &json!({ "id": request_id, "error": { "code": -32600, "message": "stale turn" } }),
                active.observed(),
            )
            .unwrap()
    );
    assert_eq!(delivery.maybe_request(&active).unwrap(), None);
    assert!(config.inbox.join(&filename).is_file());

    let retry = delivery
        .maybe_request(&subscribed_state(CodexObservedState::Idle))
        .unwrap()
        .unwrap();
    assert_eq!(retry["method"], "turn/start");
    assert_eq!(retry["params"]["clientUserMessageId"], client_id);
    assert!(config.inbox.join(&filename).is_file());
}

#[test]
fn a_hung_delivery_request_fails_instead_of_holding_the_fifo_forever() {
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    message::send_to_inbox(&config.inbox, "h.sender", Some("hung"), None, &[], "body").unwrap();
    let mut delivery = inbox_delivery(tmp.path(), config);
    let idle = subscribed_state(CodexObservedState::Idle);
    assert!(delivery.maybe_request(&idle).unwrap().is_some());
    delivery.pending.as_mut().unwrap().requested_at = Instant::now() - Duration::from_secs(31);
    assert!(delivery.maybe_request(&idle).is_err());
}

#[test]
fn a_rejected_delivery_retries_after_backoff_in_the_same_observed_state() {
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    message::send_to_inbox(&config.inbox, "h.sender", Some("retry"), None, &[], "body").unwrap();
    let mut delivery = inbox_delivery(tmp.path(), config);
    let idle = subscribed_state(CodexObservedState::Idle);
    let request = delivery.maybe_request(&idle).unwrap().unwrap();
    let client_id = request["params"]["clientUserMessageId"].clone();
    delivery
        .accept_response(
            &json!({
                "id": request["id"], "error": {"code": -32600, "message": "transient"}
            }),
            idle.observed(),
        )
        .unwrap();
    assert!(delivery.maybe_request(&idle).unwrap().is_none());
    delivery.rejected.as_mut().unwrap().rejected_at = Instant::now() - Duration::from_secs(6);
    let retry = delivery.maybe_request(&idle).unwrap().unwrap();
    assert_eq!(retry["params"]["clientUserMessageId"], client_id);
}

#[test]
fn a_success_response_persists_transport_and_exact_turn_without_archiving() {
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    let filename = message::send_to_inbox(
        &config.inbox,
        "h.sender",
        Some("submitted"),
        None,
        &[],
        "body",
    )
    .unwrap();
    let mut delivery = inbox_delivery(tmp.path(), config.clone());
    let idle = subscribed_state(CodexObservedState::Idle);
    let request = delivery.maybe_request(&idle).unwrap().unwrap();
    assert_eq!(
        delivery.ledger.entry(&filename).unwrap().phase,
        delivery_ledger::Phase::Attempted,
        "submission ownership is durable before transport"
    );
    assert!(
        delivery
            .accept_response(
                &json!({ "id": request["id"], "result": { "turn": { "id": "turn-new" } } }),
                idle.observed(),
            )
            .unwrap()
    );
    assert_eq!(
        delivery.ledger.entry(&filename).unwrap().phase,
        delivery_ledger::Phase::TransportAccepted,
        "a well-formed JSON result is transport, never typed acceptance"
    );
    assert_eq!(
        delivery
            .ledger
            .entry(&filename)
            .unwrap()
            .accepted_turn_id
            .as_deref(),
        Some("turn-new")
    );
    assert_eq!(delivery.maybe_request(&idle).unwrap(), None);
    assert!(config.inbox.join(&filename).is_file());
}

#[test]
fn a_consumed_unarchived_head_releases_the_next_fifo_message_exactly_once() {
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    let first = message::send_to_inbox(
        &config.inbox,
        "h.sender",
        Some("long-running directive"),
        None,
        &[],
        "first",
    )
    .unwrap();
    let mut delivery = inbox_delivery(tmp.path(), config.clone());
    let mut state = CodexControlState::new(&delivery.runtime, "thread-main".into());
    state.subscribed = true;
    state.observed = CodexObservedState::Idle;
    let request = delivery.maybe_request(&state).unwrap().unwrap();
    let first_client_id = request["params"]["clientUserMessageId"]
        .as_str()
        .unwrap()
        .to_owned();
    delivery
        .accept_response(
            &json!({ "id": request["id"], "result": { "turn": { "id": "turn-first" } } }),
            state.observed(),
        )
        .unwrap();
    delivery
        .accept_typed_receipt(
            &json!({
                "method": "item/completed",
                "params": {
                    "threadId": "thread-main",
                    "turnId": "turn-first",
                    "item": { "type": "userMessage", "clientId": first_client_id }
                }
            }),
            &state,
        )
        .unwrap();
    assert!(config.inbox.join(&first).is_file());
    assert_eq!(
        delivery.ledger.entry(&first).unwrap().phase,
        delivery_ledger::Phase::Consumed
    );

    let second = message::send_to_inbox(
        &config.inbox,
        "h.sender",
        Some("later message"),
        None,
        &[],
        "second",
    )
    .unwrap();
    delivery.next_inbox_refresh = Instant::now();
    let next = delivery
        .maybe_request(&state)
        .unwrap()
        .expect("the consumed unarchived head must not hold the later message");
    assert_eq!(
        next["params"]["clientUserMessageId"],
        stable_client_user_message_id("h.worker", "thread-main", &second)
    );
    assert_eq!(delivery.maybe_request(&state).unwrap(), None);
    assert!(config.inbox.join(&first).is_file());
    assert!(delivery.ledger.entry(&first).is_some());
    assert_eq!(delivery.ledger.entries().len(), 2);
}

#[test]
fn a_working_turn_reconciles_a_missed_steer_receipt_and_delivers_the_next_ping() {
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    let first = message::send_to_inbox(
        &config.inbox,
        "h.sender",
        Some("first ping"),
        None,
        &[],
        "first",
    )
    .unwrap();
    let mut delivery = inbox_delivery(tmp.path(), config.clone());
    let state = subscribed_state(CodexObservedState::Active {
        turn_id: "turn-live".into(),
    });
    let steer = delivery.maybe_request(&state).unwrap().unwrap();
    assert_eq!(steer["method"], "turn/steer");
    let client_id = steer["params"]["clientUserMessageId"]
        .as_str()
        .unwrap()
        .to_owned();
    delivery
        .accept_response(
            &json!({"id": steer["id"], "result": {"turnId": "turn-live"}}),
            state.observed(),
        )
        .unwrap();
    assert_eq!(
        delivery.ledger.entry(&first).unwrap().phase,
        delivery_ledger::Phase::TransportAccepted
    );
    let second = message::send_to_inbox(
        &config.inbox,
        "h.sender",
        Some("second ping"),
        None,
        &[],
        "second",
    )
    .unwrap();

    assert!(delivery.maybe_snapshot_request(&state).unwrap().is_none());
    delivery
        .accept_transcript_receipts(
            &[json!({
                "type": "event_msg",
                "payload": {"type": "item_completed", "thread_id": "thread-main",
                    "item": {"type": "UserMessage", "client_id": client_id}}
            })],
            "thread-main",
        )
        .unwrap();
    assert_eq!(
        delivery.ledger.entry(&first).unwrap().phase,
        delivery_ledger::Phase::Consumed
    );
    assert_eq!(
        state.observed(),
        &CodexObservedState::Active {
            turn_id: "turn-live".into()
        }
    );
    let next = delivery
        .maybe_request(&state)
        .unwrap()
        .expect("next PING must reach the still-running turn");
    assert_eq!(next["method"], "turn/steer");
    assert_eq!(
        next["params"]["clientUserMessageId"],
        stable_client_user_message_id("h.worker", "thread-main", &second)
    );
}

#[test]
fn a_delivery_on_a_history_larger_than_the_control_limit_uses_bounded_reads() {
    let mut request_lengths = Vec::new();
    let mut transcript_bytes_read = Vec::new();
    for history_bytes in [
        0,
        CODEX_CONTROL_MAX_MESSAGE_BYTES as u64 + 1024,
        2 * CODEX_CONTROL_MAX_MESSAGE_BYTES as u64 + 1024,
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let config = delivery_config(tmp.path());
        let filename =
            message::send_to_inbox(&config.inbox, "h.sender", None, None, &[], "ping").unwrap();
        let mut delivery = inbox_delivery(tmp.path(), config);
        let mut state = subscribed_state(CodexObservedState::Active {
            turn_id: "turn-live".into(),
        });
        let snapshot = delivery.maybe_snapshot_request(&state).unwrap().unwrap();
        assert_eq!(snapshot["params"]["includeTurns"], false);
        request_lengths.push(serde_json::to_vec(&snapshot).unwrap().len());
        delivery
            .accept_snapshot_response(
                &json!({"id": snapshot["id"], "result": {"thread": {
                    "id": "thread-main", "status": {"type": "active"}, "turns": []
                }}}),
                &mut state,
            )
            .unwrap();
        let request = delivery.maybe_request(&state).unwrap().unwrap();
        let client_id = request["params"]["clientUserMessageId"].as_str().unwrap();
        delivery
            .accept_response(
                &json!({"id": request["id"], "result": {"turnId": "turn-live"}}),
                state.observed(),
            )
            .unwrap();
        assert!(delivery.maybe_snapshot_request(&state).unwrap().is_none());

        let transcript = tmp.path().join("rollout.jsonl");
        let mut file = File::create(&transcript).unwrap();
        file.set_len(history_bytes).unwrap();
        file.seek(SeekFrom::End(0)).unwrap();
        writeln!(file).unwrap();
        writeln!(
            file,
            "{}",
            json!({"type": "event_msg", "payload": {
                "type": "item_completed", "thread_id": "thread-main",
                "item": {"type": "UserMessage", "client_id": client_id}
            }})
        )
        .unwrap();
        let (frames, bytes_read) = codex_transcript_tail_with_bytes(&transcript).unwrap();
        transcript_bytes_read.push(bytes_read);
        delivery
            .accept_transcript_receipts(&frames, "thread-main")
            .unwrap();
        assert_eq!(
            delivery.ledger.entry(&filename).unwrap().phase,
            delivery_ledger::Phase::Consumed
        );
        assert!(file.metadata().unwrap().len() > history_bytes);
    }
    assert_eq!(request_lengths[0], request_lengths[1]);
    assert_eq!(request_lengths[1], request_lengths[2]);
    assert_eq!(transcript_bytes_read[1], TRANSCRIPT_TURN_RECOVERY_BYTES);
    assert_eq!(transcript_bytes_read[1], transcript_bytes_read[2]);
}

#[test]
fn a_transcript_user_message_receipt_is_exact_and_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    let filename =
        message::send_to_inbox(&config.inbox, "h.sender", None, None, &[], "ping").unwrap();
    let mut delivery = inbox_delivery(tmp.path(), config);
    let state = subscribed_state(CodexObservedState::Active {
        turn_id: "turn-live".into(),
    });
    let steer = delivery.maybe_request(&state).unwrap().unwrap();
    let client_id = steer["params"]["clientUserMessageId"].as_str().unwrap();
    delivery
        .accept_response(
            &json!({"id": steer["id"], "result": {"turnId": "turn-live"}}),
            state.observed(),
        )
        .unwrap();
    let receipt = json!({"type": "event_msg", "payload": {"type": "item_completed", "thread_id": "thread-main", "turn_id": "turn-live", "item": {"type": "UserMessage", "client_id": client_id}}});
    delivery
        .accept_transcript_receipts(&[receipt.clone()], "other-thread")
        .unwrap();
    assert_eq!(
        delivery.ledger.entry(&filename).unwrap().phase,
        delivery_ledger::Phase::TransportAccepted
    );
    delivery
        .accept_transcript_receipts(&[receipt.clone(), receipt], "thread-main")
        .unwrap();
    assert_eq!(
        delivery.ledger.entry(&filename).unwrap().phase,
        delivery_ledger::Phase::Consumed
    );
}

#[test]
fn a_successful_exact_turn_completion_settles_when_codex_omits_the_item_receipt() {
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    let first = message::send_to_inbox(
        &config.inbox,
        "h.sender",
        Some("Codex 0.146 sequence"),
        None,
        &[],
        "first",
    )
    .unwrap();
    let mut delivery = inbox_delivery(tmp.path(), config.clone());
    let mut state = CodexControlState::new(&delivery.runtime, "thread-main".into());
    state.subscribed = true;
    state.observed = CodexObservedState::Idle;
    let request = delivery.maybe_request(&state).unwrap().unwrap();
    delivery
        .accept_response(
            &json!({ "id": request["id"], "result": { "turn": { "id": "turn-first" } } }),
            state.observed(),
        )
        .unwrap();
    assert!(
        !delivery
            .accept_turn_completion_receipt(
                &json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": "thread-main",
                        "turn": { "id": "turn-other", "status": "completed", "items": [] }
                    }
                }),
                &state,
            )
            .unwrap()
    );
    assert_eq!(
        delivery.ledger.entry(&first).unwrap().phase,
        delivery_ledger::Phase::TransportAccepted
    );
    assert!(
        delivery
            .accept_turn_completion_receipt(
                &json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": "thread-main",
                        "turn": { "id": "turn-first", "status": "completed", "items": [] }
                    }
                }),
                &state,
            )
            .unwrap()
    );
    assert_eq!(
        delivery.ledger.entry(&first).unwrap().phase,
        delivery_ledger::Phase::Consumed
    );

    let second = message::send_to_inbox(
        &config.inbox,
        "h.sender",
        Some("after successful turn"),
        None,
        &[],
        "second",
    )
    .unwrap();
    delivery.next_inbox_refresh = Instant::now();
    let next = delivery.maybe_request(&state).unwrap().unwrap();
    assert_eq!(
        next["params"]["clientUserMessageId"],
        stable_client_user_message_id("h.worker", "thread-main", &second)
    );
    assert!(config.inbox.join(&first).is_file());
}

#[test]
fn only_a_completed_matching_user_message_persists_acceptance() {
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    let filename = message::send_to_inbox(
        &config.inbox,
        "h.sender",
        Some("receipt"),
        None,
        &[],
        "body",
    )
    .unwrap();
    let mut delivery = inbox_delivery(tmp.path(), config.clone());
    let mut idle = CodexControlState::new(&delivery.runtime, "thread-main".into());
    idle.subscribed = true;
    idle.observed = CodexObservedState::Idle;
    let request = delivery.maybe_request(&idle).unwrap().unwrap();
    let client_id = request["params"]["clientUserMessageId"]
        .as_str()
        .unwrap()
        .to_string();

    assert!(
        !delivery
            .accept_typed_receipt(
                &json!({
                    "method": "item/started",
                    "params": {
                        "threadId": "thread-main",
                        "turnId": "turn-delivery",
                        "item": { "type": "userMessage", "clientId": client_id }
                    }
                }),
                &idle,
            )
            .unwrap(),
        "item/started is progress, not acceptance"
    );
    assert!(
        !delivery
            .accept_typed_receipt(
                &json!({
                    "method": "item/completed",
                    "params": {
                        "threadId": "thread-other",
                        "turnId": "turn-delivery",
                        "item": { "type": "userMessage", "clientId": client_id }
                    }
                }),
                &idle,
            )
            .unwrap(),
        "another thread cannot acknowledge this delivery"
    );
    assert!(
        delivery
            .accept_typed_receipt(
                &json!({
                    "method": "item/completed",
                    "params": {
                        "threadId": "thread-main",
                        "turnId": "turn-delivery",
                        "item": { "type": "userMessage", "clientId": client_id }
                    }
                }),
                &idle,
            )
            .unwrap()
    );
    assert_eq!(
        ledger_entry(tmp.path(), &filename).unwrap().phase,
        delivery_ledger::Phase::Consumed
    );
    assert!(config.inbox.join(&filename).is_file());

    drop(delivery);
    let mut replacement = inbox_delivery(tmp.path(), config.clone());
    assert_eq!(
        replacement.maybe_request(&idle).unwrap(),
        None,
        "a fresh runtime incarnation restores accepted duplicate control"
    );

    message::archive_msg(
        &config.inbox,
        &message::archive_dir(&config.agent_dir),
        &filename,
    )
    .unwrap();
    replacement.next_inbox_refresh = Instant::now();
    assert_eq!(replacement.maybe_request(&idle).unwrap(), None);
    assert!(
        ledger_entry(tmp.path(), &filename).is_none(),
        "archive precedence — the recipient agent's own act — releases the ledger entry"
    );
}

#[test]
fn an_ambiguous_attempt_reconciles_resume_history_before_retry() {
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    let filename = message::send_to_inbox(
        &config.inbox,
        "h.sender",
        Some("reconcile"),
        None,
        &[],
        "body",
    )
    .unwrap();
    let idle = subscribed_state(CodexObservedState::Idle);
    let mut first = inbox_delivery(tmp.path(), config.clone());
    let request = first.maybe_request(&idle).unwrap().unwrap();
    let client_id = request["params"]["clientUserMessageId"]
        .as_str()
        .unwrap()
        .to_string();
    drop(first);

    let mut recovered = inbox_delivery(tmp.path(), config.clone());
    assert_eq!(recovered.maybe_request(&idle).unwrap(), None);
    recovered
        .reconcile_resume(
            &json!({
                "id": CONTROL_SUBSCRIBE_REQUEST_ID,
                "result": {
                    "thread": {
                        "id": "thread-main",
                        "turns": [{
                            "id": "turn-delivery",
                            "items": [{
                                "type": "userMessage",
                                "id": "item-delivery",
                                "clientId": client_id,
                                "content": []
                            }]
                        }]
                    }
                }
            }),
            &idle,
        )
        .unwrap();
    assert_eq!(
        recovered.ledger.entry(&filename).unwrap().phase,
        delivery_ledger::Phase::Consumed,
        "a resumed history carrying the client ID is the same typed receipt, found late"
    );
    assert_eq!(recovered.maybe_request(&idle).unwrap(), None);
    assert!(config.inbox.join(&filename).is_file());

    // An authoritative resumed history WITHOUT the client ID proves the pre-crash attempt
    // never landed. Only that absence may re-authorize the same stable ID — so it needs its
    // own scenario, because the delivery above is settled and can never be un-settled.
    let absent_tmp = tempfile::tempdir().unwrap();
    let absent_config = delivery_config(absent_tmp.path());
    let absent_filename = message::send_to_inbox(
        &absent_config.inbox,
        "h.sender",
        Some("absent"),
        None,
        &[],
        "body",
    )
    .unwrap();
    let mut attempted = inbox_delivery(absent_tmp.path(), absent_config.clone());
    let absent_client_id =
        attempted.maybe_request(&idle).unwrap().unwrap()["params"]["clientUserMessageId"]
            .as_str()
            .unwrap()
            .to_string();
    drop(attempted);

    let mut replacement = inbox_delivery(absent_tmp.path(), absent_config);
    assert_eq!(
        replacement.maybe_request(&idle).unwrap(),
        None,
        "an ambiguous attempt is held and surfaced, never replayed on its own"
    );
    replacement
        .reconcile_resume(
            &json!({
                "id": CONTROL_SUBSCRIBE_REQUEST_ID,
                "result": { "thread": { "id": "thread-main", "turns": [] } }
            }),
            &idle,
        )
        .unwrap();
    assert_eq!(
        replacement.ledger.entry(&absent_filename).unwrap().negative,
        Some(delivery_ledger::NegativeReceipt::Absent),
        "the absence is retained as evidence, not erased"
    );
    let retry = replacement.maybe_request(&idle).unwrap().unwrap();
    assert_eq!(retry["params"]["clientUserMessageId"], absent_client_id);
}

#[test]
fn subscribed_control_pump_delivers_a_typed_reference_to_the_real_fifo_head() {
    let tmp = tempfile::tempdir().unwrap();
    let _stop_exclusive = stop_flag_tests();
    let config = delivery_config(tmp.path());
    let filename =
        message::send_to_inbox(&config.inbox, "h.sender", Some("wired"), None, &[], "body")
            .unwrap();
    let socket = tmp.path().join("server.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server_filename = filename.clone();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            // Parallel Darwin test runs can deschedule the in-process peer
            // for longer than the Linux-oriented two-second budget.
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut websocket = tungstenite::accept(stream).unwrap();
        assert_eq!(
            read_json_message(&mut websocket).unwrap().unwrap()["method"],
            "initialize"
        );
        write_json_message(
            &mut websocket,
            &json!({ "id": 0, "result": { "userAgent": "fake" } }),
        )
        .unwrap();
        assert_eq!(
            read_json_message(&mut websocket).unwrap().unwrap()["method"],
            "initialized"
        );
        write_json_message(
            &mut websocket,
            &json!({
                "method": "thread/started",
                "params": { "thread": { "id": "thread-main", "status": { "type": "idle" } } }
            }),
        )
        .unwrap();
        let snapshot = read_json_message(&mut websocket).unwrap().unwrap();
        assert_eq!(snapshot["id"], FIRST_DELIVERY_REQUEST_ID);
        assert_eq!(snapshot["method"], "thread/read");
        assert_eq!(snapshot["params"]["threadId"], "thread-main");
        assert_eq!(snapshot["params"]["includeTurns"], false);
        write_json_message(
            &mut websocket,
            &json!({
                "id": FIRST_DELIVERY_REQUEST_ID,
                "result": {
                    "thread": {
                        "id": "thread-main",
                        "status": { "type": "idle" },
                        "turns": []
                    }
                }
            }),
        )
        .unwrap();
        let delivery = read_json_message(&mut websocket).unwrap().unwrap();
        assert_eq!(delivery["id"], FIRST_DELIVERY_REQUEST_ID + 1);
        assert_eq!(delivery["method"], "turn/start");
        assert_eq!(delivery["params"]["threadId"], "thread-main");
        let head_id = server_filename
            .trim_end_matches(".md")
            .rsplit_once('-')
            .unwrap()
            .1;
        assert!(
            delivery["params"]["input"][0]["text"]
                .as_str()
                .unwrap()
                .contains(head_id),
            "the transport payload must identify the actionable FIFO head"
        );
        assert_eq!(
            delivery["params"]["clientUserMessageId"],
            stable_client_user_message_id("h.worker", "thread-main", &server_filename)
        );
        let client_id = delivery["params"]["clientUserMessageId"]
            .as_str()
            .unwrap()
            .to_string();
        write_json_message(
            &mut websocket,
            &json!({
                "id": FIRST_DELIVERY_REQUEST_ID + 1,
                "result": { "turn": { "id": "turn-delivery" } }
            }),
        )
        .unwrap();
        write_json_message(
            &mut websocket,
            &json!({
                "method": "item/completed",
                "params": {
                    "threadId": "thread-main",
                    "turnId": "turn-delivery",
                    "item": {
                        "type": "userMessage",
                        "id": "item-delivery",
                        "clientId": client_id,
                        "content": []
                    }
                }
            }),
        )
        .unwrap();
    });

    let stream = UnixStream::connect(&socket).unwrap();
    let shutdown = stream.try_clone().unwrap();
    let websocket = initialize_control(stream)
        .unwrap()
        .expect("no stop raised in tests");
    let binding_path = tmp.path().join("state/binding.json");
    let control_state_path = tmp.path().join("state/control-state.json");
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let (tx, rx) = mpsc::channel();
    let runtime_for_pump = runtime.clone();
    let binding_for_pump = binding_path.clone();
    let control_state_for_pump = control_state_path.clone();
    let pump = thread::spawn(move || {
        pump_control(
            websocket,
            &binding_for_pump,
            &control_state_for_pump,
            &runtime_for_pump,
            None,
            Some(config),
            Arc::new(AtomicBool::new(false)),
            tx,
        )
    });
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        ControlEvent::Bound
    ));
    server.join().unwrap();
    let _ = shutdown.shutdown(Shutdown::Both);
    pump.join().unwrap();
    assert!(delivery_config(tmp.path()).inbox.join(&filename).is_file());
    assert_eq!(
        ledger_entry(tmp.path(), &filename).unwrap().phase,
        delivery_ledger::Phase::Consumed
    );
}

/// The wiring, not the arithmetic: a `thread/tokenUsage/updated` arriving on the real control
/// socket reaches the record. Every other context test drives the producer directly, so all of
/// them would stay green if the pump stopped handing it frames — which is exactly how a
/// producer silently stops producing.
#[test]
fn the_control_pump_publishes_a_context_reading_from_a_live_token_usage_notification() {
    let tmp = tempfile::tempdir().unwrap();
    let _stop_exclusive = stop_flag_tests();
    let config = delivery_config(tmp.path());
    let agent_dir = config.agent_dir.clone();
    let socket = tmp.path().join("server.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut websocket = tungstenite::accept(stream).unwrap();
        assert_eq!(
            read_json_message(&mut websocket).unwrap().unwrap()["method"],
            "initialize"
        );
        write_json_message(
            &mut websocket,
            &json!({ "id": 0, "result": { "userAgent": "fake" } }),
        )
        .unwrap();
        assert_eq!(
            read_json_message(&mut websocket).unwrap().unwrap()["method"],
            "initialized"
        );
        write_json_message(
            &mut websocket,
            &json!({
                "method": "thread/started",
                "params": { "thread": { "id": "thread-main", "status": { "type": "idle" } } }
            }),
        )
        .unwrap();
        write_json_message(&mut websocket, &token_usage_frame(92_283, json!(258_400))).unwrap();
        // Hold the connection open until the reading has landed: closing here would race the
        // pump's read of the frame just written. Bounded, so a pump that stopped handing
        // frames to the producer fails this test instead of hanging it.
        let deadline = Instant::now() + Duration::from_secs(10);
        while harness_context::read(&harness_context::harness_context_path(&agent_dir)).is_none()
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
    });

    let stream = UnixStream::connect(&socket).unwrap();
    let shutdown = stream.try_clone().unwrap();
    let websocket = initialize_control(stream)
        .unwrap()
        .expect("no stop raised in tests");
    let binding_path = tmp.path().join("state/binding.json");
    let control_state_path = tmp.path().join("state/control-state.json");
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let (tx, rx) = mpsc::channel();
    let binding_for_pump = binding_path.clone();
    let control_state_for_pump = control_state_path.clone();
    let pump = thread::spawn(move || {
        pump_control(
            websocket,
            &binding_for_pump,
            &control_state_for_pump,
            &runtime,
            None,
            Some(config),
            Arc::new(AtomicBool::new(false)),
            tx,
        )
    });
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        ControlEvent::Bound
    ));
    server.join().unwrap();
    let _ = shutdown.shutdown(Shutdown::Both);
    pump.join().unwrap();

    let observed =
        context_record(&tmp.path().join("agents/h/worker")).expect("the pump published nothing");
    assert_eq!(observed.harness, harness_context::Harness::Codex);
    assert_eq!(observed.used_tokens, Some(92_283));
    assert_eq!(observed.window_tokens, Some(258_400));
    assert_eq!(observed.used_percent, Some(33.0));
}

#[test]
fn control_initializes_before_recording_the_first_thread_only() {
    let tmp = tempfile::tempdir().unwrap();
    let _stop_exclusive = stop_flag_tests();
    let socket = tmp.path().join("server.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut websocket = tungstenite::accept(stream).unwrap();
        let initialize = read_json_message(&mut websocket).unwrap().unwrap();
        assert_eq!(initialize["method"], "initialize");
        assert_eq!(initialize["params"]["clientInfo"]["name"], "st2");
        write_json_message(
            &mut websocket,
            &json!({ "id": 0, "result": { "userAgent": "fake" } }),
        )
        .unwrap();
        let initialized = read_json_message(&mut websocket).unwrap().unwrap();
        assert_eq!(initialized["method"], "initialized");
        write_json_message(
            &mut websocket,
            &json!({
                "method": "thread/started",
                "params": { "thread": { "id": "thread-main", "status": { "type": "idle" } } }
            }),
        )
        .unwrap();
        write_json_message(
            &mut websocket,
            &json!({
                "method": "thread/status/changed",
                "params": { "threadId": "thread-main", "status": { "type": "idle" } }
            }),
        )
        .unwrap();
        // JSON-RPC request IDs are per direction. A server request may reuse the client's
        // subscription ID and must not be consumed as a client response.
        write_json_message(
            &mut websocket,
            &json!({
                "id": CONTROL_SUBSCRIBE_REQUEST_ID,
                "method": "item/commandExecution/requestApproval",
                "params": {}
            }),
        )
        .unwrap();
        write_json_message(
            &mut websocket,
            &json!({
                "method": "thread/started",
                "params": { "thread": { "id": "thread-review", "status": { "type": "idle" } } }
            }),
        )
        .unwrap();
        write_json_message(
            &mut websocket,
            &json!({
                "method": "turn/started",
                "params": { "threadId": "thread-main", "turn": { "id": "turn-main" } }
            }),
        )
        .unwrap();
    });

    let stream = UnixStream::connect(&socket).unwrap();
    let shutdown = stream.try_clone().unwrap();
    let websocket = initialize_control(stream)
        .unwrap()
        .expect("no stop raised in tests");
    let state = tmp.path().join("state");
    let binding_path = state.join("binding.json");
    let control_state_path = state.join("control-state.json");
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let (tx, rx) = mpsc::channel();
    let runtime_for_pump = runtime.clone();
    let binding_for_pump = binding_path.clone();
    let control_state_for_pump = control_state_path.clone();
    let pump = thread::spawn(move || {
        pump_control(
            websocket,
            &binding_for_pump,
            &control_state_for_pump,
            &runtime_for_pump,
            None,
            None,
            Arc::new(AtomicBool::new(false)),
            tx,
        )
    });
    let first_event = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(
        matches!(first_event, ControlEvent::Bound),
        "first control event: {first_event:?}"
    );
    server.join().unwrap();
    let _ = shutdown.shutdown(Shutdown::Both);
    pump.join().unwrap();

    let binding = load_current_binding(&binding_path, &runtime)
        .unwrap()
        .unwrap();
    assert_eq!(binding.thread_id(), "thread-main");
    let state = load_current_control_state(&state.join("control-state.json"), &runtime, &binding)
        .unwrap()
        .unwrap();
    assert_eq!(
        state.observed(),
        &CodexObservedState::Active {
            turn_id: "turn-main".into()
        }
    );
    assert!(state.subscribed());
}

#[test]
fn expected_resume_waits_for_tui_loaded_thread_and_binds_from_control_response() {
    let tmp = tempfile::tempdir().unwrap();
    let _stop_exclusive = stop_flag_tests();
    let socket = tmp.path().join("server.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (pre_gate_checked_tx, pre_gate_checked_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut websocket = tungstenite::accept(stream).unwrap();
        let initialize = read_json_message(&mut websocket).unwrap().unwrap();
        assert_eq!(initialize["method"], "initialize");
        write_json_message(
            &mut websocket,
            &json!({ "id": 0, "result": { "userAgent": "fake" } }),
        )
        .unwrap();
        let initialized = read_json_message(&mut websocket).unwrap().unwrap();
        assert_eq!(initialized["method"], "initialized");
        assert!(matches!(
            poll_json_message(&mut websocket).unwrap(),
            ControlRead::Timeout
        ));
        pre_gate_checked_tx.send(()).unwrap();
        websocket
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        write_json_message(
            &mut websocket,
            &json!({
                "method": "thread/started",
                "params": {
                    "thread": { "id": "thread-unrelated", "status": { "type": "idle" } }
                }
            }),
        )
        .unwrap();
        write_json_message(
            &mut websocket,
            &json!({
                "method": "thread/status/changed",
                "params": {
                    "threadId": "thread-unrelated",
                    "status": { "type": "active", "activeFlags": [] }
                }
            }),
        )
        .unwrap();
        let first_loaded = read_json_message(&mut websocket).unwrap().unwrap();
        assert_eq!(first_loaded["method"], "thread/loaded/list");
        assert_eq!(first_loaded["id"], CONTROL_TUI_LOADED_REQUEST_ID);
        write_json_message(
            &mut websocket,
            &json!({
                "id": CONTROL_TUI_LOADED_REQUEST_ID,
                "result": { "data": ["thread-unrelated"] }
            }),
        )
        .unwrap();
        let second_loaded = read_json_message(&mut websocket).unwrap().unwrap();
        assert_eq!(second_loaded["method"], "thread/loaded/list");
        write_json_message(
            &mut websocket,
            &json!({
                "id": CONTROL_TUI_LOADED_REQUEST_ID,
                "result": { "data": ["thread-unrelated", "thread-prior"] }
            }),
        )
        .unwrap();
        let subscribe = read_json_message(&mut websocket).unwrap().unwrap();
        assert_eq!(subscribe["method"], "thread/resume");
        assert_eq!(subscribe["params"]["threadId"], "thread-prior");
        assert_eq!(subscribe["params"]["approvalPolicy"], "never");
        assert_eq!(subscribe["params"]["sandbox"], "danger-full-access");
        write_json_message(
            &mut websocket,
            &json!({
                "id": CONTROL_SUBSCRIBE_REQUEST_ID,
                "result": {
                    "thread": { "id": "thread-prior", "status": { "type": "idle" } },
                    "approvalPolicy": "never",
                    "sandbox": { "type": "dangerFullAccess" }
                }
            }),
        )
        .unwrap();
    });

    let stream = UnixStream::connect(&socket).unwrap();
    let shutdown = stream.try_clone().unwrap();
    let websocket = initialize_control(stream)
        .unwrap()
        .expect("no stop raised in tests");
    let binding_path = tmp.path().join("state/binding.json");
    let control_state_path = tmp.path().join("state/control-state.json");
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let (tx, rx) = mpsc::channel();
    let (resume_ready_tx, resume_ready_rx) = mpsc::channel();
    let runtime_for_pump = runtime.clone();
    let binding_for_pump = binding_path.clone();
    let control_state_for_pump = control_state_path.clone();
    let pump = thread::spawn(move || {
        pump_control(
            websocket,
            &binding_for_pump,
            &control_state_for_pump,
            &runtime_for_pump,
            Some(ControlResume {
                thread_id: "thread-prior",
                ready: resume_ready_rx,
                tui_loaded_timeout: TUI_LOADED_TIMEOUT,
                permission_overrides: Some(ResumePermissionOverrides {
                    approval_policy: Some("never".into()),
                    approvals_reviewer: None,
                    sandbox: Some("danger-full-access".into()),
                }),
                preload: false,
                preloaded: None,
            }),
            None,
            Arc::new(AtomicBool::new(false)),
            tx,
        )
    });
    pre_gate_checked_rx
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    resume_ready_tx.send(()).unwrap();
    acknowledge_tui_thread_loaded(&rx);
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        ControlEvent::ResumePermissionPolicyApplied(ResumePermissionOverrides {
            approval_policy: Some(policy),
            approvals_reviewer: None,
            sandbox: Some(sandbox),
        }) if policy == "never" && sandbox == "danger-full-access"
    ));
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        ControlEvent::Bound
    ));
    server.join().unwrap();
    let _ = shutdown.shutdown(Shutdown::Both);
    pump.join().unwrap();

    let binding = load_current_binding(&binding_path, &runtime)
        .unwrap()
        .unwrap();
    assert_eq!(binding.thread_id(), "thread-prior");
    let state = load_current_control_state(&control_state_path, &runtime, &binding)
        .unwrap()
        .unwrap();
    assert!(state.subscribed());
    assert_eq!(state.observed(), &CodexObservedState::Idle);
}

#[test]
fn declared_resume_policy_preloads_before_the_tui_can_load_read_only() {
    let tmp = tempfile::tempdir().unwrap();
    let _stop_exclusive = stop_flag_tests();
    let socket = tmp.path().join("server.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut websocket = tungstenite::accept(stream).unwrap();
        assert_eq!(
            read_json_message(&mut websocket).unwrap().unwrap()["method"],
            "initialize"
        );
        write_json_message(
            &mut websocket,
            &json!({ "id": 0, "result": { "userAgent": "fake" } }),
        )
        .unwrap();
        assert_eq!(
            read_json_message(&mut websocket).unwrap().unwrap()["method"],
            "initialized"
        );
        let request = read_json_message(&mut websocket).unwrap().unwrap();
        assert_eq!(request["method"], "thread/resume");
        assert_eq!(request["params"]["threadId"], "thread-prior");
        assert_eq!(request["params"]["approvalPolicy"], "never");
        assert_eq!(request["params"]["sandbox"], "danger-full-access");
        write_json_message(
            &mut websocket,
            &json!({
                "id": CONTROL_SUBSCRIBE_REQUEST_ID,
                "result": {
                    "thread": { "id": "thread-prior", "status": { "type": "idle" } },
                    "approvalPolicy": "never",
                    "sandbox": { "type": "dangerFullAccess" }
                }
            }),
        )
        .unwrap();
    });

    let stream = UnixStream::connect(&socket).unwrap();
    let shutdown = stream.try_clone().unwrap();
    let websocket = initialize_control(stream)
        .unwrap()
        .expect("no stop raised in tests");
    let binding_path = tmp.path().join("state/binding.json");
    let control_state_path = tmp.path().join("state/control-state.json");
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let (events_tx, events_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (preloaded_tx, preloaded_rx) = mpsc::channel();
    let pump = thread::spawn(move || {
        pump_control(
            websocket,
            &binding_path,
            &control_state_path,
            &runtime,
            Some(ControlResume {
                thread_id: "thread-prior",
                ready: ready_rx,
                tui_loaded_timeout: TUI_LOADED_TIMEOUT,
                permission_overrides: Some(ResumePermissionOverrides {
                    approval_policy: Some("never".into()),
                    approvals_reviewer: None,
                    sandbox: Some("danger-full-access".into()),
                }),
                preload: true,
                preloaded: Some(preloaded_tx),
            }),
            None,
            Arc::new(AtomicBool::new(false)),
            events_tx,
        )
    });
    ready_tx.send(()).unwrap();
    preloaded_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(matches!(
        events_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        ControlEvent::ResumePermissionPolicyApplied(_)
    ));
    assert!(matches!(
        events_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        ControlEvent::Bound
    ));
    server.join().unwrap();
    let _ = shutdown.shutdown(Shutdown::Both);
    pump.join().unwrap();
}

#[test]
fn rejected_resume_permission_projection_retries_once_with_provider_safe_policy() {
    let tmp = tempfile::tempdir().unwrap();
    let _stop_exclusive = stop_flag_tests();
    let socket = tmp.path().join("server.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let mut websocket = tungstenite::accept(stream).unwrap();
        assert_eq!(
            read_json_message(&mut websocket).unwrap().unwrap()["method"],
            "initialize"
        );
        write_json_message(
            &mut websocket,
            &json!({ "id": 0, "result": { "userAgent": "fake" } }),
        )
        .unwrap();
        assert_eq!(
            read_json_message(&mut websocket).unwrap().unwrap()["method"],
            "initialized"
        );
        let loaded = read_json_message(&mut websocket).unwrap().unwrap();
        assert_eq!(loaded["method"], "thread/loaded/list");
        write_json_message(
            &mut websocket,
            &json!({
                "id": CONTROL_TUI_LOADED_REQUEST_ID,
                "result": { "data": ["thread-prior"] }
            }),
        )
        .unwrap();

        let declared = read_json_message(&mut websocket).unwrap().unwrap();
        assert_eq!(declared["method"], "thread/resume");
        assert_eq!(declared["params"]["threadId"], "thread-prior");
        assert_eq!(declared["params"]["approvalPolicy"], "never");
        assert_eq!(declared["params"]["sandbox"], "danger-full-access");
        write_json_message(
            &mut websocket,
            &json!({
                "id": CONTROL_SUBSCRIBE_REQUEST_ID,
                "error": {
                    "code": -32602,
                    "message": "permission override rejected"
                }
            }),
        )
        .unwrap();

        let fallback = read_json_message(&mut websocket).unwrap().unwrap();
        assert_eq!(fallback["method"], "thread/resume");
        assert_eq!(fallback["params"], json!({ "threadId": "thread-prior" }));
        write_json_message(
            &mut websocket,
            &json!({
                "id": CONTROL_SUBSCRIBE_REQUEST_ID,
                "result": {
                    "thread": { "id": "thread-prior", "status": { "type": "idle" } }
                }
            }),
        )
        .unwrap();
        assert!(matches!(
            poll_json_message(&mut websocket).unwrap(),
            ControlRead::Timeout
        ));
    });

    let stream = UnixStream::connect(&socket).unwrap();
    let shutdown = stream.try_clone().unwrap();
    let websocket = initialize_control(stream)
        .unwrap()
        .expect("no stop raised in tests");
    let binding_path = tmp.path().join("state/binding.json");
    let control_state_path = tmp.path().join("state/control-state.json");
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let (tx, rx) = mpsc::channel();
    let (resume_ready_tx, resume_ready_rx) = mpsc::channel();
    let fallback_active = Arc::new(AtomicBool::new(false));
    let fallback_for_pump = fallback_active.clone();
    let runtime_for_pump = runtime.clone();
    let pump = thread::spawn(move || {
        pump_control(
            websocket,
            &binding_path,
            &control_state_path,
            &runtime_for_pump,
            Some(ControlResume {
                thread_id: "thread-prior",
                ready: resume_ready_rx,
                tui_loaded_timeout: TUI_LOADED_TIMEOUT,
                permission_overrides: Some(ResumePermissionOverrides {
                    approval_policy: Some("never".into()),
                    approvals_reviewer: None,
                    sandbox: Some("danger-full-access".into()),
                }),
                preload: false,
                preloaded: None,
            }),
            None,
            fallback_for_pump,
            tx,
        )
    });
    resume_ready_tx.send(()).unwrap();
    acknowledge_tui_thread_loaded(&rx);
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        ControlEvent::SafeFallbackActivated {
            cause: "resumePermissionProjectionRejected",
            ..
        }
    ));
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        ControlEvent::Bound
    ));
    assert!(fallback_active.load(Ordering::SeqCst));

    server.join().unwrap();
    let _ = shutdown.shutdown(Shutdown::Both);
    pump.join().unwrap();
}

/// A resumed thread still holds its context, and the app-server replays
/// `thread/tokenUsage/updated` to the newly attached connection — before the resume response,
/// which the binding handshake otherwise discards along with every other notification. The
/// construction that resumed this seat has already removed the predecessor's record, so a
/// dropped replay leaves a resumed-and-idle seat reading `null` against a full window with
/// nothing to correct it until its next model response.
#[test]
fn a_token_usage_replayed_before_the_resume_response_still_reaches_the_record() {
    let tmp = tempfile::tempdir().unwrap();
    let _stop_exclusive = stop_flag_tests();
    let config = delivery_config(tmp.path());
    let agent_dir = config.agent_dir.clone();
    let socket = tmp.path().join("server.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let agent_dir_for_server = agent_dir.clone();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut websocket = tungstenite::accept(stream).unwrap();
        assert_eq!(
            read_json_message(&mut websocket).unwrap().unwrap()["method"],
            "initialize"
        );
        write_json_message(
            &mut websocket,
            &json!({ "id": 0, "result": { "userAgent": "fake" } }),
        )
        .unwrap();
        assert_eq!(
            read_json_message(&mut websocket).unwrap().unwrap()["method"],
            "initialized"
        );
        let loaded = read_json_message(&mut websocket).unwrap().unwrap();
        assert_eq!(loaded["method"], "thread/loaded/list");
        write_json_message(
            &mut websocket,
            &json!({
                "id": CONTROL_TUI_LOADED_REQUEST_ID,
                "result": { "data": ["thread-prior"] }
            }),
        )
        .unwrap();
        let subscribe = read_json_message(&mut websocket).unwrap().unwrap();
        assert_eq!(subscribe["method"], "thread/resume");
        assert_eq!(subscribe["params"]["threadId"], "thread-prior");
        // The replay, ahead of the response the handshake is waiting for.
        write_json_message(
            &mut websocket,
            &json!({
                "method": "thread/tokenUsage/updated",
                "params": {
                    "threadId": "thread-prior",
                    "turnId": "turn-prior",
                    "tokenUsage": {
                        "last": { "totalTokens": 92_283 },
                        "total": { "totalTokens": 2_235_329 },
                        "modelContextWindow": 258_400
                    }
                }
            }),
        )
        .unwrap();
        write_json_message(
            &mut websocket,
            &json!({
                "id": CONTROL_SUBSCRIBE_REQUEST_ID,
                "result": {
                    "thread": { "id": "thread-prior", "status": { "type": "idle" } }
                }
            }),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while harness_context::read(&harness_context::harness_context_path(
            &agent_dir_for_server,
        ))
        .is_none()
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
    });

    let stream = UnixStream::connect(&socket).unwrap();
    let shutdown = stream.try_clone().unwrap();
    let websocket = initialize_control(stream)
        .unwrap()
        .expect("no stop raised in tests");
    let binding_path = tmp.path().join("state/binding.json");
    let control_state_path = tmp.path().join("state/control-state.json");
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let (tx, rx) = mpsc::channel();
    let (resume_ready_tx, resume_ready_rx) = mpsc::channel();
    let binding_for_pump = binding_path.clone();
    let control_state_for_pump = control_state_path.clone();
    let pump = thread::spawn(move || {
        pump_control(
            websocket,
            &binding_for_pump,
            &control_state_for_pump,
            &runtime,
            Some(ControlResume {
                thread_id: "thread-prior",
                ready: resume_ready_rx,
                tui_loaded_timeout: TUI_LOADED_TIMEOUT,
                permission_overrides: None,
                preload: false,
                preloaded: None,
            }),
            Some(config),
            Arc::new(AtomicBool::new(false)),
            tx,
        )
    });
    resume_ready_tx.send(()).unwrap();
    acknowledge_tui_thread_loaded(&rx);
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        ControlEvent::Bound
    ));
    server.join().unwrap();
    let _ = shutdown.shutdown(Shutdown::Both);
    pump.join().unwrap();

    let observed =
        context_record(&agent_dir).expect("the replayed reading never reached the record");
    assert_eq!(observed.used_percent, Some(33.0));
    assert_eq!(observed.used_tokens, Some(92_283));
    assert_eq!(observed.session_total_tokens, Some(2_235_329));
}

#[test]
fn tui_loaded_timeout_reports_the_specific_failure_before_outer_binding_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let _stop_exclusive = stop_flag_tests();
    let socket = tmp.path().join("server.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut websocket = tungstenite::accept(stream).unwrap();
        assert_eq!(
            read_json_message(&mut websocket).unwrap().unwrap()["method"],
            "initialize"
        );
        write_json_message(
            &mut websocket,
            &json!({ "id": 0, "result": { "userAgent": "fake" } }),
        )
        .unwrap();
        assert_eq!(
            read_json_message(&mut websocket).unwrap().unwrap()["method"],
            "initialized"
        );
        let loaded = read_json_message(&mut websocket).unwrap().unwrap();
        assert_eq!(loaded["method"], "thread/loaded/list");
        write_json_message(
            &mut websocket,
            &json!({
                "id": CONTROL_TUI_LOADED_REQUEST_ID,
                "result": { "data": [] }
            }),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(250));
    });

    let stream = UnixStream::connect(&socket).unwrap();
    let shutdown = stream.try_clone().unwrap();
    let websocket = initialize_control(stream)
        .unwrap()
        .expect("no stop raised in tests");
    let binding_path = tmp.path().join("state/binding.json");
    let control_state_path = tmp.path().join("state/control-state.json");
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let (tx, rx) = mpsc::channel();
    let (resume_ready_tx, resume_ready_rx) = mpsc::channel();
    let pump = thread::spawn(move || {
        pump_control(
            websocket,
            &binding_path,
            &control_state_path,
            &runtime,
            Some(ControlResume {
                thread_id: "thread-prior",
                ready: resume_ready_rx,
                tui_loaded_timeout: Duration::from_millis(50),
                permission_overrides: None,
                preload: false,
                preloaded: None,
            }),
            None,
            Arc::new(AtomicBool::new(false)),
            tx,
        )
    });
    resume_ready_tx.send(()).unwrap();
    let ControlEvent::Failed(error) = rx.recv_timeout(Duration::from_secs(10)).unwrap() else {
        panic!("inner TUI-loaded deadline did not report its specific failure");
    };
    assert!(
        error.contains(
            "controlled Codex TUI did not load preserved thread thread-prior before control resume"
        ),
        "unexpected control failure: {error}"
    );

    let _ = shutdown.shutdown(Shutdown::Both);
    pump.join().unwrap();
    server.join().unwrap();
}

#[test]
fn missing_saved_rollout_fails_without_rebinding_the_incarnation() {
    let tmp = tempfile::tempdir().unwrap();
    let _stop_exclusive = stop_flag_tests();
    let binding_path = tmp.path().join("state/binding.json");
    let control_state_path = tmp.path().join("state/control-state.json");
    let prior_runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let prior_binding = CodexThreadBinding::new(&prior_runtime, "thread-prior".into());
    atomic_json(&binding_path, &prior_binding).unwrap();

    let socket = tmp.path().join("server.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut websocket = tungstenite::accept(stream).unwrap();
        assert_eq!(
            read_json_message(&mut websocket).unwrap().unwrap()["method"],
            "initialize"
        );
        write_json_message(
            &mut websocket,
            &json!({ "id": 0, "result": { "userAgent": "fake" } }),
        )
        .unwrap();
        assert_eq!(
            read_json_message(&mut websocket).unwrap().unwrap()["method"],
            "initialized"
        );
        let loaded = read_json_message(&mut websocket).unwrap().unwrap();
        assert_eq!(loaded["method"], "thread/loaded/list");
        write_json_message(
            &mut websocket,
            &json!({
                "id": CONTROL_TUI_LOADED_REQUEST_ID,
                "result": { "data": ["thread-prior"] }
            }),
        )
        .unwrap();
        let resume = read_json_message(&mut websocket).unwrap().unwrap();
        assert_eq!(resume["method"], "thread/resume");
        assert_eq!(resume["params"]["threadId"], "thread-prior");
        write_json_message(
            &mut websocket,
            &json!({
                "id": CONTROL_SUBSCRIBE_REQUEST_ID,
                "error": {
                    "code": -32600,
                    "message": "no rollout found for thread id thread-prior"
                }
            }),
        )
        .unwrap();
    });

    let stream = UnixStream::connect(&socket).unwrap();
    let shutdown = stream.try_clone().unwrap();
    let websocket = initialize_control(stream)
        .unwrap()
        .expect("no stop raised in tests");
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let (tx, rx) = mpsc::channel();
    let (resume_ready_tx, resume_ready_rx) = mpsc::channel();
    let runtime_for_pump = runtime.clone();
    let binding_for_pump = binding_path.clone();
    let control_state_for_pump = control_state_path.clone();
    let pump = thread::spawn(move || {
        pump_control(
            websocket,
            &binding_for_pump,
            &control_state_for_pump,
            &runtime_for_pump,
            Some(ControlResume {
                thread_id: "thread-prior",
                ready: resume_ready_rx,
                tui_loaded_timeout: TUI_LOADED_TIMEOUT,
                permission_overrides: None,
                preload: false,
                preloaded: None,
            }),
            None,
            Arc::new(AtomicBool::new(false)),
            tx,
        )
    });
    resume_ready_tx.send(()).unwrap();
    acknowledge_tui_thread_loaded(&rx);
    let ControlEvent::Failed(error) = rx.recv_timeout(Duration::from_secs(10)).unwrap() else {
        panic!("missing saved rollout did not fail closed");
    };
    assert!(error.contains("saved Codex resume binding has no persisted rollout"));

    server.join().unwrap();
    let _ = shutdown.shutdown(Shutdown::Both);
    pump.join().unwrap();
    assert_eq!(
        serde_json::from_slice::<CodexThreadBinding>(&fs::read(&binding_path).unwrap()).unwrap(),
        prior_binding
    );
    assert!(!control_state_path.exists());
}

#[test]
fn a_binding_from_another_runtime_incarnation_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("binding.json");
    let prior = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let current = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    atomic_json(
        &path,
        &CodexThreadBinding::new(&prior, "thread-prior".into()),
    )
    .unwrap();
    assert_eq!(
        load_resume_thread(&path, "h.worker", "h.worker").unwrap(),
        Some("thread-prior".into()),
        "a validated prior binding may select resume but must not become current ownership"
    );
    let error = load_current_binding(&path, &current).unwrap_err();
    assert!(error.to_string().contains("different runtime incarnation"));
}

#[test]
fn residency_checkpoint_requires_and_preserves_the_exact_bound_thread() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    atomic_json(&state.join("runtime.json"), &runtime).unwrap();
    atomic_json(
        &state.join("binding.json"),
        &CodexThreadBinding::new(&runtime, "thread-prior".into()),
    )
    .unwrap();

    let checkpoint = checkpoint_residency(
        &state,
        "h.worker",
        "h.worker",
        crate::residency::Generation(1),
        crate::residency::Generation(2),
    )
    .unwrap();
    assert_eq!(checkpoint.thread_id(), "thread-prior");
    assert_eq!(
        required_residency_resume(
            &state,
            "h.worker",
            "h.worker",
            crate::residency::Generation(2),
            &["--model".into(), "gpt-test".into(), "boot".into()],
        )
        .unwrap(),
        "thread-prior"
    );

    atomic_json(
        &state.join("binding.json"),
        &CodexThreadBinding::new(&runtime, "thread-replaced".into()),
    )
    .unwrap();
    let error = required_residency_resume(
        &state,
        "h.worker",
        "h.worker",
        crate::residency::Generation(2),
        &["boot".into()],
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("changed after residency checkpoint")
    );
}

#[test]
fn residency_checkpoint_rejects_a_binding_from_a_prior_runtime_incarnation() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let prior = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let current = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    atomic_json(&state.join("runtime.json"), &current).unwrap();
    atomic_json(
        &state.join("binding.json"),
        &CodexThreadBinding::new(&prior, "thread-prior".into()),
    )
    .unwrap();

    let error = checkpoint_residency(
        &state,
        "h.worker",
        "h.worker",
        crate::residency::Generation(1),
        crate::residency::Generation(2),
    )
    .unwrap_err();
    assert!(error.to_string().contains("different runtime incarnation"));
    assert!(!state.join(RESIDENCY_CHECKPOINT_FILE).exists());
}

#[test]
fn residency_readiness_requires_the_exact_thread_under_the_expected_incarnation() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let prior = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let prior_binding = CodexThreadBinding::new(&prior, "thread-prior".into());
    atomic_json(&state.join("runtime.json"), &prior).unwrap();
    atomic_json(&state.join("binding.json"), &prior_binding).unwrap();
    checkpoint_residency(
        &state,
        "h.worker",
        "h.worker",
        crate::residency::Generation(1),
        crate::residency::Generation(2),
    )
    .unwrap();

    fs::remove_file(state.join("binding.json")).unwrap();
    assert!(
        !residency_ready(
            &state,
            "h.worker",
            "h.worker",
            crate::residency::Generation(2),
            "attempt-next",
        )
        .unwrap()
    );

    atomic_json(&state.join("binding.json"), &prior_binding).unwrap();
    let stale = residency_ready(
        &state,
        "h.worker",
        "h.worker",
        crate::residency::Generation(2),
        "attempt-next",
    )
    .unwrap_err();
    assert!(stale.to_string().contains("new runtime incarnation"));

    let foreign = CodexRuntime::fresh("h.other".into(), "h.other".into()).unwrap();
    atomic_json(
        &state.join("binding.json"),
        &CodexThreadBinding::new(&foreign, "thread-prior".into()),
    )
    .unwrap();
    let foreign = residency_ready(
        &state,
        "h.worker",
        "h.worker",
        crate::residency::Generation(2),
        "attempt-next",
    )
    .unwrap_err();
    assert!(foreign.to_string().contains("different agent runtime"));

    let stale_attempt = CodexRuntime::with_incarnation(
        "h.worker".into(),
        "h.worker".into(),
        "attempt-stale".into(),
    )
    .unwrap();
    atomic_json(
        &state.join("binding.json"),
        &CodexThreadBinding::new(&stale_attempt, "thread-other".into()),
    )
    .unwrap();
    let mismatched = residency_ready(
        &state,
        "h.worker",
        "h.worker",
        crate::residency::Generation(2),
        "attempt-next",
    )
    .unwrap_err();
    assert!(
        mismatched
            .to_string()
            .contains("checkpointed native thread")
    );

    atomic_json(
        &state.join("binding.json"),
        &CodexThreadBinding::new(&stale_attempt, "thread-prior".into()),
    )
    .unwrap();
    assert!(
        !residency_ready(
            &state,
            "h.worker",
            "h.worker",
            crate::residency::Generation(2),
            "attempt-next",
        )
        .unwrap(),
        "a stale prior-attempt binding became ready"
    );

    let replacement =
        CodexRuntime::with_incarnation("h.worker".into(), "h.worker".into(), "attempt-next".into())
            .unwrap();
    atomic_json(
        &state.join("binding.json"),
        &CodexThreadBinding::new(&replacement, "thread-prior".into()),
    )
    .unwrap();
    assert!(
        residency_ready(
            &state,
            "h.worker",
            "h.worker",
            crate::residency::Generation(2),
            "attempt-next",
        )
        .unwrap()
    );
    assert!(
        residency_ready(
            &state,
            "h.worker",
            "h.worker",
            crate::residency::Generation(3),
            "attempt-next",
        )
        .unwrap_err()
        .to_string()
        .contains("different generation")
    );
}

#[test]
fn mandatory_residency_resume_refuses_missing_or_authored_session_selection() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    atomic_json(&state.join("runtime.json"), &runtime).unwrap();
    let error = checkpoint_residency(
        &state,
        "h.worker",
        "h.worker",
        crate::residency::Generation(1),
        crate::residency::Generation(2),
    )
    .unwrap_err();
    assert!(error.to_string().contains("has no native thread binding"));

    atomic_json(
        &state.join("binding.json"),
        &CodexThreadBinding::new(&runtime, "thread-prior".into()),
    )
    .unwrap();
    checkpoint_residency(
        &state,
        "h.worker",
        "h.worker",
        crate::residency::Generation(1),
        crate::residency::Generation(2),
    )
    .unwrap();
    for authored in [
        vec!["resume".into(), "thread-other".into()],
        vec!["fork".into(), "thread-other".into()],
    ] {
        let error = required_residency_resume(
            &state,
            "h.worker",
            "h.worker",
            crate::residency::Generation(2),
            &authored,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("authored Codex session selection")
        );
    }
}

#[test]
fn residency_resume_rejects_missing_corrupt_foreign_and_stale_checkpoints() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    atomic_json(&state.join("runtime.json"), &runtime).unwrap();
    atomic_json(
        &state.join("binding.json"),
        &CodexThreadBinding::new(&runtime, "thread-prior".into()),
    )
    .unwrap();

    let missing = required_residency_resume(
        &state,
        "h.worker",
        "h.worker",
        crate::residency::Generation(2),
        &["boot".into()],
    )
    .unwrap_err();
    assert!(missing.to_string().contains("residency checkpoint"));

    checkpoint_residency(
        &state,
        "h.worker",
        "h.worker",
        crate::residency::Generation(1),
        crate::residency::Generation(2),
    )
    .unwrap();
    let stale = required_residency_resume(
        &state,
        "h.worker",
        "h.worker",
        crate::residency::Generation(3),
        &["boot".into()],
    )
    .unwrap_err();
    assert!(stale.to_string().contains("different generation"));

    let foreign_runtime = CodexRuntime::fresh("h.other".into(), "h.other".into()).unwrap();
    atomic_json(
        &state.join(RESIDENCY_CHECKPOINT_FILE),
        &CodexResidencyCheckpoint {
            schema: RESIDENCY_CHECKPOINT_SCHEMA.into(),
            source_generation: crate::residency::Generation(1),
            resume_generation: crate::residency::Generation(2),
            binding: CodexThreadBinding::new(&foreign_runtime, "thread-prior".into()),
        },
    )
    .unwrap();
    let foreign = required_residency_resume(
        &state,
        "h.worker",
        "h.worker",
        crate::residency::Generation(2),
        &["boot".into()],
    )
    .unwrap_err();
    assert!(foreign.to_string().contains("different agent runtime"));

    fs::write(state.join(RESIDENCY_CHECKPOINT_FILE), b"{").unwrap();
    assert!(
        required_residency_resume(
            &state,
            "h.worker",
            "h.worker",
            crate::residency::Generation(2),
            &["boot".into()],
        )
        .is_err(),
        "a corrupt residency checkpoint was accepted"
    );
}

#[test]
fn mandatory_residency_failure_does_not_spawn_a_provider_child() {
    let _stop_exclusive = stop_flag_tests();
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("provider-started");
    let codex = tmp.path().join("codex");
    fs::write(
        &codex,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();

    let error = run_controlled_residency_attempt(
        tmp.path(),
        "h.worker".into(),
        "h.worker".into(),
        vec![codex.display().to_string(), "boot".into()],
        crate::residency::Generation(2),
        "attempt-test".into(),
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("residency checkpoint"),
        "{error:#}"
    );
    assert!(
        !marker.exists(),
        "a provider process started before mandatory resume validation"
    );
}

#[test]
fn a_native_seat_restart_retires_the_old_thread_binding() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("binding.json");
    let prior = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    atomic_json(
        &path,
        &CodexThreadBinding::new(&prior, "thread-prior".into()),
    )
    .unwrap();

    let selected = select_resume_thread(&path, "h.worker", "h.worker", false).unwrap();
    assert_eq!(selected, None);
    assert!(
        !path.exists(),
        "the old binding must not make this seat ready"
    );
    let prepared = prepare_controlled_launch_args("unix:///server.sock", &[], selected.as_deref());
    assert!(!prepared.tui_args.iter().any(|arg| arg == "resume"));
    assert!(prepared.expected_resume.is_none());
}

#[test]
fn codex_control_accepts_a_frame_larger_than_sixteen_mib() {
    let _stop_exclusive = stop_flag_tests();
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("server.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let payload = "x".repeat(16 * 1024 * 1024 + 4096);
    let expected_size = payload.len();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut websocket = tungstenite::accept(stream).unwrap();
        let initialize = read_json_message(&mut websocket).unwrap().unwrap();
        assert_eq!(initialize["method"], "initialize");
        write_json_message(&mut websocket, &json!({ "id": 0, "result": {} })).unwrap();
        let initialized = read_json_message(&mut websocket).unwrap().unwrap();
        assert_eq!(initialized["method"], "initialized");
        write_json_message(
            &mut websocket,
            &json!({ "method": "thread/read", "params": { "payload": payload } }),
        )
        .unwrap();
    });

    let stream = UnixStream::connect(&socket).unwrap();
    let mut websocket = initialize_control(stream).unwrap().unwrap();
    let ControlRead::Message(message) = poll_json_message(&mut websocket).unwrap() else {
        panic!("large control frame was not received");
    };
    assert_eq!(
        message["params"]["payload"].as_str().unwrap().len(),
        expected_size
    );
    server.join().unwrap();
}

#[test]
fn watcher_holds_without_an_exact_turn_and_tracks_one_unmatched_lifecycle() {
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let mut state = CodexControlState::new(&runtime, "thread-main".into());

    assert!(
        state
            .observe(&json!({
                "method": "thread/status/changed",
                "params": {
                    "threadId": "thread-main",
                    "status": { "type": "active", "activeFlags": [] }
                }
            }))
            .unwrap()
    );
    assert_eq!(
        state.observed(),
        &CodexObservedState::Held {
            reason: CodexHoldReason::ActiveWithoutTurn,
            turn_id: None,
        }
    );

    assert!(
        state
            .observe(&json!({
                "method": "turn/started",
                "params": {
                    "threadId": "thread-main",
                    "turn": { "id": "turn-1" }
                }
            }))
            .unwrap()
    );
    assert_eq!(
        state.observed(),
        &CodexObservedState::Active {
            turn_id: "turn-1".into()
        }
    );

    assert!(
        !state
            .observe(&json!({
                "method": "turn/started",
                "params": {
                    "threadId": "thread-other",
                    "turn": { "id": "turn-other" }
                }
            }))
            .unwrap()
    );
    assert_eq!(
        state.observed(),
        &CodexObservedState::Active {
            turn_id: "turn-1".into()
        }
    );

    assert!(
        state
            .observe(&json!({
                "method": "turn/completed",
                "params": {
                    "threadId": "thread-main",
                    "turn": { "id": "turn-1" }
                }
            }))
            .unwrap()
    );
    assert_eq!(state.observed(), &CodexObservedState::Idle);
}

#[test]
fn a_typed_item_recovers_the_startup_turn_for_immediate_steering() {
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let mut state = CodexControlState::new(&runtime, "thread-main".into());
    state.subscribed = true;
    state
        .observe(&json!({
            "method": "thread/status/changed",
            "params": {
                "threadId": "thread-main",
                "status": { "type": "active", "activeFlags": [] }
            }
        }))
        .unwrap();
    assert!(matches!(
        state.observed(),
        CodexObservedState::Held {
            reason: CodexHoldReason::ActiveWithoutTurn,
            ..
        }
    ));

    assert!(
        state
            .observe(&json!({
                "method": "item/started",
                "params": {
                    "threadId": "thread-main",
                    "turnId": "turn-recovered",
                    "item": { "type": "commandExecution" }
                }
            }))
            .unwrap()
    );
    assert_eq!(
        state.observed(),
        &CodexObservedState::Active {
            turn_id: "turn-recovered".into(),
        }
    );

    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    message::send_to_inbox(&config.inbox, "h.sender", None, None, &[], "queued").unwrap();
    let mut delivery = inbox_delivery(tmp.path(), config);
    let request = delivery
        .maybe_request(&state)
        .unwrap()
        .expect("the staged message should steer the recovered turn");
    assert_eq!(request["method"], "turn/steer");
    assert_eq!(request["params"]["expectedTurnId"], "turn-recovered");
}

#[test]
fn delivery_reads_bounded_status_and_fences_the_subscribed_turn_before_steering() {
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    message::send_to_inbox(&config.inbox, "h.sender", None, None, &[], "queued").unwrap();
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let mut delivery = CodexInboxDelivery::new(
        config,
        tmp.path().join("state").join(delivery_ledger::LEDGER_FILE),
        runtime.clone(),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let mut state = CodexControlState::new(&runtime, "thread-main".into());
    state.subscribed = true;
    state.observed = CodexObservedState::Active {
        turn_id: "turn-current".into(),
    };

    let read = delivery
        .maybe_snapshot_request(&state)
        .unwrap()
        .expect("an unread inbox head requires a fresh thread snapshot");
    assert_eq!(read["method"], "thread/read");
    assert_eq!(read["params"]["includeTurns"], false);
    let read_id = read["id"].clone();
    assert!(
        delivery
            .accept_snapshot_response(
                &json!({
                    "id": read_id,
                    "result": {
                        "thread": {
                            "id": "thread-main",
                            "status": { "type": "active", "activeFlags": [] },
                            "turns": []
                        }
                    }
                }),
                &mut state,
            )
            .unwrap()
    );
    let steer = delivery
        .maybe_request(&state)
        .unwrap()
        .expect("the verified current turn is steerable");
    assert_eq!(steer["method"], "turn/steer");
    assert_eq!(steer["params"]["expectedTurnId"], "turn-current");
    assert_eq!(
        state.observed(),
        &CodexObservedState::Active {
            turn_id: "turn-current".into()
        }
    );
}

#[test]
fn idle_delivery_retries_snapshot_errors_with_fresh_ids_then_falls_back() {
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    message::send_to_inbox(&config.inbox, "h.sender", None, None, &[], "queued").unwrap();
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let mut delivery = CodexInboxDelivery::new(
        config,
        tmp.path().join("state").join(delivery_ledger::LEDGER_FILE),
        runtime.clone(),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let mut state = CodexControlState::new(&runtime, "thread-main".into());
    state.subscribed = true;
    state.observed = CodexObservedState::Idle;

    for attempt in 0..SNAPSHOT_REQUEST_ATTEMPTS {
        let read = delivery
            .maybe_snapshot_request(&state)
            .unwrap()
            .expect("each bounded retry issues a request");
        assert_eq!(
            read["id"],
            Value::from(FIRST_DELIVERY_REQUEST_ID + u64::from(attempt)),
            "every retry must use a fresh JSON-RPC ID"
        );
        assert!(
            delivery
                .accept_snapshot_response(
                    &json!({
                        "id": read["id"],
                        "error": { "code": -32000, "message": "temporarily unavailable" }
                    }),
                    &mut state,
                )
                .unwrap()
        );
    }

    assert_eq!(delivery.maybe_snapshot_request(&state).unwrap(), None);
    let request = delivery
        .maybe_request(&state)
        .unwrap()
        .expect("bounded read errors must not permanently fence known-idle delivery");
    assert_eq!(request["method"], "turn/start");
    assert_eq!(
        request["id"],
        Value::from(FIRST_DELIVERY_REQUEST_ID + u64::from(SNAPSHOT_REQUEST_ATTEMPTS))
    );
}

#[test]
fn idle_delivery_expires_missing_snapshot_responses_and_ignores_late_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    message::send_to_inbox(&config.inbox, "h.sender", None, None, &[], "queued").unwrap();
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let mut delivery = CodexInboxDelivery::new(
        config,
        tmp.path().join("state").join(delivery_ledger::LEDGER_FILE),
        runtime.clone(),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let mut state = CodexControlState::new(&runtime, "thread-main".into());
    state.subscribed = true;
    state.observed = CodexObservedState::Idle;

    let first = delivery
        .maybe_snapshot_request(&state)
        .unwrap()
        .expect("the first snapshot request is issued");
    for attempt in 1..SNAPSHOT_REQUEST_ATTEMPTS {
        delivery.pending_snapshot.as_mut().unwrap().requested_at = Instant::now()
            .checked_sub(SNAPSHOT_REQUEST_TIMEOUT + Duration::from_millis(1))
            .unwrap();
        let retry = delivery
            .maybe_snapshot_request(&state)
            .unwrap()
            .expect("an expired request is replaced within the retry bound");
        assert_eq!(
            retry["id"],
            Value::from(FIRST_DELIVERY_REQUEST_ID + u64::from(attempt))
        );
        assert_ne!(retry["id"], first["id"]);
        assert!(
            !delivery
                .accept_snapshot_response(
                    &json!({
                        "id": first["id"],
                        "result": {
                            "thread": {
                                "id": "thread-main",
                                "status": { "type": "idle" },
                                "turns": []
                            }
                        }
                    }),
                    &mut state,
                )
                .unwrap(),
            "a late response for an expired request cannot satisfy the fresh request"
        );
    }
    delivery.pending_snapshot.as_mut().unwrap().requested_at = Instant::now()
        .checked_sub(SNAPSHOT_REQUEST_TIMEOUT + Duration::from_millis(1))
        .unwrap();

    assert_eq!(
        delivery.maybe_snapshot_request(&state).unwrap(),
        None,
        "the final timeout clears the snapshot fence instead of issuing an unbounded retry"
    );
    let request = delivery
        .maybe_request(&state)
        .unwrap()
        .expect("missing read responses must not permanently fence known-idle delivery");
    assert_eq!(request["method"], "turn/start");
    assert_eq!(
        request["id"],
        Value::from(FIRST_DELIVERY_REQUEST_ID + u64::from(SNAPSHOT_REQUEST_ATTEMPTS))
    );
}

#[test]
fn the_transcript_snapshot_recovers_and_clears_the_active_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let transcript = tmp.path().join("rollout-thread-main.jsonl");
    fs::write(
        &transcript,
        concat!(
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-old"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-old"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-live"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"reasoning","internal_chat_message_metadata_passthrough":{"turn_id":"turn-live"}}}"#,
            "\n",
        ),
    )
    .unwrap();
    assert_eq!(
        active_turn_from_codex_transcript(&transcript).unwrap(),
        Some("turn-live".into())
    );

    fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap()
        .write_all(
            b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"turn_aborted\",\"turn_id\":\"turn-live\"}}\n",
        )
        .unwrap();
    assert_eq!(
        active_turn_from_codex_transcript(&transcript).unwrap(),
        None
    );
}

#[test]
fn transcript_proves_only_the_latest_matching_failed_completion() {
    let started = |id| json!({"type":"event_msg","payload":{"type":"task_started","turn_id":id}});
    let completed = |id, error: Option<&str>| json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":id,"error":error}});
    assert_eq!(
        failed_completed_turn_from_codex_frames(&[
            started("old"),
            completed("old", Some("unauthorized")),
        ]),
        Some("old".into())
    );
    assert_eq!(
        failed_completed_turn_from_codex_frames(&[
            started("old"),
            completed("old", Some("unauthorized")),
            started("new"),
        ]),
        None,
        "a newer live turn cannot be unblocked by an older failed turn"
    );
    assert_eq!(
        failed_completed_turn_from_codex_frames(&[
            started("one"),
            completed("other", Some("unauthorized")),
        ]),
        None,
        "the completion must match the started turn"
    );
    assert_eq!(
        failed_completed_turn_from_codex_frames(&[started("one"), completed("one", None),]),
        None,
        "a successful completion cannot prove a terminal system error"
    );
}

#[test]
fn a_long_active_turn_recovers_from_recent_typed_transcript_evidence() {
    let tmp = tempfile::tempdir().unwrap();
    let transcript = tmp.path().join("rollout-thread-main.jsonl");
    let mut content = String::from(
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-live\"}}\n",
    );
    content.push_str(&" ".repeat(TRANSCRIPT_TURN_RECOVERY_BYTES as usize));
    content.push('\n');
    content.push_str(
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"turn_id\":\"turn-live\"}}\n",
    );
    fs::write(&transcript, content).unwrap();
    assert_eq!(
        active_turn_from_codex_transcript(&transcript).unwrap(),
        Some("turn-live".into()),
        "the task-start frame is outside the bounded tail"
    );
    fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap()
        .write_all(
            b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"turn-live\"}}\n",
        )
        .unwrap();
    assert_eq!(
        active_turn_from_codex_transcript(&transcript).unwrap(),
        None
    );
}

#[test]
fn a_successful_snapshot_without_turn_allows_transcript_recovery_and_steer() {
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    message::send_to_inbox(&config.inbox, "h.sender", None, None, &[], "queued").unwrap();
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let mut delivery = CodexInboxDelivery::new(
        config,
        tmp.path().join("state").join(delivery_ledger::LEDGER_FILE),
        runtime.clone(),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let mut state = CodexControlState::new(&runtime, "thread-main".into());
    state.subscribed = true;
    let read = delivery.maybe_snapshot_request(&state).unwrap().unwrap();
    delivery
        .accept_snapshot_response(
            &json!({
                "id": read["id"],
                "result": {"thread": {
                    "id": "thread-main",
                    "status": {"type": "active", "activeFlags": []},
                    "turns": []
                }}
            }),
            &mut state,
        )
        .unwrap();
    assert!(delivery.transcript_recovery_due());
    assert!(delivery.maybe_request(&state).unwrap().is_none());
    state.observe_turn_evidence("turn-live");
    delivery.accept_transcript_recovery(state.observed.clone());
    let request = delivery.maybe_request(&state).unwrap().unwrap();
    assert_eq!(request["method"], "turn/steer");
    assert_eq!(request["params"]["expectedTurnId"], "turn-live");
}

#[test]
fn watcher_holds_review_compaction_and_conflicting_turns_until_safe() {
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let mut state = CodexControlState::new(&runtime, "thread-main".into());

    state
        .observe(&json!({
            "method": "turn/started",
            "params": { "threadId": "thread-main", "turn": { "id": "turn-1" } }
        }))
        .unwrap();
    state
        .observe(&json!({
            "method": "item/started",
            "params": {
                "threadId": "thread-main",
                "turnId": "turn-1",
                "item": { "type": "enteredReviewMode" }
            }
        }))
        .unwrap();
    assert_eq!(
        state.observed(),
        &CodexObservedState::Held {
            reason: CodexHoldReason::Review,
            turn_id: Some("turn-1".into()),
        }
    );

    state
        .observe(&json!({
            "method": "thread/status/changed",
            "params": {
                "threadId": "thread-main",
                "status": { "type": "active", "activeFlags": [] }
            }
        }))
        .unwrap();
    assert!(matches!(
        state.observed(),
        CodexObservedState::Held {
            reason: CodexHoldReason::Review,
            ..
        }
    ));

    state
        .observe(&json!({
            "method": "turn/started",
            "params": { "threadId": "thread-main", "turn": { "id": "turn-2" } }
        }))
        .unwrap();
    assert_eq!(
        state.observed(),
        &CodexObservedState::Held {
            reason: CodexHoldReason::Review,
            turn_id: Some("turn-2".into()),
        }
    );

    // Codex can complete the preparatory review item after the reviewer turn starts. That
    // duplicate review event keeps the typed hold bound to the newer turn.
    assert!(
        !state
            .observe(&json!({
                "method": "item/completed",
                "params": {
                    "threadId": "thread-main",
                    "turnId": "turn-1",
                    "item": { "type": "enteredReviewMode" }
                }
            }))
            .unwrap()
    );
    assert_eq!(
        state.observed(),
        &CodexObservedState::Held {
            reason: CodexHoldReason::Review,
            turn_id: Some("turn-2".into()),
        }
    );

    // The review hold also survives the stale turn completion. Only an idle thread releases
    // it.
    assert!(
        !state
            .observe(&json!({
                "method": "turn/completed",
                "params": { "threadId": "thread-main", "turn": { "id": "turn-1" } }
            }))
            .unwrap()
    );
    assert_eq!(
        state.observed(),
        &CodexObservedState::Held {
            reason: CodexHoldReason::Review,
            turn_id: Some("turn-2".into()),
        }
    );

    state
        .observe(&json!({
            "method": "thread/status/changed",
            "params": { "threadId": "thread-main", "status": { "type": "idle" } }
        }))
        .unwrap();
    assert_eq!(state.observed(), &CodexObservedState::Idle);

    // A real review can start its reviewer turn before Codex reports the preparatory turn's
    // typed review item. The typed non-steerable event refines that generic conflict.
    state
        .observe(&json!({
            "method": "turn/started",
            "params": { "threadId": "thread-main", "turn": { "id": "turn-late-1" } }
        }))
        .unwrap();
    state
        .observe(&json!({
            "method": "turn/started",
            "params": { "threadId": "thread-main", "turn": { "id": "turn-late-2" } }
        }))
        .unwrap();
    assert!(matches!(
        state.observed(),
        CodexObservedState::Held {
            reason: CodexHoldReason::ConflictingTurn,
            ..
        }
    ));
    state
        .observe(&json!({
            "method": "item/started",
            "params": {
                "threadId": "thread-main",
                "turnId": "turn-late-1",
                "item": { "type": "enteredReviewMode" }
            }
        }))
        .unwrap();
    assert_eq!(
        state.observed(),
        &CodexObservedState::Held {
            reason: CodexHoldReason::Review,
            turn_id: Some("turn-late-1".into()),
        }
    );
    state
        .observe(&json!({
            "method": "thread/status/changed",
            "params": { "threadId": "thread-main", "status": { "type": "idle" } }
        }))
        .unwrap();

    state
        .observe(&json!({
            "method": "turn/started",
            "params": { "threadId": "thread-main", "turn": { "id": "turn-3" } }
        }))
        .unwrap();
    state
        .observe(&json!({
            "method": "item/completed",
            "params": {
                "threadId": "thread-main",
                "turnId": "turn-3",
                "item": { "type": "contextCompaction" }
            }
        }))
        .unwrap();
    assert_eq!(
        state.observed(),
        &CodexObservedState::Held {
            reason: CodexHoldReason::Compaction,
            turn_id: Some("turn-3".into()),
        }
    );
    assert!(
        !state
            .observe(&json!({
                "method": "turn/completed",
                "params": { "threadId": "thread-main", "turn": { "id": "turn-3" } }
            }))
            .unwrap()
    );
    assert!(matches!(
        state.observed(),
        CodexObservedState::Held {
            reason: CodexHoldReason::Compaction,
            ..
        }
    ));
}

#[test]
fn exiting_review_mode_mid_turn_restores_the_steerable_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    let filename =
        message::send_to_inbox(&config.inbox, "h.sender", Some("held"), None, &[], "body").unwrap();
    let mut delivery = inbox_delivery(tmp.path(), config.clone());

    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let mut state = CodexControlState::new(&runtime, "thread-main".into());
    state.subscribed = true;

    // An inline review runs as its own turn on the reviewed thread, so the hold binds to the
    // reviewer turn that `exitedReviewMode` later reports.
    state
        .observe(&json!({
            "method": "turn/started",
            "params": { "threadId": "thread-main", "turn": { "id": "turn-review" } }
        }))
        .unwrap();
    state
        .observe(&json!({
            "method": "item/started",
            "params": {
                "threadId": "thread-main",
                "turnId": "turn-review",
                "item": { "type": "enteredReviewMode", "id": "item-1", "review": "review" }
            }
        }))
        .unwrap();
    assert_eq!(
        state.observed(),
        &CodexObservedState::Held {
            reason: CodexHoldReason::Review,
            turn_id: Some("turn-review".into()),
        }
    );
    assert_eq!(delivery.maybe_request(&state).unwrap(), None);

    // Review ends while the turn keeps running: the typed exit item is the only signal, and it
    // must restore the exact turn the hold carried instead of waiting for the next idle.
    assert!(
        state
            .observe(&json!({
                "method": "item/started",
                "params": {
                    "threadId": "thread-main",
                    "turnId": "turn-review",
                    "item": { "type": "exitedReviewMode", "id": "item-2", "review": "review" }
                }
            }))
            .unwrap()
    );
    assert_eq!(
        state.observed(),
        &CodexObservedState::Active {
            turn_id: "turn-review".into(),
        }
    );

    // Codex reports both lifecycle edges of the same item; the second one changes nothing.
    assert!(
        !state
            .observe(&json!({
                "method": "item/completed",
                "params": {
                    "threadId": "thread-main",
                    "turnId": "turn-review",
                    "item": { "type": "exitedReviewMode", "id": "item-2", "review": "review" }
                }
            }))
            .unwrap()
    );
    assert_eq!(
        state.observed(),
        &CodexObservedState::Active {
            turn_id: "turn-review".into(),
        }
    );

    // The payoff: native delivery steers the still-running turn instead of waiting for idle.
    let request = delivery.maybe_request(&state).unwrap().unwrap();
    assert_eq!(request["method"], "turn/steer");
    assert_eq!(request["params"]["threadId"], "thread-main");
    assert_eq!(request["params"]["expectedTurnId"], "turn-review");
    assert!(config.inbox.join(&filename).is_file());
}

#[test]
fn delivery_irrelevant_items_and_foreign_turn_review_exits_keep_the_observed_state() {
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let mut state = CodexControlState::new(&runtime, "thread-main".into());
    state
        .observe(&json!({
            "method": "turn/started",
            "params": { "threadId": "thread-main", "turn": { "id": "turn-1" } }
        }))
        .unwrap();

    // Most item types say nothing about steerability. They are ignored on purpose, not by
    // omission: the observed state and the changed flag both stay put.
    for item_type in ["agentMessage", "commandExecution", "webSearch"] {
        assert!(
            !state
                .observe(&json!({
                    "method": "item/completed",
                    "params": {
                        "threadId": "thread-main",
                        "turnId": "turn-1",
                        "item": { "type": item_type, "id": "item-1" }
                    }
                }))
                .unwrap()
        );
        assert_eq!(
            state.observed(),
            &CodexObservedState::Active {
                turn_id: "turn-1".into(),
            }
        );
    }

    // A review exit reporting a turn the hold does not carry proves nothing about the held
    // turn, so the hold survives exactly as it did before typed exits were observed.
    state.observed = CodexObservedState::Held {
        reason: CodexHoldReason::Review,
        turn_id: Some("turn-2".into()),
    };
    let stale_exit = json!({
        "method": "item/completed",
        "params": {
            "threadId": "thread-main",
            "turnId": "turn-1",
            "item": { "type": "exitedReviewMode", "id": "item-2", "review": "review" }
        }
    });
    assert!(!state.observe(&stale_exit).unwrap());
    assert_eq!(
        state.observed(),
        &CodexObservedState::Held {
            reason: CodexHoldReason::Review,
            turn_id: Some("turn-2".into()),
        }
    );

    // A review exit never invents a turn on an idle thread and never releases another hold.
    for observed in [
        CodexObservedState::Idle,
        CodexObservedState::AwaitingStatus,
        CodexObservedState::Held {
            reason: CodexHoldReason::Compaction,
            turn_id: Some("turn-1".into()),
        },
        CodexObservedState::Held {
            reason: CodexHoldReason::ConflictingTurn,
            turn_id: None,
        },
    ] {
        state.observed = observed.clone();
        assert!(
            !state
                .observe(&json!({
                    "method": "item/started",
                    "params": {
                        "threadId": "thread-main",
                        "turnId": "turn-1",
                        "item": {
                            "type": "exitedReviewMode",
                            "id": "item-3",
                            "review": "review"
                        }
                    }
                }))
                .unwrap()
        );
        assert_eq!(state.observed(), &observed);
    }
}

#[test]
fn an_unclassified_item_holds_until_the_next_idle_status() {
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let mut state = CodexControlState::new(&runtime, "thread-main".into());
    state
        .observe(&json!({
            "method": "turn/started",
            "params": { "threadId": "thread-main", "turn": { "id": "turn-1" } }
        }))
        .unwrap();

    assert!(
        state
            .observe(&json!({
                "method": "item/completed",
                "params": {
                    "threadId": "thread-main",
                    "turnId": "turn-1",
                    "item": { "type": "futureBlockingItem", "id": "item-1" }
                }
            }))
            .unwrap()
    );
    assert!(matches!(
        state.observed(),
        CodexObservedState::Held {
            reason: CodexHoldReason::UnknownProtocol,
            turn_id: Some(turn_id),
        } if turn_id == "turn-1"
    ));

    assert!(
        state
            .observe(&json!({
                "method": "thread/status/changed",
                "params": {
                    "threadId": "thread-main",
                    "status": { "type": "idle" }
                }
            }))
            .unwrap()
    );
    assert_eq!(state.observed(), &CodexObservedState::Idle);
}

#[test]
fn an_unclassified_server_request_holds_until_the_next_idle_status() {
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let mut state = CodexControlState::new(&runtime, "thread-main".into());
    state
        .observe(&json!({
            "method": "turn/started",
            "params": { "threadId": "thread-main", "turn": { "id": "turn-1" } }
        }))
        .unwrap();

    assert!(
        !state
            .observe(&json!({
                "id": 1,
                "method": "item/commandExecution/requestApproval",
                "params": {}
            }))
            .unwrap()
    );
    assert!(matches!(
        state.observed(),
        CodexObservedState::Active { .. }
    ));

    assert!(
        state
            .observe(&json!({
                "id": 2,
                "method": "future/request",
                "params": {}
            }))
            .unwrap()
    );
    assert_eq!(
        state.observed(),
        &CodexObservedState::Held {
            reason: CodexHoldReason::UnknownProtocol,
            turn_id: Some("turn-1".into()),
        }
    );

    assert!(
        state
            .observe(&json!({
                "method": "thread/status/changed",
                "params": {
                    "threadId": "thread-main",
                    "status": { "type": "idle" }
                }
            }))
            .unwrap()
    );
    assert_eq!(state.observed(), &CodexObservedState::Idle);
}

#[test]
fn an_errored_turn_completes_into_the_named_error_not_a_conflicting_turn() {
    // Replays the captured terminal-error ordering (#264): a usage limit emits
    // `thread/status/changed -> systemError` immediately before the failed turn's
    // `turn/completed`. That completion reports one turn's lifecycle and carries no thread
    // status, so it is not evidence the thread recovered, and it is not evidence of a second
    // live turn either. The honest resolution is the condition the thread itself reported.
    for (status, reason) in [
        ("systemError", CodexHoldReason::SystemError),
        ("notLoaded", CodexHoldReason::NotLoaded),
    ] {
        let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
        let mut state = CodexControlState::new(&runtime, "thread-main".into());
        state.subscribed = true;
        state
            .observe(&json!({
                "method": "turn/started",
                "params": { "threadId": "thread-main", "turn": { "id": "turn-1" } }
            }))
            .unwrap();
        assert!(
            state
                .observe(&json!({
                    "method": "thread/status/changed",
                    "params": { "threadId": "thread-main", "status": { "type": status } }
                }))
                .unwrap()
        );
        assert_eq!(
            state.observed(),
            &CodexObservedState::Held {
                reason,
                turn_id: None
            }
        );

        // Completion makes a reported system error terminal. It preserves `notLoaded`, whose
        // owner is the later thread status that proves the thread loaded again.
        let changed = state
            .observe(&json!({
                "method": "turn/completed",
                "params": { "threadId": "thread-main", "turn": { "id": "turn-1" } }
            }))
            .unwrap();

        if reason == CodexHoldReason::SystemError {
            assert!(changed);
            assert_eq!(
                state.observed(),
                &CodexObservedState::TerminalError {
                    reason: CodexTerminalError::SystemError,
                }
            );
        } else {
            assert!(!changed);
            assert_eq!(
                state.observed(),
                &CodexObservedState::Held {
                    reason,
                    turn_id: None,
                }
            );
        }

        let tmp = tempfile::tempdir().unwrap();
        let config = delivery_config(tmp.path());
        let filename =
            message::send_to_inbox(&config.inbox, "h.sender", Some("held"), None, &[], "body")
                .unwrap();
        let mut delivery = inbox_delivery(tmp.path(), config.clone());
        if reason == CodexHoldReason::SystemError {
            let request = delivery
                .maybe_request(&state)
                .unwrap()
                .expect("a terminal system error must permit the next turn");
            assert_eq!(request["method"], "turn/start");
        } else {
            assert_eq!(delivery.maybe_request(&state).unwrap(), None);
            assert!(
                state
                    .observe(&json!({
                        "method": "thread/status/changed",
                        "params": {
                            "threadId": "thread-main",
                            "status": { "type": "idle" }
                        }
                    }))
                    .unwrap()
            );
            assert_eq!(state.observed(), &CodexObservedState::Idle);
            assert!(delivery.maybe_request(&state).unwrap().is_some());
        }
        assert!(config.inbox.join(&filename).is_file());
    }

    // The next provider turn replaces the terminal diagnostic with the exact live turn.
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let mut state = CodexControlState::new(&runtime, "thread-main".into());
    state.subscribed = true;
    for message in [
        json!({
            "method": "turn/started",
            "params": { "threadId": "thread-main", "turn": { "id": "turn-1" } }
        }),
        json!({
            "method": "thread/status/changed",
            "params": { "threadId": "thread-main", "status": { "type": "systemError" } }
        }),
        json!({
            "method": "turn/completed",
            "params": { "threadId": "thread-main", "turn": { "id": "turn-1" } }
        }),
    ] {
        state.observe(&message).unwrap();
    }
    assert_eq!(
        state.observed(),
        &CodexObservedState::TerminalError {
            reason: CodexTerminalError::SystemError,
        }
    );
    state
        .observe(&json!({
            "method": "thread/status/changed",
            "params": {
                "threadId": "thread-main",
                "status": { "type": "active", "activeFlags": [] }
            }
        }))
        .unwrap();
    assert_eq!(
        state.observed(),
        &CodexObservedState::Held {
            reason: CodexHoldReason::ActiveWithoutTurn,
            turn_id: None,
        }
    );
    state
        .observe(&json!({
            "method": "turn/started",
            "params": { "threadId": "thread-main", "turn": { "id": "turn-2" } }
        }))
        .unwrap();
    assert_eq!(
        state.observed(),
        &CodexObservedState::Active {
            turn_id: "turn-2".into(),
        }
    );
}

#[test]
fn persisted_control_state_is_bound_to_the_exact_runtime_incarnation() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("control-state.json");
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let binding = CodexThreadBinding::new(&runtime, "thread-main".into());
    let mut state = CodexControlState::new(&runtime, "thread-main".into());
    state.observed = CodexObservedState::Active {
        turn_id: "turn-1".into(),
    };
    atomic_json(&path, &state).unwrap();
    let persisted: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(persisted["observed"]["turnId"], "turn-1");
    assert!(persisted["observed"].get("turn_id").is_none());

    assert_eq!(
        load_current_control_state(&path, &runtime, &binding)
            .unwrap()
            .unwrap(),
        state
    );

    let replacement = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let replacement_binding = CodexThreadBinding::new(&replacement, "thread-main".into());
    let error = load_current_control_state(&path, &replacement, &replacement_binding).unwrap_err();
    assert!(error.to_string().contains("different runtime binding"));
}

#[test]
fn subscription_waits_for_a_rollout_without_claiming_success() {
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let mut state = CodexControlState::new(&runtime, "thread-main".into());
    let acceptance = state
        .accept_subscription(&json!({
            "id": CONTROL_SUBSCRIBE_REQUEST_ID,
            "error": {
                "code": -32600,
                "message": "no rollout found for thread id thread-main"
            }
        }))
        .unwrap();

    assert!(matches!(acceptance, SubscriptionAcceptance::Deferred));
    assert!(!state.subscribed());
    assert_eq!(state.observed(), &CodexObservedState::AwaitingStatus);
}

#[test]
fn app_server_receives_only_its_supported_global_configuration() {
    let authored = vec![
        "-c".into(),
        "projects={\"/workspace\"={trust_level=\"trusted\"}}".into(),
        "--model".into(),
        "gpt-test".into(),
        "--enable".into(),
        "one".into(),
        "--disable=two".into(),
        "--strict-config".into(),
        "--dangerously-bypass-approvals-and-sandbox".into(),
        "--dangerously-bypass-hook-trust".into(),
        "boot".into(),
    ];

    assert_eq!(
        controlled_app_server_args("unix:///server.sock", &authored).unwrap(),
        [
            "app-server",
            "-c",
            "projects={\"/workspace\"={trust_level=\"trusted\"}}",
            "--enable",
            "one",
            "--disable=two",
            "--strict-config",
            "--listen",
            "unix:///server.sock",
        ]
    );
}

#[test]
fn remote_resume_projects_exact_hook_hashes_without_persisted_state() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = fs::canonicalize(tmp.path()).unwrap();
    let source = cwd.join(".codex/hooks.json");
    let untrusted_key = format!("{}:session_start:0:0", source.display());
    let modified_key = format!("{}:stop:1:0", source.display());
    let response = json!({
        "id": HOOK_TRUST_PREFLIGHT_REQUEST_ID,
        "result": {
            "data": [{
                "cwd": cwd,
                "hooks": [
                    {
                        "key": untrusted_key,
                        "currentHash": "sha256:one",
                        "trustStatus": "untrusted",
                        "isManaged": false,
                        "enabled": true
                    },
                    {
                        "key": modified_key,
                        "currentHash": "sha256:two",
                        "trustStatus": "modified",
                        "isManaged": false,
                        "enabled": false
                    },
                    {
                        "key": "already-trusted",
                        "currentHash": "sha256:three",
                        "trustStatus": "trusted",
                        "isManaged": false,
                        "enabled": true
                    },
                    {
                        "key": "managed",
                        "currentHash": "sha256:four",
                        "trustStatus": "managed",
                        "isManaged": true,
                        "enabled": true
                    }
                ]
            }]
        }
    });

    let projection = hook_trust_projection_from_response(&response, &cwd)
        .unwrap()
        .unwrap();
    assert_eq!(projection.count, 2);
    let parsed: toml::Value = toml::from_str(&projection.override_value).unwrap();
    let state = parsed
        .get("hooks")
        .and_then(|hooks| hooks.get("state"))
        .and_then(toml::Value::as_table)
        .unwrap();
    assert_eq!(
        state[&untrusted_key]["trusted_hash"].as_str(),
        Some("sha256:one")
    );
    assert_eq!(
        state[&modified_key]["trusted_hash"].as_str(),
        Some("sha256:two")
    );
    assert!(!state.contains_key("already-trusted"));
    assert!(!state.contains_key("managed"));

    let mut args = controlled_app_server_args(
        "unix:///server.sock",
        &["--dangerously-bypass-hook-trust".into(), "boot".into()],
    )
    .unwrap();
    insert_app_server_config_override(&mut args, projection.override_value).unwrap();
    assert_eq!(args[args.len() - 4], "-c");
    assert!(args[args.len() - 3].starts_with("hooks.state="));
    assert_eq!(&args[args.len() - 2..], ["--listen", "unix:///server.sock"]);
}

#[test]
fn hook_trust_projection_fails_closed_on_provider_shape_drift() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = fs::canonicalize(tmp.path()).unwrap();
    let response = json!({
        "result": {
            "data": [{
                "cwd": cwd,
                "hooks": [{
                    "key": "hook",
                    "currentHash": "not-a-provider-hash",
                    "trustStatus": "untrusted",
                    "isManaged": false
                }]
            }]
        }
    });
    let error = hook_trust_projection_from_response(&response, &cwd).unwrap_err();
    assert!(error.to_string().contains("typed currentHash"));

    let response = json!({
        "result": {
            "data": [{
                "cwd": cwd,
                "hooks": [{
                    "key": "hook",
                    "currentHash": "sha256:value",
                    "trustStatus": "future-status",
                    "isManaged": false
                }]
            }]
        }
    });
    let error = hook_trust_projection_from_response(&response, &cwd).unwrap_err();
    assert!(error.to_string().contains("unknown trustStatus"));
}

#[test]
fn hook_preflight_uses_the_explicit_controlled_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let explicit = tmp.path().join("workspace");
    fs::create_dir(&explicit).unwrap();
    assert_eq!(
        controlled_hook_cwd(&[
            "--dangerously-bypass-hook-trust".into(),
            "--cd".into(),
            explicit.display().to_string(),
            "boot".into(),
        ])
        .unwrap(),
        fs::canonicalize(explicit).unwrap()
    );
    assert!(
        authored_bypasses_hook_trust(&["--dangerously-bypass-hook-trust".into(), "boot".into()])
            .unwrap()
    );
    assert!(
        !authored_bypasses_hook_trust(&["--".into(), "--dangerously-bypass-hook-trust".into()])
            .unwrap()
    );
}

#[test]
fn process_group_cleanup_reaps_a_native_launcher_descendant() {
    let temporary = tempfile::tempdir().unwrap();
    let descendant_pidfile = temporary.path().join("descendant.pid");
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(r#"sh -c 'printf "%s" "$$" > "$DESCENDANT_PIDFILE"; exec sleep 60' & sleep 60"#)
        .env("DESCENDANT_PIDFILE", &descendant_pidfile)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut launcher = spawn_process_group(&mut command, None).unwrap();
    let mut foreign_command = Command::new("/bin/sh");
    foreign_command
        .arg("-c")
        .arg("sleep 60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut foreign_owner = spawn_process_group(&mut foreign_command, None).unwrap();
    let foreign_pid = foreign_owner.id() as i32;
    let deadline = Instant::now() + Duration::from_secs(1);
    // The shell's `>` redirection creates an empty pidfile before `printf`
    // writes, so wait for parsable content, not mere file existence.
    let mut descendant = None;
    while descendant.is_none() && Instant::now() < deadline {
        if let Ok(content) = std::fs::read_to_string(&descendant_pidfile) {
            descendant = content.trim().parse::<i32>().ok();
        }
        if descendant.is_none() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let descendant = descendant.expect("the launcher did not create its native descendant");
    assert!(
        process_can_retain_cleanup_resources(descendant),
        "the native descendant was not alive before cleanup"
    );

    launcher.terminate();
    assert!(
        process_can_retain_cleanup_resources(foreign_pid),
        "cleanup killed a different live owner"
    );
    foreign_owner.terminate();
    let deadline = Instant::now() + Duration::from_secs(1);
    while process_can_retain_cleanup_resources(descendant) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let survived = process_can_retain_cleanup_resources(descendant);
    if survived {
        unsafe {
            libc::kill(descendant, libc::SIGKILL);
        }
    }
    assert!(
        !survived,
        "native descendant {descendant} survived process-group cleanup"
    );
}

#[test]
fn dropping_a_process_group_owner_reaps_the_group_and_socket() {
    let temporary = tempfile::tempdir().unwrap();
    let socket_path = temporary.path().join("app-server.sock");
    let _listener = UnixListener::bind(&socket_path).unwrap();
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg("sleep 60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let launcher = spawn_process_group(&mut command, Some(&socket_path)).unwrap();
    let launcher_pid = launcher.id() as i32;
    assert!(process_can_retain_cleanup_resources(launcher_pid));

    drop(launcher);
    let deadline = Instant::now() + Duration::from_secs(1);
    while (process_can_retain_cleanup_resources(launcher_pid) || socket_path.exists())
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !process_can_retain_cleanup_resources(launcher_pid),
        "the app-server survived owner cleanup"
    );
    assert!(
        !socket_path.exists(),
        "the app-server socket survived owner cleanup"
    );
}

#[test]
fn a_live_socket_refuses_a_second_control_owner() {
    let temporary = tempfile::tempdir().unwrap();
    let socket_path = temporary.path().join("app-server.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();

    let error = prepare_socket_for_launch(&socket_path).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("refusing a second control owner")
    );
    assert!(socket_path.exists(), "the live owner socket was removed");
    assert!(
        UnixStream::connect(&socket_path).is_ok(),
        "the first owner stopped accepting connections"
    );
    drop(listener);
}

#[test]
fn a_dead_socket_is_removed_before_launch() {
    let temporary = tempfile::tempdir().unwrap();
    let socket_path = temporary.path().join("app-server.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    drop(listener);
    assert!(socket_path.exists());

    prepare_socket_for_launch(&socket_path).unwrap();

    assert!(!socket_path.exists(), "the dead socket was not removed");
}

#[test]
fn a_killed_wrapper_reaps_its_app_server_and_the_next_launch_recovers_its_socket() {
    const TEST_NAME: &str = "codex_app_server::tests::a_killed_wrapper_reaps_its_app_server_and_the_next_launch_recovers_its_socket";
    const ROLE: &str = "ST2_CODEX_ORPHAN_TEST_ROLE";
    const SOCKET_PATH: &str = "ST2_CODEX_ORPHAN_TEST_SOCKET";
    const PID_PATH: &str = "ST2_CODEX_ORPHAN_TEST_PID";
    const READY_PATH: &str = "ST2_CODEX_ORPHAN_TEST_READY";

    match std::env::var(ROLE).as_deref() {
        Ok("server") => {
            let socket_path = PathBuf::from(std::env::var_os(SOCKET_PATH).unwrap());
            let ready_path = PathBuf::from(std::env::var_os(READY_PATH).unwrap());
            let _listener = UnixListener::bind(socket_path).unwrap();
            fs::write(ready_path, b"ready").unwrap();
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        Ok("wrapper") => {
            let pid_path = PathBuf::from(std::env::var_os(PID_PATH).unwrap());
            let socket_path = PathBuf::from(std::env::var_os(SOCKET_PATH).unwrap());
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .arg("--exact")
                .arg(TEST_NAME)
                .arg("--nocapture")
                .env(ROLE, "server")
                .env(SOCKET_PATH, std::env::var_os(SOCKET_PATH).unwrap())
                .env(READY_PATH, std::env::var_os(READY_PATH).unwrap())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let server = spawn_process_group(&mut command, Some(&socket_path)).unwrap();
            fs::write(pid_path, server.id().to_string()).unwrap();
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        Ok(role) => panic!("unknown orphan test role {role}"),
        Err(_) => {}
    }

    let temporary = tempfile::tempdir().unwrap();
    let socket_path = temporary.path().join("app-server.sock");
    let pid_path = temporary.path().join("app-server.pid");
    let ready_path = temporary.path().join("app-server.ready");
    let mut wrapper = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(TEST_NAME)
        .arg("--nocapture")
        .env(ROLE, "wrapper")
        .env(SOCKET_PATH, &socket_path)
        .env(PID_PATH, &pid_path)
        .env(READY_PATH, &ready_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while (!pid_path.is_file() || !ready_path.is_file()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if !pid_path.is_file() || !ready_path.is_file() {
        let _ = wrapper.kill();
        let _ = wrapper.wait();
        panic!("the wrapper did not start its app-server");
    }
    let server_pid = fs::read_to_string(&pid_path)
        .expect("the wrapper did not report its app-server PID")
        .parse::<i32>()
        .unwrap();
    assert!(
        process_can_retain_cleanup_resources(server_pid),
        "the app-server was not alive before the wrapper died"
    );
    assert!(
        fs::symlink_metadata(&socket_path)
            .unwrap()
            .file_type()
            .is_socket(),
        "the app-server did not bind its socket"
    );

    unsafe {
        libc::kill(wrapper.id() as i32, libc::SIGKILL);
    }
    let _ = wrapper.wait();
    let deadline = Instant::now() + Duration::from_secs(2);
    while process_can_retain_cleanup_resources(server_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let server_survived = process_can_retain_cleanup_resources(server_pid);
    if server_survived {
        unsafe {
            libc::kill(server_pid, libc::SIGKILL);
        }
    }
    assert!(!server_survived, "the app-server survived its wrapper");
    assert!(
        socket_path.exists(),
        "the app-server did not leave the expected recoverable socket"
    );
    let refusal_deadline = Instant::now() + Duration::from_secs(2);
    let refusal = loop {
        match UnixStream::connect(&socket_path) {
            Ok(stream) if Instant::now() < refusal_deadline => {
                drop(stream);
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(_) => panic!("the residual socket still had a live listener"),
            Err(error) => break error,
        }
    };
    assert_eq!(refusal.kind(), std::io::ErrorKind::ConnectionRefused);

    prepare_socket_for_launch(&socket_path)
        .expect("the next launch did not recover the residual socket");
    assert!(
        !socket_path.exists(),
        "the next launch did not remove the residual socket"
    );
    let replacement = UnixListener::bind(&socket_path)
        .expect("the next app-server could not bind the recovered socket");
    assert!(
        UnixStream::connect(&socket_path).is_ok(),
        "the replacement app-server socket did not accept a connection"
    );
    drop(replacement);
}

#[test]
fn app_server_configuration_extraction_fails_closed_at_ambiguous_boundaries() {
    let missing = controlled_app_server_args("unix:///server.sock", &["-c".into()]).unwrap_err();
    assert!(missing.to_string().contains("has no value"));

    let unknown = controlled_app_server_args(
        "unix:///server.sock",
        &["--future-option".into(), "value".into(), "boot".into()],
    )
    .unwrap_err();
    assert!(unknown.to_string().contains("unknown Codex option"));

    let sensitive = controlled_app_server_args(
        "unix:///server.sock",
        &["--future-token=do-not-log-this".into(), "boot".into()],
    )
    .unwrap_err();
    assert!(sensitive.to_string().contains("--future-token"));
    assert!(!sensitive.to_string().contains("do-not-log-this"));

    assert_eq!(
        controlled_app_server_args(
            "unix:///server.sock",
            &[
                "--config=projects.x.trust_level=\"trusted\"".into(),
                "resume".into(),
                "thread-explicit".into(),
            ],
        )
        .unwrap(),
        [
            "app-server",
            "--config=projects.x.trust_level=\"trusted\"",
            "--listen",
            "unix:///server.sock",
        ]
    );
}

#[test]
fn controlled_tui_resumes_a_prior_binding_without_overriding_authored_selection() {
    let authored = vec!["--model".into(), "gpt-test".into(), "boot".into()];
    assert_eq!(
        controlled_tui_args("unix:///server.sock", &authored, None).unwrap(),
        [
            "--remote",
            "unix:///server.sock",
            "--model",
            "gpt-test",
            "boot"
        ]
    );
    assert_eq!(
        controlled_tui_args("unix:///server.sock", &authored, Some("thread-prior")).unwrap(),
        [
            "--remote",
            "unix:///server.sock",
            "resume",
            "--model",
            "gpt-test",
            "thread-prior",
            "boot"
        ]
    );
    assert_eq!(
        controlled_tui_args(
            "unix:///server.sock",
            &["resume".into(), "thread-explicit".into()],
            Some("thread-prior")
        )
        .unwrap(),
        [
            "--remote",
            "unix:///server.sock",
            "resume",
            "thread-explicit"
        ]
    );
    assert_eq!(
        expected_resume_thread(
            &["resume".into(), "thread-explicit".into()],
            Some("thread-prior")
        )
        .unwrap(),
        None
    );

    let fork = vec![
        "--dangerously-bypass-hook-trust".into(),
        "fork".into(),
        "thread-explicit".into(),
    ];
    assert_eq!(
        controlled_tui_args("unix:///server.sock", &fork, Some("thread-prior")).unwrap(),
        [
            "--remote",
            "unix:///server.sock",
            "--dangerously-bypass-hook-trust",
            "fork",
            "thread-explicit"
        ]
    );
    assert_eq!(
        expected_resume_thread(&fork, Some("thread-prior")).unwrap(),
        None
    );
    assert_eq!(
        expected_resume_thread(&authored, Some("thread-prior")).unwrap(),
        Some("thread-prior")
    );
}

#[test]
fn automatic_remote_resume_omits_permission_overrides_but_fresh_launch_is_exact() {
    let authored = vec![
        "--dangerously-bypass-approvals-and-sandbox".into(),
        "--dangerously-bypass-hook-trust".into(),
        "--sandbox".into(),
        "workspace-write".into(),
        "--ask-for-approval=never".into(),
        "--model".into(),
        "gpt-test".into(),
        "boot".into(),
    ];

    let mut fresh = vec!["--remote".to_string(), "unix:///server.sock".to_string()];
    fresh.extend(authored.clone());
    assert_eq!(
        controlled_tui_args("unix:///server.sock", &authored, None).unwrap(),
        fresh,
        "an ordinary accepted launch must preserve the exact declaration"
    );
    assert_eq!(
        controlled_tui_args("unix:///server.sock", &authored, Some("thread-prior")).unwrap(),
        [
            "--remote",
            "unix:///server.sock",
            "resume",
            "--model",
            "gpt-test",
            "thread-prior",
            "boot",
        ],
        "remote resume must not send provider-rejected permission overrides"
    );

    let prepared =
        prepare_controlled_launch_args("unix:///server.sock", &authored, Some("thread-prior"));
    assert!(
        prepared
            .server_args
            .windows(2)
            .any(|pair| { pair == ["-c", "approval_policy=\"never\""] })
    );
    assert!(
        prepared
            .server_args
            .windows(2)
            .any(|pair| { pair == ["-c", "sandbox_mode=\"workspace-write\""] })
    );
    let permissions = prepared
        .resume_permissions
        .expect("automatic resume projects the declared permission policy through control");
    assert_eq!(
        permissions,
        ResumePermissionOverrides {
            approval_policy: Some("never".into()),
            approvals_reviewer: None,
            sandbox: Some("workspace-write".into()),
        }
    );
    assert_eq!(
        control_resume_request("thread-prior", Some(&permissions)),
        json!({
            "method": "thread/resume",
            "id": CONTROL_SUBSCRIBE_REQUEST_ID,
            "params": {
                "threadId": "thread-prior",
                "approvalPolicy": "never",
                "sandbox": "workspace-write",
            }
        })
    );
    assert!(resume_permission_overrides_applied(
        &json!({
            "result": {
                "approvalPolicy": "never",
                "sandbox": { "type": "workspaceWrite" }
            }
        }),
        &permissions
    ));
    assert!(!resume_permission_overrides_applied(
        &json!({
            "result": {
                "approvalPolicy": "on-request",
                "sandbox": { "type": "workspaceWrite" }
            }
        }),
        &permissions
    ));

    // This is the standing st3 seat's exact policy declaration. It is the regression that
    // originally exposed Codex rejecting permission flags on automatic remote resume: the TUI
    // argv must be compatible, while the redundant typed resume restores Full Access.
    let standing_seat = vec![
        "--model".into(),
        "gpt-5.6-sol".into(),
        "-c".into(),
        "model_reasoning_effort=xhigh".into(),
        "--dangerously-bypass-approvals-and-sandbox".into(),
        "--dangerously-bypass-hook-trust".into(),
        "boot".into(),
    ];
    let prepared =
        prepare_controlled_launch_args("unix:///server.sock", &standing_seat, Some("thread-prior"));
    assert!(
        prepared
            .server_args
            .windows(2)
            .any(|pair| { pair == ["-c", "sandbox_mode=\"danger-full-access\""] })
    );
    assert_eq!(
        prepared.tui_args,
        [
            "--remote",
            "unix:///server.sock",
            "resume",
            "--model",
            "gpt-5.6-sol",
            "-c",
            "model_reasoning_effort=xhigh",
            "thread-prior",
            "boot",
        ]
    );
    assert_eq!(
        prepared.resume_permissions,
        Some(ResumePermissionOverrides {
            approval_policy: Some("never".into()),
            approvals_reviewer: None,
            sandbox: Some("danger-full-access".into()),
        })
    );
}

#[test]
fn rejected_declared_option_selects_one_minimal_safe_fallback_without_leaking_its_value() {
    let prepared = prepare_controlled_launch_args(
        "unix:///server.sock",
        &["--future-token=do-not-log-this".into(), "boot".into()],
        Some("thread-prior"),
    );

    assert!(prepared.safe_fallback);
    assert_eq!(prepared.declared_options, ["--future-token"]);
    assert!(!format!("{prepared:?}").contains("do-not-log-this"));
    assert_eq!(
        prepared.server_args,
        ["app-server", "--listen", "unix:///server.sock"]
    );
    assert_eq!(
        prepared.tui_args,
        ["--remote", "unix:///server.sock", "resume", "thread-prior"]
    );
    assert_eq!(prepared.expected_resume.as_deref(), Some("thread-prior"));

    let mut attempted = false;
    assert!(claim_safe_fallback_attempt(&mut attempted));
    assert!(!claim_safe_fallback_attempt(&mut attempted));
}

#[test]
fn st3_controlled_launch_rejects_a_promptless_safe_fallback() {
    let prepared = prepare_controlled_launch_args(
        "unix:///server.sock",
        &["--future-token=secret".into(), "Follow .st3/boot.md".into()],
        None,
    );
    assert!(prepared.safe_fallback);
    let error = strict_launch_preflight(&prepared, false).unwrap_err();
    assert!(error.to_string().contains("--future-token"));
    assert!(!error.to_string().contains("secret"));
    assert!(strict_launch_preflight(&prepared, true).is_ok());
}

#[test]
fn controlled_tui_resume_fails_closed_at_ambiguous_option_boundaries() {
    let unknown = controlled_tui_args(
        "unix:///server.sock",
        &["--future-option".into(), "value".into(), "prompt".into()],
        Some("thread-prior"),
    )
    .unwrap_err();
    assert!(unknown.to_string().contains("unknown Codex option"));

    let image = controlled_tui_args(
        "unix:///server.sock",
        &["--image".into(), "one.png".into(), "prompt".into()],
        Some("thread-prior"),
    )
    .unwrap_err();
    assert!(image.to_string().contains("explicit `--`"));

    assert_eq!(
        controlled_tui_args(
            "unix:///server.sock",
            &[
                "--image".into(),
                "one.png".into(),
                "--".into(),
                "prompt".into(),
            ],
            Some("thread-prior"),
        )
        .unwrap(),
        [
            "--remote",
            "unix:///server.sock",
            "resume",
            "--image",
            "one.png",
            "thread-prior",
            "--",
            "prompt"
        ]
    );
}

#[test]
fn state_key_is_path_and_identity_specific_without_embedding_either() {
    let base = Path::new("/state");
    let first = state_dir_in(base, Path::new("/catalog/a"), "h.worker");
    let second = state_dir_in(base, Path::new("/catalog/b"), "h.worker");
    assert_ne!(first, second);
    assert!(first.starts_with("/state/st2/codex"));
    assert!(!first.display().to_string().contains("worker"));
    assert!(!first.display().to_string().contains("catalog/a"));
}

#[test]
fn wrapper_diagnostics_keep_one_bounded_run_without_authored_input() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    secure_dir(&state).unwrap();

    {
        let mut diagnostics = WrapperDiagnostics::open(&state, "h.worker", "h.worker").unwrap();
        diagnostics.record("ownerAcquired", json!({})).unwrap();
        diagnostics
            .record("failed", json!({ "error": "control socket was not ready" }))
            .unwrap();
    }
    let path = state.join("wrapper.log");
    let first = fs::read_to_string(&path).unwrap();
    let entries = first
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["schema"], WRAPPER_DIAGNOSTIC_SCHEMA);
    assert_eq!(entries[0]["agent"], "h.worker");
    assert_eq!(entries[1]["stage"], "failed");
    assert!(first.contains("control socket was not ready"));
    assert!(!first.contains("prompt"));

    {
        let mut replacement = WrapperDiagnostics::open(&state, "h.worker", "h.worker").unwrap();
        replacement.record("ownerAcquired", json!({})).unwrap();
    }
    let replacement = fs::read_to_string(&path).unwrap();
    assert_eq!(replacement.lines().count(), 1);
    assert!(!replacement.contains("control socket was not ready"));
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn runtime_owner_lock_is_nonblocking_and_released_on_close() {
    let tmp = tempfile::tempdir().unwrap();
    let first = acquire_owner_lock(tmp.path()).unwrap();
    let error = acquire_owner_lock(tmp.path()).unwrap_err();
    assert!(error.to_string().contains("already has an owner"));
    drop(first);
    acquire_owner_lock(tmp.path()).unwrap();
}

#[test]
fn waiting_on_a_human_holds_the_exact_turn_and_releases_it_when_the_flag_clears() {
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let mut state = CodexControlState::new(&runtime, "thread-main".into());
    let status_changed = |flags: Value| {
        json!({
            "method": "thread/status/changed",
            "params": {
                "threadId": "thread-main",
                "status": { "type": "active", "activeFlags": flags }
            }
        })
    };
    let active_turn_1 = CodexObservedState::Active {
        turn_id: "turn-1".into(),
    };

    state.observe(&status_changed(json!([]))).unwrap();
    state
        .observe(&json!({
            "method": "turn/started",
            "params": { "threadId": "thread-main", "turn": { "id": "turn-1" } }
        }))
        .unwrap();
    assert_eq!(state.observed(), &active_turn_1);

    for (flags, reason) in [
        (
            json!(["waitingOnApproval"]),
            CodexHoldReason::WaitingOnApproval,
        ),
        (
            json!(["waitingOnUserInput"]),
            CodexHoldReason::WaitingOnUserInput,
        ),
        (
            json!(["conversationHandoff", "waitingOnApproval"]),
            CodexHoldReason::WaitingOnApproval,
        ),
    ] {
        assert!(state.observe(&status_changed(flags)).unwrap());
        assert_eq!(
            state.observed(),
            &CodexObservedState::Held {
                reason,
                turn_id: Some("turn-1".into()),
            }
        );
        // Clearing the flag releases the same turn: no `turn/started` repeats mid-turn.
        assert!(state.observe(&status_changed(json!([]))).unwrap());
        assert_eq!(state.observed(), &active_turn_1);
    }

    // An unknown future flag value degrades to plain `active` instead of failing the frame.
    assert!(!state.observe(&status_changed(json!(["handoff"]))).unwrap());
    assert_eq!(state.observed(), &active_turn_1);

    // The same field is carried by `thread/started`, before any turn is known.
    let mut resumed = CodexControlState::new(&runtime, "thread-main".into());
    assert!(
        resumed
            .observe(&json!({
                "method": "thread/started",
                "params": {
                    "thread": {
                        "id": "thread-main",
                        "status": {
                            "type": "active",
                            "activeFlags": ["waitingOnUserInput"]
                        }
                    }
                }
            }))
            .unwrap()
    );
    assert_eq!(
        resumed.observed(),
        &CodexObservedState::Held {
            reason: CodexHoldReason::WaitingOnUserInput,
            turn_id: None,
        }
    );

    // A turn that ends while still flagged stays unsteerable and is released by the next
    // idle status. `observe_turn_completed` is not modified here; this pins only that the
    // flagged hold cannot decay into a steerable turn.
    state
        .observe(&status_changed(json!(["waitingOnApproval"])))
        .unwrap();
    state
        .observe(&json!({
            "method": "turn/completed",
            "params": { "threadId": "thread-main", "turn": { "id": "turn-1" } }
        }))
        .unwrap();
    assert!(matches!(state.observed(), CodexObservedState::Held { .. }));

    // A status arm without `activeFlags` keeps reading exactly as before.
    assert!(
        state
            .observe(&json!({
                "method": "thread/status/changed",
                "params": { "threadId": "thread-main", "status": { "type": "idle" } }
            }))
            .unwrap()
    );
    assert_eq!(state.observed(), &CodexObservedState::Idle);

    // Delivery declines to steer a session that is waiting on a human, and retains the head.
    let tmp = tempfile::tempdir().unwrap();
    let config = delivery_config(tmp.path());
    let filename =
        message::send_to_inbox(&config.inbox, "h.sender", Some("held"), None, &[], "body").unwrap();
    let mut delivery = inbox_delivery(tmp.path(), config.clone());
    for reason in [
        CodexHoldReason::WaitingOnApproval,
        CodexHoldReason::WaitingOnUserInput,
    ] {
        let blocked = subscribed_state(CodexObservedState::Held {
            reason,
            turn_id: Some("turn-1".into()),
        });
        assert_eq!(delivery.maybe_request(&blocked).unwrap(), None);
        assert!(config.inbox.join(&filename).is_file());
    }
    let released = delivery
        .maybe_request(&subscribed_state(active_turn_1.clone()))
        .unwrap()
        .expect("the retained head steers once the human has answered");
    assert_eq!(released["method"], "turn/steer");
    assert_eq!(released["params"]["expectedTurnId"], "turn-1");
}
