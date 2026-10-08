//! Authenticate the transport peer against physical runtime and provider ownership.
//! Request/reported PIDs, activity and ready observations never grant channel custody.
use super::*;
use crate::mailbox::Authority;
use smallclaims::error::internal;

fn refused(reason: impl Into<String>) -> St3Error {
    St3Error::new("stale-mailbox-session", reason)
}

fn source_agent_dir(state: &AppState, fence: &Fence) -> PathBuf {
    crate::hooks::claude_agent_dir(
        &state.state_dir.join("drivers"),
        &fence.subject,
        &state.node,
    )
}

fn episode(fence: &Fence) -> String {
    let key = format!(
        "mailbox:{}:{}:{}",
        fence.subject, fence.incarnation, fence.component
    );
    format!(
        "attention/{}",
        &hex::encode(Sha256::digest(key.as_bytes()))[..32]
    )
}

fn current_provider(
    state: &AppState,
    fence: &Fence,
    agent_dir: &Path,
    owner: &Authority,
) -> Result<(), St3Error> {
    let raw = st_drivers::harness_events::read_runtime_state(agent_dir, &fence.incarnation)
        .map_err(internal)?
        .ok_or_else(|| refused("the provider source disappeared"))?;
    let current = st_drivers::harness_state::read_raw_at(&raw, None, client_now_ms() as u64);
    if current.state == st_drivers::harness_state::Activity::Ended
        || current.harness.as_deref() != Some(&owner.provider)
        || current.evidence_incarnation.as_deref() != Some(&owner.session)
        || current.ownership_sequence != Some(owner.sequence)
    {
        return Err(refused("the provider is terminal or superseded"));
    }
    state.store.mailbox_session_active(fence, owner)
}

/// Presence reports inherit the admitted peer's custody, never their reported PID.
/// Recheck the durable provider fence without repeatedly querying the PTY process tree.
pub(super) fn admit_report(
    state: &AppState,
    fence: &Fence,
    peer: &NativeDeliveryPeer,
    raw: &str,
) -> Result<(), St3Error> {
    let report: Value = serde_json::from_str(raw).map_err(internal)?;
    if report["transport"].as_str() != Some(peer.transport)
        || report["pid"].as_u64() != Some(u64::from(peer.pid))
    {
        return Err(refused("the report belongs to another native process"));
    }
    let owner = state
        .store
        .mailbox_lease_authority(fence)?
        .ok_or_else(|| refused("the report has no canonical lease"))?;
    if owner.sequence == 0 {
        return with_authority(state, fence, peer, |owner| {
            state.store.check_mailbox(fence)?;
            if !state.store.owns_mailbox_lease(fence, owner)? {
                return Err(refused("the report lease was superseded"));
            }
            Ok(())
        });
    }
    if owner.pid != peer.pid
        || st_runtime::process_start_token(peer.pid).ok() != Some(owner.process_token)
    {
        return Err(refused("the admitted process generation disappeared"));
    }
    let agent_dir = source_agent_dir(state, fence);
    st_drivers::harness_state::with_current_ownership(
        &agent_dir,
        &owner.session,
        owner.sequence,
        || {
            current_provider(state, fence, &agent_dir, &owner)?;
            state.store.check_mailbox(fence)?;
            if !state.store.owns_mailbox_lease(fence, &owner)? {
                return Err(anyhow::Error::new(refused(
                    "the report lease was superseded",
                )));
            }
            Ok(())
        },
    )
    .map_err(|error| {
        error
            .downcast::<St3Error>()
            .unwrap_or_else(|error| refused(error.to_string()))
    })
}

