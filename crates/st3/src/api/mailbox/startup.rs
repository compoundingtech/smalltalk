//! A native startup wait comes from live local launch evidence, not a retained status POST.
use super::*;
use crate::api::native_process_identity::{birth, is_descendant};

pub(super) fn bind_with_native_startup(
    store: &Store,
    request: &Fence,
    mut resolve_launch: impl FnMut() -> bool,
) -> Result<Fence, St3Error> {
    reclassify_native_startup(
        store,
        request,
        store.bind_mailbox(request),
        &mut resolve_launch,
    )
}

/// Reclassify only a graph-readiness refusal after ordinary admission. The graph check
/// precedes provider promotion and lease writes; all admitted and other refused paths
/// retain their results. No status hint or capability is minted by this read-only proof.
pub(super) fn reclassify_native_startup(
    store: &Store,
    request: &Fence,
    result: Result<Fence, St3Error>,
    mut resolve_launch: impl FnMut() -> bool,
) -> Result<Fence, St3Error> {
    if matches!(result.as_ref(), Err(error) if error.code == "stale-mailbox-session"
        && error.details.get("mailbox_runtime_pending") == Some(&Value::Bool(true)))
        && store.mailbox_bootstrap_pending(request)?
        && resolve_launch()
        && store.mailbox_bootstrap_pending(request)?
    {
        return Err(St3Error::new(
            "mailbox-session-starting",
            "waiting for the seat's running incarnation",
        ));
    }
    result
}

pub(super) fn live_native_incarnation(
    store: &Store,
    node: &str,
    pty_root: &std::path::Path,
    peer: &NativeDeliveryPeer,
    fence: &Fence,
) -> bool {
    if !same_caller_birth(peer) {
        return false;
    }
    // The authenticated seat's selected declaration names the launch. No caller text
    // is interpreted as a path, and no whole registry is enumerated on a bind retry.
    let Ok(desired) = store.desired_subjects_named(std::slice::from_ref(&peer.agent)) else {
        return false;
    };
    let Some(member) = desired
        .first()
        .filter(|desired| desired.subject == peer.agent && desired.kind == "agent")
        .and_then(|desired| desired.member.as_ref())
        .filter(|member| member.host == node)
    else {
        return false;
    };
    verify_point_launch(pty_root, &member.runtime_id, peer, fence, is_descendant)
}

struct LaunchEvidence {
    observation: st_runtime::PtyObservation,
    metadata: pty_core::registry::SessionMetadata,
    kernel_birth: u64,
}

impl LaunchEvidence {
    fn same_identity(&self, other: &Self) -> bool {
        self.kernel_birth == other.kernel_birth
            && self.metadata.generation == other.metadata.generation
            && self.metadata.daemon_pid == other.metadata.daemon_pid
            && self.metadata.daemon_start_token() == other.metadata.daemon_start_token()
            && self.metadata.created_at == other.metadata.created_at
            && self.observation.tags.get("st3.subject") == other.observation.tags.get("st3.subject")
    }
}

fn verify_point_launch(
    root: &std::path::Path,
    id: &str,
    peer: &NativeDeliveryPeer,
    fence: &Fence,
    walk: impl FnOnce(u32, u32, u64) -> bool,
) -> bool {
    let Some(launch) = point_launch(root, id) else {
        return false;
    };
    matches_incarnation(&launch.observation, peer, fence)
        && launch.observation.pid.is_some_and(|pid| walk(peer.pid, pid, launch.kernel_birth))
        // Re-read only this launch after ancestry, retaining the private generation and
        // opaque supervisor token. Root and caller replacements both fail closed.
        && point_launch(root, id).is_some_and(|current| launch.same_identity(&current))
        && same_caller_birth(peer)
}

