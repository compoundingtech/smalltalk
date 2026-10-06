//! A native startup wait comes from live local launch evidence, not a retained status POST.
use super::*;
use crate::api::native_process_identity::{birth, is_descendant};

pub(super) fn bind_with_native_startup(
    store: &Store,
    request: &Fence,
    mut resolve_launch: impl FnMut() -> bool,
) -> Result<Fence, St3Error> {
    let result = store.bind_mailbox(request);
    if matches!(result.as_ref(),Err(error) if error.code=="stale-mailbox-session")
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
    let Ok(desired) = store.desired_subjects_named(&[peer.agent.clone()]) else {
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
    let Some(observation) = point_launch(pty_root, &member.runtime_id) else {
        return false;
    };
    same_caller_birth(peer)
        && matches_incarnation(&observation, peer, fence)
        && observation
            .pid
            .is_some_and(|pid| is_descendant(peer.pid, pid))
}

fn point_launch(root: &std::path::Path, id: &str) -> Option<st_runtime::PtyObservation> {
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
        let pid = registry::read_signal_target_with(id, Some(&metadata))?;
        let current = read()?;
        if current.has_exited()
            || current.generation != metadata.generation
            || current.daemon_pid != metadata.daemon_pid
            || current.daemon_start_token() != metadata.daemon_start_token()
            || current.created_at != metadata.created_at
        {
            return None;
        }
        Some(st_runtime::PtyObservation {
            name: id.into(),
            status: "running".into(),
            exit_code: None,
            pid: Some(u32::try_from(pid).ok()?),
            created_at: Some(metadata.created_at),
            display_name: metadata.display_name,
            tags: metadata.tags?.into_iter().collect(),
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
        assert!(is_descendant(child.0.id(), pid));
        assert!(is_descendant(pid, pid));
        assert!(!is_descendant(child.0.id(), u32::MAX));
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
        let source = "version 2\nagent \"eval.worker\" { workspace \"/tmp\"; harness \"omp\" {} }";
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
            transport: "omp-channel",
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
    async fn full_native_bind_waits_without_starting_status_then_preserves_live_and_retired_fences()
    {
        let root = tempfile::tempdir().unwrap();
        let (state, peer, fence, _) = bootstrap_fixture(root.path());
        let result = super::super::bind(
            State(state.clone()),
            Some(Extension(peer.clone())),
            Json(fence.clone()),
        )
        .await;
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
        let first = super::super::bind(
            State(state.clone()),
            Some(Extension(peer.clone())),
            Json(fence.clone()),
        )
        .await
        .unwrap()
        .0;
        let successor = Fence::new(&fence.subject, &fence.incarnation, "delivery");
        let second = super::super::bind(
            State(state.clone()),
            Some(Extension(peer.clone())),
            Json(successor),
        )
        .await
        .unwrap()
        .0;
        assert!(second.epoch > first.epoch);
        assert_eq!(
            super::super::bind(State(state), Some(Extension(peer)), Json(first))
                .await
                .err()
                .unwrap()
                .code,
            "stale-mailbox-session"
        );
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