pub(super) fn loss_if_current(
    state: &AppState,
    fence: &Fence,
    peer: &NativeDeliveryPeer,
) -> Result<(), St3Error> {
    if fence.component != "delivery" {
        return Ok(());
    }
    let Some(owner) = state.store.mailbox_lease_authority(fence)? else {
        return Ok(());
    };
    if owner.pid != peer.pid {
        return Ok(());
    }
    state.store.mailbox_session_active(fence, &owner)?;
    let agent_dir = source_agent_dir(state, fence);
    let bytes = st_drivers::harness_events::read_runtime_state(&agent_dir, &fence.incarnation)
        .map_err(internal)?
        .ok_or_else(|| refused("the current provider source is unavailable"))?;
    let observed = st_drivers::harness_state::read_raw_at(&bytes, None, client_now_ms() as u64);
    if observed.state == st_drivers::harness_state::Activity::Ended
        || observed.harness.as_deref() != Some(&owner.provider)
    {
        return Ok(());
    }
    st_drivers::harness_state::with_current_ownership(&agent_dir, &owner.session, owner.sequence, || {
        current_provider(state, fence, &agent_dir, &owner)?;
        if !state.store.owns_mailbox_lease(fence, &owner)? { return Ok(()); }
        let reviewer = state.store.agent_person(&fence.subject).map_err(internal)?
            .or_else(|| state.store.desired_subject_with_writer(&fence.subject).ok().flatten().and_then(|(_, writer)| writer))
            .unwrap_or_else(|| fence.subject.clone());
        let key = episode(fence);
        let previous = state.store.mailbox_failure_episode(fence, &key)?;
        if previous.as_ref().is_some_and(|(_, pending)| *pending) { return Ok(()); }
        let source_revision = state.store.selected_desired_token(&fence.subject).map_err(internal)?;
        state.store.append_mailbox_lease_claim(fence, &owner, &ClaimInput {
            subject: fence.subject.clone(), kind: "operational.failure".into(), actor: Some("daemon/runtime".into()),
            fields: serde_json::from_value(json!({
                "episode": key, "condition":"mailbox-channel-lost", "reviewer":reviewer,
                "title":"A live seat lost its mailbox channel",
                "reason":format!("{} is live but its admitted mailbox channel disconnected. Its authenticated lease reconnects automatically; this fault clears only after a fenced mailbox replay and current native delivery report.", fence.subject),
                "severity":"error", "targets":[fence.subject], "source_revision":source_revision, "incarnation":fence.incarnation,
            })).map_err(internal)?, evidence: vec![], expected_subject: None,
            idempotency_key: Some(format!("{key}:lost:{}", previous.as_ref().map(|(request, _)| request.as_str()).unwrap_or("first"))),
        }, false)?;
        signal_changed(state);
        Ok(())
    }).map_err(|error| error.downcast::<St3Error>().unwrap_or_else(|error| refused(error.to_string())))
}

pub(super) fn repair_proven(
    state: &AppState,
    fence: &Fence,
    peer: &NativeDeliveryPeer,
    raw: &str,
) -> Result<(), St3Error> {
    let Ok(report) = serde_json::from_str::<Value>(raw) else {
        return Err(refused("the delivery report is malformed"));
    };
    if report["transport"].as_str() != Some(peer.transport)
        || report["pid"].as_u64() != Some(u64::from(peer.pid))
    {
        return Err(refused(
            "the delivery report belongs to another native process",
        ));
    }
    with_authority(state, fence, peer, |owner| {
        state.store.check_mailbox(fence)?;
        if !state.store.owns_mailbox_lease(fence, owner)? {
            return Err(refused("the replay lease was superseded"));
        }
        if owner.sequence == 0 {
            return Err(refused("provider custody is not established"));
        }
        let key = episode(fence);
        if let Some((failure, true)) = state.store.mailbox_failure_episode(fence, &key)? {
            state.store.append_mailbox_lease_claim(fence, owner, &ClaimInput {
                subject: fence.subject.clone(), kind: "operational.recovered".into(), actor: Some("daemon/runtime".into()),
                fields: serde_json::from_value(json!({"episode":key,"failure":failure,"reason":"the current authenticated lease consumed a mailbox replay and reported native delivery"})).map_err(internal)?,
                evidence: vec![], expected_subject: None, idempotency_key: Some(format!("{key}:repaired:{failure}")),
            }, true)?;
            signal_changed(state);
        }
        Ok(())
    })
}