fn point_launch(root: &std::path::Path, id: &str) -> Option<LaunchEvidence> {
    use pty_core::registry;
    registry::with_root(root, || {
        registry::validate_name(id).ok()?;
        let read = || {
            use std::io::Read as _;
            let mut bytes = Vec::new();
            fs::File::open(root.join(format!("{id}.json")))
                .ok()?
                .take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() > 1024 * 1024 {
                return None;
            }
            serde_json::from_slice::<registry::SessionMetadata>(&bytes).ok()
        };
        let metadata = read()?;
        if metadata.has_exited() {
            return None;
        }
        // Retain the supervisor's published birth fence, not an unchecked PID sidecar.
        // Its Darwin token is the existing opaque registry contract; do not reinterpret
        // the separate microsecond Unix-caller token as that supervisor identity.
        let root_pid = u32::try_from(metadata.daemon_pid?).ok()?;
        // Capture before verifying the published token and carry this exact kernel
        // identity through ancestry. A root PID recycled after lookup cannot match.
        let root_birth = birth(root_pid)?;
        let pid = registry::read_signal_target_with(id, Some(&metadata))?;
        if u32::try_from(pid).ok()? != root_pid || birth(root_pid) != Some(root_birth) {
            return None;
        }
        let current = read()?;
        if current.has_exited()
            || current.generation != metadata.generation
            || current.daemon_pid != metadata.daemon_pid
            || current.daemon_start_token() != metadata.daemon_start_token()
            || current.created_at != metadata.created_at
        {
            return None;
        }
        Some(LaunchEvidence {
            observation: st_runtime::PtyObservation {
                name: id.into(),
                status: "running".into(),
                exit_code: None,
                pid: Some(u32::try_from(pid).ok()?),
                created_at: Some(metadata.created_at.clone()),
                display_name: metadata.display_name.clone(),
                tags: metadata.tags.clone()?.into_iter().collect(),
            },
            metadata,
            kernel_birth: root_birth,
        })
    })
}

fn same_caller_birth(peer: &NativeDeliveryPeer) -> bool {
    peer.start_token
        .is_some_and(|start| birth(peer.pid) == Some(start))
}

fn matches_incarnation(
    observation: &st_runtime::PtyObservation,
    peer: &NativeDeliveryPeer,
    fence: &Fence,
) -> bool {
    observation.status == "running"
        && peer.agent == fence.subject
        && observation.tags.get("st3.subject") == Some(&fence.subject)
        && observation
            .pid
            .zip(observation.created_at.as_deref())
            .is_some_and(|(pid, at)| fence.incarnation == format!("{pid}:{at}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_evidence_binds_subject_incarnation_and_live_launch() {
        let mut observation = st_runtime::PtyObservation {
            name: "fixture".into(),
            status: "running".into(),
            exit_code: None,
            pid: Some(42),
            created_at: Some("launch-time".into()),
            display_name: None,
            tags: std::collections::BTreeMap::from([("st3.subject".into(), "agent/cedar".into())]),
        };
        let peer = NativeDeliveryPeer {
            agent: "agent/cedar".into(),
            transport: "omp-channel",
            pid: 43,
            archives_inbox: false,
            start_token: None,
        };
        let fence = Fence::new("agent/cedar", "42:launch-time", "delivery");
        assert!(matches_incarnation(&observation, &peer, &fence));
        assert!(!matches_incarnation(
            &observation,
            &peer,
            &Fence::new("agent/cedar", "42:older-launch", "delivery")
        ));
        observation
            .tags
            .insert("st3.subject".into(), "agent/birch".into());
        assert!(!matches_incarnation(&observation, &peer, &fence));
        observation
            .tags
            .insert("st3.subject".into(), "agent/cedar".into());
        observation.status = "exited".into();
        assert!(!matches_incarnation(&observation, &peer, &fence));
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn startup_wait_requires_the_kernel_peer_to_belong_to_the_launch() {
        struct OwnedChild(std::process::Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let pid = std::process::id();
        let child = OwnedChild(
            crate::test_support::command("sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        );
        let root_birth = birth(pid).unwrap();
        assert!(is_descendant(child.0.id(), pid, root_birth));
        assert!(is_descendant(pid, pid, root_birth));
        assert!(!is_descendant(
            child.0.id(),
            pid,
            root_birth.wrapping_add(1)
        ));
        assert!(!is_descendant(pid, pid, root_birth.wrapping_add(1)));
        assert!(!is_descendant(child.0.id(), u32::MAX, root_birth));
    }
    #[test]
    fn bootstrap_refuses_missing_or_reused_authenticated_process_identity() {
        let pid = std::process::id();
        let mut peer = NativeDeliveryPeer {
            agent: "agent/cedar".into(),
            transport: "omp-channel",
            pid,
            archives_inbox: false,
            start_token: None,
        };
        assert!(!same_caller_birth(&peer));
        let start = birth(pid).unwrap();
        peer.start_token = Some(start);
        assert!(same_caller_birth(&peer));
        peer.start_token = Some(start.wrapping_add(1));
        assert!(!same_caller_birth(&peer));
    }
    fn bootstrap_fixture(root: &std::path::Path) -> (AppState, NativeDeliveryPeer, Fence, Value) {
        let state = crate::api::tests::state(root);
        let source =
            "version 2\nagent \"eval.worker\" { workspace \"/tmp\"; harness \"opencode\" {} }";
        let intent = crate::graph::parse_intent(source, "node").unwrap();
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
            .apply(&intent, &planned.subject_tokens, "bootstrap-declaration")
            .unwrap();
        let subject = "agent/eval.worker";
        let pid = std::process::id();
        let peer = NativeDeliveryPeer {
            agent: subject.into(),
            transport: "opencode-server",
            pid,
            archives_inbox: false,
            start_token: birth(pid),
        };
        let metadata = json!({"generation":"00000000000000000000000000000001","daemonPid":pid,
            "daemonStartToken":pty_core::registry::read_process_start_token(pid as i32).unwrap(),
            "createdAt":"launch-time","tags":{"st3.subject":subject}});
        fs::create_dir_all(&state.pty_root).unwrap();
        fs::write(
            state.pty_root.join("eval.worker.json"),
            metadata.to_string(),
        )
        .unwrap();
        let fence = Fence::new(subject, &format!("{pid}:launch-time"), "delivery");
        (state, peer, fence, metadata)
    }

    #[tokio::test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    async fn native_startup_binding_waits_without_starting_status_then_preserves_live_and_retired_fences()
     {
        let root = tempfile::tempdir().unwrap();
        let (state, peer, fence, _) = bootstrap_fixture(root.path());
        let result = bind_with_native_startup(&state.store, &fence, || {
            live_native_incarnation(&state.store, &state.node, &state.pty_root, &peer, &fence)
        });
        assert_eq!(result.err().unwrap().code, "mailbox-session-starting");
        assert_eq!(
            state
                .store
                .readers
                .get()
                .query_row("SELECT count(*) FROM local_mailbox_owners", [], |row| row
                    .get::<_, u64>(
                    0
                ))
                .unwrap(),
            0
        );
        let foreign = NativeDeliveryPeer {
            agent: "agent/birch".into(),
            ..peer.clone()
        };
        assert_eq!(
            super::super::bind(
                State(state.clone()),
                Some(Extension(foreign)),
                Json(fence.clone())
            )
            .await
            .err()
            .unwrap()
            .code,
            "foreign-mailbox"
        );
        crate::mailbox::tests::ready(&state.store, &fence.incarnation);
        let first = bind_with_native_startup(&state.store, &fence, || {
            live_native_incarnation(&state.store, &state.node, &state.pty_root, &peer, &fence)
        })
        .unwrap();
        let successor = Fence::new(&fence.subject, &fence.incarnation, "delivery");
        let second = bind_with_native_startup(&state.store, &successor, || {
            live_native_incarnation(
                &state.store,
                &state.node,
                &state.pty_root,
                &peer,
                &successor,
            )
        })
        .unwrap();
        assert!(second.epoch > first.epoch);
        assert_eq!(
            bind_with_native_startup(&state.store, &first, || {
                live_native_incarnation(&state.store, &state.node, &state.pty_root, &peer, &first)
            })
            .unwrap_err()
            .code,
            "stale-mailbox-session"
        );
    }

    #[cfg(target_os = "linux")]
    struct ArgvStats {
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        task: Option<std::thread::JoinHandle<()>>,
    }

    #[cfg(target_os = "linux")]
    impl ArgvStats {
        fn start(root: &std::path::Path) -> Self {
            use std::io::{Read as _, Write as _};
            use std::sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            };
            let listener =
                std::os::unix::net::UnixListener::bind(root.join("eval.worker.sock")).unwrap();
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = stop.clone();
            let pid = std::process::id();
            let body = json!({"name":"eval.worker", "createdAt":"launch-time", "uptimeSeconds":1,
                "daemon":{"pid":pid,"resources":null},
                "process":{"alive":true,"pid":pid,"exitCode":null,"resources":null},
                "terminal":{"cols":80,"rows":24,"cursorX":0,"cursorY":0,
                    "scrollbackUsed":0,"scrollbackCapacity":100},
                "clients":{"total":0,"attached":0,"readOnly":0},
                "modes":{"sgrMouse":false,"cursorHidden":false,"kittyKeyboard":false,"kittyKeyboardFlags":[]}
            }).to_string();
            let task = std::thread::spawn(move || {
                while !stopped.load(Ordering::Acquire) {
                    if let Ok((mut socket, _)) = listener.accept() {
                        socket
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        let mut request = [0; 128];
                        if socket.read(&mut request).is_ok() {
                            let _ = socket
                                .write_all(&pty_core::protocol::encode_status_response(&body));
                        }
                    } else {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            });
            Self {
                stop,
                task: Some(task),
            }
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for ArgvStats {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Release);
            self.task.take().unwrap().join().unwrap();
        }
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn linux_bind_route_waits_for_the_exact_launch_and_preserves_refusals() {
        use st_drivers::{harness_events, harness_state};
        for argv in [false, true] {
            for case in [
                "pending",
                "admitted",
                "foreign-birth",
                "foreign-subject",
                "ended",
                "exited",
                "other-running",
                "generation-mismatch",
                "provider-ended",
                "past-token",
            ] {
                let root = tempfile::tempdir().unwrap();
                if argv && case == "provider-ended" {
                    continue;
                }
                let (state, mut peer, mut fence, mut metadata) = bootstrap_fixture(root.path());
                let _stats = if argv {
                    let intent = crate::graph::parse_intent(
                    "version 2\nagent \"eval.worker\" { workspace \"/tmp\"; argv \"fixture\"; }", "node",
                ).unwrap();
                    state
                        .store
                        .apply_internal(&intent, "argv-bind-route-fixture")
                        .unwrap();
                    peer.transport = "omp-channel";
                    Some(ArgvStats::start(&state.pty_root))
                } else {
                    None
                };
                // Ordinary Linux admission inventories the registry's daemon PID file as well
                // as metadata. The point-proof helper alone does not need that inventory.
                fs::write(
                    state.pty_root.join("eval.worker.pid"),
                    std::process::id().to_string(),
                )
                .unwrap();

                let agent_dir = crate::hooks::claude_agent_dir(
                    &state.state_dir.join("drivers"),
                    &fence.subject,
                    &state.node,
                );
                harness_events::enable(&agent_dir, &fence.incarnation).unwrap();
                let sequence =
                    harness_state::claim(&agent_dir, "eval.worker", "opencode", "provider")
                        .unwrap();
                harness_state::Writer::new(
                    &agent_dir,
                    "eval.worker",
                    "opencode",
                    Some("eval.worker".into()),
                )
                .with_ownership("provider", sequence)
                .observe(harness_state::Observation::new(
                    harness_state::Activity::Idle,
                    harness_state::BlockedOn::None,
                    harness_state::InputBuffer::Empty,
                ))
                .unwrap();
                let claim = |kind: &str, fields: Value| crate::model::ClaimInput {
                    subject: fence.subject.clone(),
                    kind: kind.into(),
                    actor: None,
                    fields: serde_json::from_value(fields).unwrap(),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                };
                match case {
                    "foreign-birth" => {
                        peer.start_token = peer.start_token.map(|token| token.wrapping_add(1))
                    }
                    "foreign-subject" => peer.agent = "agent/foreign".into(),
                    "ended" => {
                        state
                            .store
                            .append_claim(&claim(
                                "harness.observed",
                                json!({"state":"ended","incarnation_id":fence.incarnation}),
                            ))
                            .unwrap();
                    }
                    "exited" => {
                        state.store.append_claim(&claim("runtime.observed",
                    json!({"status":"exited","runtime_id":"eval.worker","incarnation_id":fence.incarnation}))).unwrap();
                    }
                    "admitted" => crate::mailbox::tests::ready(&state.store, &fence.incarnation),
                    "other-running" => {
                        crate::mailbox::tests::ready(&state.store, "another-incarnation")
                    }
                    "generation-mismatch" => {
                        metadata["createdAt"] = json!("another-launch-generation");
                        fs::write(
                            state.pty_root.join("eval.worker.json"),
                            metadata.to_string(),
                        )
                        .unwrap();
                    }
                    "provider-ended" => {
                        let mut raw: Value = serde_json::from_slice(
                            &harness_events::read_runtime_state(&agent_dir, &fence.incarnation)
                                .unwrap()
                                .unwrap(),
                        )
                        .unwrap();
                        raw["state"] = json!("ended");
                        raw["exit"] = json!("exit 0");
                        raw["reason"] = Value::Null;
                        harness_events::write_snapshot(
                            &agent_dir,
                            "harness-state",
                            &serde_json::to_vec(&raw).unwrap(),
                        )
                        .unwrap();
                    }
                    "past-token" => {
                        crate::mailbox::tests::ready(&state.store, &fence.incarnation);
                        fence = super::super::bind(
                            State(state.clone()),
                            Some(Extension(peer.clone())),
                            Json(fence.clone()),
                        )
                        .await
                        .unwrap()
                        .0;
                        let successor = Fence::new(&fence.subject, &fence.incarnation, "delivery");
                        // The same live provider may reconnect, but a fresh token cannot
                        // make the already-bound predecessor regain its capability.
                        state.store.bind_mailbox(&successor).unwrap();
                    }
                    _ => {}
                }
                let before = state.store.index().unwrap();
                let routed =
                    super::super::bind(State(state.clone()), Some(Extension(peer)), Json(fence))
                        .await;
                if case == "admitted" {
                    assert!(routed.unwrap().0.epoch > 0);
                    assert_eq!(state.store.index().unwrap(), before);
                    continue;
                }
                let error = routed.err().unwrap();
                let expected = match case {
                    "pending" => "mailbox-session-starting",
                    "foreign-subject" => "foreign-mailbox",
                    "generation-mismatch" => "mailbox-authority-unavailable",
                    _ => "stale-mailbox-session",
                };
                assert_eq!(error.code, expected, "{case}: {}", error.message);
                assert_eq!(
                    state.store.index().unwrap(),
                    before,
                    "{case} wrote graph facts"
                );
                if case != "past-token" {
                    let connection = state.store.readers.get();
                    for table in [
                        "local_mailbox_owners",
                        "local_mailbox_leases",
                        "local_mailbox_bindings",
                        "local_mailbox_argv_bindings",
                    ] {
                        assert_eq!(
                            connection
                                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                                    .get::<_, u64>(
                                    0
                                ))
                                .unwrap(),
                            0,
                            "{case} allocated {table}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn launch_replacement_between_lookup_and_ancestry_is_not_starting_authority() {
        let root = tempfile::tempdir().unwrap();
        let (state, peer, fence, metadata) = bootstrap_fixture(root.path());
        let path = state.pty_root.join("eval.worker.json");
        for field in ["generation", "daemonStartToken"] {
            fs::write(&path, metadata.to_string()).unwrap();
            let result = bind_with_native_startup(&state.store, &fence, || {
                verify_point_launch(
                    &state.pty_root,
                    "eval.worker",
                    &peer,
                    &fence,
                    |pid, root, root_birth| {
                        let mut replacement = metadata.clone();
                        replacement[field] = json!("replacement");
                        fs::write(&path, replacement.to_string()).unwrap();
                        is_descendant(pid, root, root_birth)
                    },
                )
            });
            assert_eq!(result.err().unwrap().code, "stale-mailbox-session");
            assert_eq!(
                state
                    .store
                    .readers
                    .get()
                    .query_row("SELECT count(*) FROM local_mailbox_owners", [], |row| row
                        .get::<_, u64>(
                        0
                    ))
                    .unwrap(),
                0
            );
        }
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn point_lookup_refuses_wrong_live_launch_reused_root_and_terminal_racing_resolution() {
        let root = tempfile::tempdir().unwrap();
        let (state, peer, fence, metadata) = bootstrap_fixture(root.path());
        assert!(live_native_incarnation(
            &state.store,
            &state.node,
            &state.pty_root,
            &peer,
            &fence
        ));
        let path = state.pty_root.join("eval.worker.json");
        let mut wrong = metadata.clone();
        wrong["tags"]["st3.subject"] = json!("agent/birch");
        fs::write(&path, wrong.to_string()).unwrap();
        assert!(!live_native_incarnation(
            &state.store,
            &state.node,
            &state.pty_root,
            &peer,
            &fence
        ));
        wrong = metadata.clone();
        wrong["daemonStartToken"] = json!("reused-root");
        fs::write(&path, wrong.to_string()).unwrap();
        assert!(!live_native_incarnation(
            &state.store,
            &state.node,
            &state.pty_root,
            &peer,
            &fence
        ));
        fs::write(&path, metadata.to_string()).unwrap();
        let result = bind_with_native_startup(&state.store, &fence, || {
            let live =
                live_native_incarnation(&state.store, &state.node, &state.pty_root, &peer, &fence);
            state.store.append_claim(&crate::model::ClaimInput {subject:fence.subject.clone(),kind:"runtime.observed".into(),actor:None,
                fields:serde_json::from_value(json!({"status":"exited","runtime_id":"eval.worker","incarnation_id":fence.incarnation})).unwrap(),
                evidence:vec![],expected_subject:None,idempotency_key:None}).unwrap();
            live
        });
        assert_eq!(result.err().unwrap().code, "stale-mailbox-session");
        assert_eq!(
            state
                .store
                .readers
                .get()
                .query_row("SELECT count(*) FROM local_mailbox_owners", [], |row| row
                    .get::<_, u64>(
                    0
                ))
                .unwrap(),
            0
        );
        assert!(point_launch(&state.pty_root, "../eval.worker").is_none());
    }
}