#[cfg(target_os = "linux")]
fn belongs_to_runtime(mut pid: u32, runtime_pid: u32) -> bool {
    let mut seen = BTreeSet::new();
    for _ in 0..128 {
        if pid == runtime_pid {
            return true;
        }
        if pid <= 1 || !seen.insert(pid) {
            return false;
        }
        let Some(parent) = fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| stat.rsplit_once(") ").map(|(_, tail)| tail.to_owned()))
            .and_then(|tail| tail.split_whitespace().nth(1)?.parse().ok())
        else {
            return false;
        };
        pid = parent;
    }
    false
}

#[cfg(not(target_os = "linux"))]
fn belongs_to_runtime(_pid: u32, _runtime_pid: u32) -> bool {
    false
}

pub(super) fn with_authority<T>(
    state: &AppState,
    fence: &Fence,
    peer: &NativeDeliveryPeer,
    action: impl FnOnce(&Authority) -> Result<T, St3Error>,
) -> Result<T, St3Error> {
    let (_, env) = local_process_arguments(peer.pid)
        .ok_or_else(|| refused("the authenticated channel process disappeared"))?;
    let var = |key: &str| {
        env.iter()
            .find_map(|entry| entry.strip_prefix(&format!("{key}=")).map(str::to_owned))
    };
    let process_token = st_runtime::process_start_token(peer.pid).map_err(internal)?;
    let desired = state
        .store
        .desired_subjects_named(std::slice::from_ref(&fence.subject))
        .map_err(internal)?
        .into_iter()
        .next()
        .ok_or_else(|| refused("the seat is no longer declared"))?;
    let member = desired
        .member
        .ok_or_else(|| refused("the seat is intentionally stopped"))?;
    if desired.kind == "stop" || member.host != state.node {
        return Err(refused("the seat is stopped or belongs to another host"));
    }
    let provider = member
        .driver
        .ok_or_else(|| refused("the seat has no native provider"))?;
    let transport = match provider.as_str() {
        "claude" => "claude-channel",
        "codex" => "app-server",
        "opencode" => "opencode-server",
        "omp" => "omp-channel",
        "pi" => "pi-channel",
        _ => return Err(refused("unsupported native provider")),
    };
    if peer.transport != transport {
        return Err(refused("the channel belongs to another provider"));
    }
    let physical_incarnation = member
        .terminal_binding
        .as_ref()
        .map(|binding| binding.incarnation.as_str())
        .unwrap_or(&fence.incarnation);
    if member
        .terminal_binding
        .as_ref()
        .is_some_and(|binding| binding.agent_incarnation() != fence.incarnation)
    {
        return Err(refused("the bound terminal invocation was replaced"));
    }
    let physical = st_runtime::PtyRuntime::new(state.pty_root.clone())
        .with_binary(state.pty_binary.to_string_lossy())
        .snapshot()
        .map_err(internal)?
        .into_iter()
        .find(|runtime| {
            runtime
                .pid
                .zip(runtime.created_at.as_deref())
                .is_some_and(|(pid, stamp)| format!("{pid}:{stamp}") == physical_incarnation)
                && (runtime.name == member.runtime_id
                    || runtime.tags.get("st3.subject") == Some(&fence.subject))
        })
        .ok_or_else(|| refused("the exact physical runtime is unavailable"))?;
    let runtime_pid = physical
        .pid
        .ok_or_else(|| refused("the runtime has no process identity"))?;
    if physical.status != "running" || !belongs_to_runtime(peer.pid, runtime_pid) {
        return Err(refused(
            "the channel is outside the exact live physical runtime",
        ));
    }
    let identity = fence
        .subject
        .strip_prefix("agent/")
        .unwrap_or(&fence.subject);
    // The daemon chooses the source path. Inherited environment cannot redirect custody.
    let agent_dir = source_agent_dir(state, fence);
    if let Some(paths) =
        st_drivers::driver_paths::Paths::from_environment(identity, &var).map_err(internal)?
        && paths.agent_dir != agent_dir
    {
        return Err(refused(
            "the channel carries another runtime's observation paths",
        ));
    }
    let source =
        st_drivers::harness_events::read_bound_provider_state(&agent_dir).map_err(internal)?;
    if let Some((_, raw)) = &source {
        let observed = st_drivers::harness_state::read_raw_at(raw, None, client_now_ms() as u64);
        if observed.evidence_incarnation.is_none() || observed.ownership_sequence.is_none() {
            return Err(refused(
                "the provider authority is malformed or unsupported",
            ));
        }
    }
    let bytes = source
        .filter(|(runtime, _)| runtime == &fence.incarnation)
        .map(|(_, raw)| raw);
    let Some(bytes) = bytes else {
        if provider != "codex" || fence.component != "delivery" || peer.pid != runtime_pid {
            return Err(St3Error::new(
                "mailbox-session-starting",
                "waiting for provider ownership",
            ));
        }
        // Codex launches its provider only after binding. Physical custody grants no
        // provider readiness, and a qualified lease cannot downgrade to this bootstrap.
        let bootstrap = Authority {
            provider,
            session: format!("runtime:{}", fence.incarnation),
            sequence: 0,
            pid: peer.pid,
            process_token,
        };
        if state
            .store
            .mailbox_lease_authority(fence)?
            .is_some_and(|owner| owner.sequence != 0)
        {
            return Err(refused(
                "qualified provider custody cannot become bootstrap custody",
            ));
        }
        state.store.mailbox_session_active(fence, &bootstrap)?;
        return action(&bootstrap);
    };
    let observed = st_drivers::harness_state::read_raw_at(&bytes, None, client_now_ms() as u64);
    if observed.harness.as_deref() != Some(&provider)
        || observed.state == st_drivers::harness_state::Activity::Ended
    {
        return Err(refused("the provider ownership is foreign or terminal"));
    }
    let session = observed
        .evidence_incarnation
        .filter(|session| !session.is_empty())
        .ok_or_else(|| refused("the provider session identity is unavailable"))?;
    let sequence = observed
        .ownership_sequence
        .ok_or_else(|| refused("the provider ownership sequence is unavailable"))?;
    let prefix = match provider.as_str() {
        "omp" => Some("ST_OMP_CHANNEL"),
        "pi" => Some("ST_PI_CHANNEL"),
        _ => None,
    };
    if let Some(prefix) = prefix
        && fence.component == "delivery"
        && (var(&format!("{prefix}_SESSION")).as_deref() != Some(&session)
            || var(&format!("{prefix}_SEQ")).and_then(|seq| seq.parse::<u64>().ok())
                != Some(sequence))
    {
        return Err(refused("the channel's provider session was superseded"));
    }
    if provider == "claude"
        && fence.component == "delivery"
        && (var(st_drivers::claude_session::SESSION_ENV).as_deref() != Some(&session)
            || var(st_drivers::claude_session::SESSION_SEQ_ENV)
                .and_then(|seq| seq.parse::<u64>().ok())
                != Some(sequence))
    {
        return Err(refused(
            "the Claude channel's provider session was superseded",
        ));
    }
    let authority = Authority {
        provider,
        session,
        sequence,
        pid: peer.pid,
        process_token,
    };
    // Lock provider ownership through the Store writer decision. It cannot change between
    // capture and admission; Store rechecks runtime and terminal completion in that decision.
    st_drivers::harness_state::with_current_ownership(
        &agent_dir,
        &authority.session,
        authority.sequence,
        || {
            if st_runtime::process_start_token(peer.pid).ok() != Some(process_token) {
                return Err(anyhow::Error::new(refused(
                    "the channel process generation changed",
                )));
            }
            // Terminal state may be written without changing session/sequence. Recheck
            // it while holding the record lock rather than admitting a completion race.
            current_provider(state, fence, &agent_dir, &authority)?;
            state.store.promote_mailbox_bootstrap(fence, &authority)?;
            action(&authority).map_err(anyhow::Error::new)
        },
    )
    .map_err(|error| {
        error
            .downcast::<St3Error>()
            .unwrap_or_else(|error| refused(error.to_string()))
    })
}
