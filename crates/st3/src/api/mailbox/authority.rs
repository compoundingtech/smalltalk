//! Authenticate the transport peer against physical runtime and provider ownership.
//! Request/reported PIDs, activity and ready observations never grant channel custody.
use super::*;
use crate::mailbox::Authority;
use smallclaims::error::internal;
use std::path::PathBuf;

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

/// Decode ownership at the record's own stamp: observation freshness cannot erase a
/// terminal ownership decision. This grants custody only, never fresh activity or readiness.
fn decode_provider_authority(raw: &[u8]) -> Result<st_drivers::harness_state::Observed, St3Error> {
    use st_drivers::harness_state::{Activity, read_raw_at};
    let record: Value =
        serde_json::from_slice(raw).map_err(|_| refused("malformed provider authority"))?;
    let stamp = record["writtenAtMs"]
        .as_u64()
        .ok_or_else(|| refused("missing provider authority stamp"))?;
    let observed = read_raw_at(raw, None, stamp);
    let placeholder =
        record["state"] == "ended" && record["exit"].is_null() && record["reason"] == "superseded";
    if observed
        .evidence_incarnation
        .as_deref()
        .is_none_or(str::is_empty)
        || observed
            .ownership_sequence
            .is_none_or(|sequence| sequence == 0)
        || (observed.state == Activity::Unknown && !placeholder)
    {
        return Err(refused(
            "the provider authority is terminal, malformed or unsupported",
        ));
    }
    Ok(observed)
}

fn provider_authority(raw: &[u8]) -> Result<st_drivers::harness_state::Observed, St3Error> {
    let observed = decode_provider_authority(raw)?;
    if observed.state == st_drivers::harness_state::Activity::Ended {
        return Err(refused("the provider authority is terminal"));
    }
    Ok(observed)
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
    let current = provider_authority(&raw)?;
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
        return with_authority(state, fence, peer, |owner, validate| {
            validate()?;
            state.store.check_mailbox(fence)?;
            if !state.store.owns_mailbox_lease(fence, owner)? {
                return Err(refused("the report lease was superseded"));
            }
            Ok(())
        });
    }
    if owner.pid != peer.pid || process_birth(peer.pid)? != owner.process_token {
        return Err(refused("the admitted process generation disappeared"));
    }
    let agent_dir = source_agent_dir(state, fence);
    let result = st_drivers::harness_state::with_current_ownership(
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
    });
    if result.is_ok() {
        return result;
    }
    let current = st_drivers::harness_events::read_runtime_state(&agent_dir, &fence.incarnation)
        .map_err(internal)?
        .ok_or_else(|| refused("the provider source disappeared"))?;
    let observed = provider_authority(&current)?;
    if observed.harness.as_deref() == Some(&owner.provider)
        && observed.evidence_incarnation.as_deref() == Some(&owner.session)
        && observed.ownership_sequence.is_some_and(|sequence| sequence > owner.sequence)
    {
        // A surviving wrapper reclaims its provider record during re-exec. Re-admit
        // this exact capability under physical/provider and writer fences once, rather
        // than treating the ownership transition as a new channel or fresh token.
        return with_authority(state, fence, peer, |current_owner, validate| {
            validate()?;
            state.store.check_mailbox(fence)?;
            if !state.store.owns_mailbox_lease(fence, current_owner)? {
                return Err(refused("the report lease was superseded"));
            }
            Ok(())
        });
    }
    result
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
    let observed = provider_authority(&bytes)?;
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
    with_authority(state, fence, peer, |owner, validate| {
        validate()?;
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
fn runtime_parent(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[cfg(not(target_os = "linux"))]
fn runtime_parent(_pid: u32) -> Option<u32> {
    None
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
        let Some(parent) = runtime_parent(pid) else {
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

/// A captured physical witness is immutable across the writer admission boundary.
/// A PID alone never certifies the daemon, its terminal child or the authenticated peer.
#[derive(Clone, Debug)]
struct BootstrapWitness {
    name: String,
    created_at: String,
    daemon: (u32, u64),
    terminal: (u32, u64),
    peer: (u32, u64),
    bound: bool,
}

fn unavailable(reason: impl Into<String>) -> St3Error {
    St3Error::new("mailbox-authority-unavailable", reason)
}

fn terminal_identity(
    name: &str,
    created_at: &str,
    daemon_pid: u32,
    stats: &pty_core::stats::StatsResult,
) -> Result<u32, St3Error> {
    if stats.name != name
        || stats.created_at.as_deref() != Some(created_at)
        || u32::try_from(stats.daemon.pid).ok() != Some(daemon_pid)
        || !stats.process.alive
    {
        return Err(refused(
            "the exact physical terminal generation was replaced or ended",
        ));
    }
    stats
        .process
        .pid
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid > 1)
        .ok_or_else(|| unavailable("the runtime has no proven live terminal process"))
}

impl BootstrapWitness {
    fn validate_with(
        &self,
        stats: Result<pty_core::stats::StatsResult, St3Error>,
        birth: impl Fn(u32) -> Result<u64, St3Error>,
        parent: impl Fn(u32) -> Option<u32>,
    ) -> Result<(), St3Error> {
        let terminal = terminal_identity(&self.name, &self.created_at, self.daemon.0, &stats?)?;
        if terminal != self.terminal.0 {
            return Err(refused("the physical terminal child was replaced"));
        }
        for (pid, captured) in [self.daemon, self.terminal, self.peer] {
            if birth(pid)? != captured {
                return Err(refused(
                    "a captured physical process generation was replaced",
                ));
            }
        }
        let wrapper_matches = if self.bound {
            parent(self.peer.0) == Some(self.terminal.0)
        } else {
            self.peer == self.terminal
        };
        if !wrapper_matches {
            return Err(refused(
                "the authenticated wrapper left its exact terminal custody",
            ));
        }
        let mut pid = self.terminal.0;
        let mut seen = BTreeSet::new();
        while pid != self.daemon.0 {
            if pid <= 1 || !seen.insert(pid) || seen.len() > 128 {
                return Err(refused("the terminal child left its exact daemon custody"));
            }
            pid = parent(pid).ok_or_else(|| unavailable("terminal ancestry is unavailable"))?;
        }
        Ok(())
    }

    fn validate(&self, state: &AppState) -> Result<(), St3Error> {
        self.validate_with(
            terminal_stats(state, &self.name),
            process_birth,
            runtime_parent,
        )
    }
}

fn process_birth(pid: u32) -> Result<u64, St3Error> {
    st_runtime::process_start_token(pid)
        .map_err(|error| unavailable(format!("process generation is unavailable: {error}")))
}

fn terminal_stats(state: &AppState, name: &str) -> Result<pty_core::stats::StatsResult, St3Error> {
    pty_client::query_stats_in_with_timeout(&state.pty_root, name, pty_client::STATS_TIMEOUT)
        .map_err(|error| unavailable(format!("the terminal identity is unavailable: {error}")))
}

/// The registry incarnation names the supporting daemon. Its typed live answer must
/// agree with the selected creation stamp before its terminal child grants bootstrap custody.
fn bootstrap_witness(
    state: &AppState,
    physical: &st_runtime::PtyObservation,
    peer: &NativeDeliveryPeer,
    peer_birth: u64,
    bound: bool,
) -> Result<BootstrapWitness, St3Error> {
    let daemon_pid = physical
        .pid
        .ok_or_else(|| unavailable("the runtime has no daemon identity"))?;
    let created_at = physical
        .created_at
        .clone()
        .filter(|stamp| !stamp.is_empty())
        .ok_or_else(|| unavailable("the runtime has no creation identity"))?;
    let daemon_birth = process_birth(daemon_pid)?;
    let stats = terminal_stats(state, &physical.name)?;
    let terminal_pid = terminal_identity(&physical.name, &created_at, daemon_pid, &stats)?;
    let witness = BootstrapWitness {
        name: physical.name.clone(),
        created_at,
        daemon: (daemon_pid, daemon_birth),
        terminal: (terminal_pid, process_birth(terminal_pid)?),
        peer: (peer.pid, peer_birth),
        bound,
    };
    witness.validate_with(Ok(stats), process_birth, runtime_parent)?;
    Ok(witness)
}

/// A declared argv-only program can consume channel envelopes without a model provider.
/// This preserves the delivery monitor's protocol, not qualified native custody or repair.
#[cfg(target_os = "linux")]
pub(super) fn with_argv_channel<T>(
    state: &AppState,
    fence: &Fence,
    peer: &NativeDeliveryPeer,
    action: impl FnOnce(&crate::model::MemberSpec, &dyn Fn() -> Result<(), St3Error>) -> Result<T, St3Error>,
) -> Result<Option<T>, St3Error> {
    let desired = state.store.desired_subjects_named(std::slice::from_ref(&fence.subject))
        .map_err(internal)?.into_iter().next()
        .ok_or_else(|| refused("the seat is no longer declared"))?;
    let member = desired.member.ok_or_else(|| refused("the seat is intentionally stopped"))?;
    if member.driver.is_some() { return Ok(None); }
    if desired.kind == "stop"
        || member.kind != crate::model::MemberKind::Agent
        || member.host != state.node
        || member.terminal_binding.is_some()
        || !matches!(&member.launch, crate::model::LaunchSpec::Argv(argv) if !argv.is_empty())
        || fence.component != "delivery"
        || peer.transport != "omp-channel"
    {
        return Err(refused("this is not an explicit local argv channel seat"));
    }
    let physical = st_runtime::PtyRuntime::new(state.pty_root.clone())
        .with_binary(state.pty_binary.to_string_lossy()).snapshot().map_err(internal)?
        .into_iter().find(|runtime| runtime.name == member.runtime_id
            && runtime.status == "running"
            && runtime.pid.zip(runtime.created_at.as_deref())
                .is_some_and(|(pid, stamp)| format!("{pid}:{stamp}") == fence.incarnation))
        .ok_or_else(|| unavailable("the exact argv runtime is unavailable"))?;
    let daemon_pid = physical.pid.ok_or_else(|| refused("missing argv runtime identity"))?;
    let stamp = physical.created_at.as_deref().ok_or_else(|| refused("missing argv runtime generation"))?;
    let stats = terminal_stats(state, &physical.name)?;
    let terminal_pid = terminal_identity(&physical.name, stamp, daemon_pid, &stats)?;
    let generations = [
        (daemon_pid, process_birth(daemon_pid)?),
        (terminal_pid, process_birth(terminal_pid)?),
        (peer.pid, process_birth(peer.pid)?),
    ];
    let validate = || {
        let stats = terminal_stats(state, &physical.name)?;
        if terminal_identity(&physical.name, stamp, daemon_pid, &stats)? != terminal_pid {
            return Err(refused("the argv terminal process was replaced"));
        }
        for (pid, birth) in generations {
            if process_birth(pid)? != birth {
                return Err(refused("the argv process generation was replaced"));
            }
        }
        if !belongs_to_runtime(terminal_pid, daemon_pid) || !belongs_to_runtime(peer.pid, terminal_pid) {
            return Err(refused("the channel is outside the exact argv process"));
        }
        Ok(())
    };
    action(&member, &validate).map(Some)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn with_argv_channel<T>(
    _state: &AppState,
    _fence: &Fence,
    _peer: &NativeDeliveryPeer,
    _action: impl FnOnce(&crate::model::MemberSpec, &dyn Fn() -> Result<(), St3Error>) -> Result<T, St3Error>,
) -> Result<Option<T>, St3Error> {
    Ok(None)
}

pub(super) fn with_authority<T>(
    state: &AppState,
    fence: &Fence,
    peer: &NativeDeliveryPeer,
    action: impl FnOnce(&Authority, &dyn Fn() -> Result<(), St3Error>) -> Result<T, St3Error>,
) -> Result<T, St3Error> {
    if fence.epoch != 0 {
        state.store.check_mailbox_custody_fence(fence)?;
    }
    let (arguments, env) = local_process_arguments(peer.pid)
        .ok_or_else(|| unavailable("the authenticated channel process identity is unavailable"))?;
    let var = |key: &str| {
        env.iter()
            .find_map(|entry| entry.strip_prefix(&format!("{key}=")).map(str::to_owned))
    };
    let process_token = process_birth(peer.pid)?;
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
                    || runtime.tags.get("st3.subject")
                        == Some(
                            member
                                .terminal_binding
                                .as_ref()
                                .map(|binding| &binding.subject)
                                .unwrap_or(&fence.subject),
                        ))
        })
        .ok_or_else(|| unavailable("the exact physical runtime is unavailable"))?;
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
    let source = st_drivers::harness_events::read_bound_provider_state(&agent_dir)
        .map_err(|error| unavailable(error.to_string()))?;
    if let Some((_, raw)) = &source {
        let observed = decode_provider_authority(raw)?;
        if observed.evidence_incarnation.is_none() || observed.ownership_sequence.is_none() {
            return Err(refused(
                "the provider authority is malformed or unsupported",
            ));
        }
    }
    let captured_source = source.clone();
    let bytes = source
        .filter(|(runtime, _)| runtime == &fence.incarnation)
        .map(|(_, raw)| raw);
    let Some(bytes) = bytes else {
        if provider != "codex" || fence.component != "delivery" {
            return Err(St3Error::new(
                "mailbox-session-starting",
                "waiting for provider ownership",
            ));
        }
        let witness = bootstrap_witness(
            state,
            &physical,
            peer,
            process_token,
            member.terminal_binding.is_some(),
        )?;
        let terminal_pid = witness.terminal.0;
        let current_wrapper = (member.terminal_binding.is_none() && peer.pid == terminal_pid
            || member.terminal_binding.is_some() && runtime_parent(peer.pid) == Some(terminal_pid))
            && arguments.windows(2).any(|pair| pair == ["driver", "codex"])
            && arguments
                .windows(2)
                .any(|pair| pair[0] == "--subject" && pair[1] == fence.subject);
        if !current_wrapper {
            return Err(refused(
                "this process is not the current physical Codex wrapper",
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
        let validate = || {
            witness.validate(state)?;
            // Do not admit a stale bootstrap capture after qualified ownership appeared.
            let current = st_drivers::harness_events::read_bound_provider_state(&agent_dir)
                .map_err(|error| unavailable(error.to_string()))?;
            if current != captured_source {
                return Err(St3Error::new(
                    "mailbox-session-starting",
                    "provider ownership changed during bootstrap admission",
                ));
            }
            Ok(())
        };
        return action(&bootstrap, &validate);
    };
    let observed = provider_authority(&bytes)?;
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
            if process_birth(peer.pid)? != process_token {
                return Err(anyhow::Error::new(refused(
                    "the channel process generation changed",
                )));
            }
            // Terminal state may be written without changing session/sequence. Recheck
            // it while holding the record lock rather than admitting a completion race.
            current_provider(state, fence, &agent_dir, &authority)?;
            state.store.promote_current_mailbox_ownership(fence, &authority)?;
            action(&authority, &|| Ok(())).map_err(anyhow::Error::new)
        },
    )
    .map_err(|error| {
        error
            .downcast::<St3Error>()
            .unwrap_or_else(|error| refused(error.to_string()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_stats() -> pty_core::stats::StatsResult {
        serde_json::from_value(json!({
            "name":"physical", "createdAt":"generation-1", "uptimeSeconds":1,
            "daemon":{"pid":100,"resources":null},
            "process":{"alive":true,"pid":200,"exitCode":null,"resources":null},
            "terminal":{"cols":80,"rows":24,"cursorX":0,"cursorY":0,
                "scrollbackUsed":0,"scrollbackCapacity":100},
            "clients":{"total":0,"attached":0,"readOnly":0},
            "modes":{"sgrMouse":false,"cursorHidden":false,"kittyKeyboard":false,"kittyKeyboardFlags":[]}
        })).unwrap()
    }

    #[test]
    fn bootstrap_generation_refusals_leave_capabilities_and_epochs_unchanged() {
        for case in [
            "name",
            "stamp",
            "missing-stamp",
            "daemon-pid",
            "terminal-pid",
            "ended",
            "daemon-birth",
            "terminal-birth",
            "peer-birth",
            "uncertain",
            "absent",
            "timeout",
            "parent",
        ] {
            let store = Store::open_memory("node").unwrap();
            let intent = crate::graph::parse_intent(
                "version 2\nagent \"eval.worker\" { host \"node\"; workspace \"/tmp\"; harness \"codex\" {} }", "node",
            ).unwrap();
            store
                .apply_internal(&intent, "bootstrap-witness-fixture")
                .unwrap();
            crate::mailbox::tests::ready(&store, "current");
            let owner = Authority {
                provider: "codex".into(),
                session: "runtime:current".into(),
                sequence: 0,
                pid: std::process::id(),
                process_token: st_runtime::process_start_token(std::process::id()).unwrap(),
            };
            let bound = store
                .bind_mailbox_with_lease(
                    &Fence::new("agent/eval.worker", "current", "delivery"),
                    Some(&owner),
                )
                .unwrap();
            let witness = BootstrapWitness {
                name: "physical".into(),
                created_at: "generation-1".into(),
                daemon: (100, 10),
                terminal: (200, 20),
                peer: (300, 30),
                bound: true,
            };
            let mut stats = fixture_stats();
            match case {
                "name" => stats.name = "foreign".into(),
                "stamp" => stats.created_at = Some("reused-pid-generation".into()),
                "missing-stamp" => stats.created_at = None,
                "daemon-pid" => stats.daemon.pid += 1,
                "terminal-pid" => stats.process.pid = Some(201),
                "ended" => stats.process.alive = false,
                _ => {}
            }
            let validate = || {
                witness.validate_with(
                    if case == "timeout" {
                        Err(unavailable("isolated injected stats timeout"))
                    } else {
                        Ok(stats.clone())
                    },
                    |pid| {
                        if case == "uncertain" {
                            return Err(unavailable("isolated unreadable process"));
                        }
                        if case == "absent" && pid == 200 {
                            return Err(unavailable("isolated absent terminal"));
                        }
                        let original = u64::from(pid / 10);
                        Ok(original
                            + u64::from(matches!(
                                (case, pid),
                                ("daemon-birth", 100)
                                    | ("terminal-birth", 200)
                                    | ("peer-birth", 300)
                            )))
                    },
                    |pid| match pid {
                        200 => Some(100),
                        300 => Some(if case == "parent" { 201 } else { 200 }),
                        _ => None,
                    },
                )
            };
            let expected = if matches!(case, "uncertain" | "absent" | "timeout") {
                "mailbox-authority-unavailable"
            } else {
                "stale-mailbox-session"
            };
            let duplicate = Fence::new(&bound.subject, &bound.incarnation, &bound.component);
            assert_eq!(
                store
                    .bind_mailbox_with_lease_checked(&duplicate, Some(&owner), &validate)
                    .unwrap_err()
                    .code,
                expected,
                "{case}"
            );
            assert_eq!(
                store.mailbox_lease_authority(&bound).unwrap(),
                Some(owner.clone()),
                "{case}"
            );
            store.check_mailbox(&bound).unwrap();
            let connection = store.connection.write();
            let tokens: u64 = connection
                .query_row("SELECT COUNT(*) FROM local_mailbox_bindings", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(tokens, 1, "{case}");
            // An absent owner is the only repairable displacement. Uncertainty still cannot insert it.
            connection
                .execute("DELETE FROM local_mailbox_owners", [])
                .unwrap();
            drop(connection);
            assert_eq!(
                store
                    .repair_mailbox_checked(&bound, &owner, &validate)
                    .unwrap_err()
                    .code,
                expected,
                "{case}"
            );
            let connection = store.connection.write();
            let owners: u64 = connection
                .query_row("SELECT COUNT(*) FROM local_mailbox_owners", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(owners, 0, "{case}");
            assert_eq!(
                connection
                    .query_row("SELECT epoch FROM local_mailbox_leases", [], |row| row
                        .get::<_, u64>(0))
                    .unwrap(),
                bound.epoch
            );
        }
    }

    #[test]
    fn managed_bootstrap_requires_wrapper_equality_and_bound_bootstrap_requires_direct_parent() {
        let mut witness = BootstrapWitness {
            name: "physical".into(),
            created_at: "generation-1".into(),
            daemon: (100, 10),
            terminal: (200, 20),
            peer: (200, 20),
            bound: false,
        };
        let birth = |pid| Ok(u64::from(pid / 10));
        let parent = |pid| match pid {
            200 => Some(100),
            300 => Some(200),
            400 => Some(300),
            _ => None,
        };
        witness
            .validate_with(Ok(fixture_stats()), birth, parent)
            .unwrap();
        witness.peer = (300, 30);
        assert_eq!(
            witness
                .validate_with(Ok(fixture_stats()), birth, parent)
                .unwrap_err()
                .code,
            "stale-mailbox-session"
        );
        witness.bound = true;
        witness
            .validate_with(Ok(fixture_stats()), birth, parent)
            .unwrap();
        witness.peer = (400, 40);
        assert_eq!(
            witness
                .validate_with(Ok(fixture_stats()), birth, parent)
                .unwrap_err()
                .code,
            "stale-mailbox-session"
        );
    }

    #[test]
    fn raw_terminal_authority_survives_staleness_and_clock_skew_under_the_owner_lock() {
        use st_drivers::{harness_events, harness_state};
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_memory("node").unwrap());
        let intent = crate::graph::parse_intent(
            "version 2\nagent \"eval.worker\" { host \"node\"; workspace \"/tmp\"; harness \"codex\" {} }", "node",
        ).unwrap();
        store
            .apply_internal(&intent, "raw-terminal-fixture")
            .unwrap();
        crate::mailbox::tests::ready(&store, "current");
        let state = AppState {
            store: store.clone(),
            notify: Arc::new(Notify::new()),
            event_notify: watch::channel(0).0,
            node: "node".into(),
            state_dir: root.path().into(),
            pty_root: root.path().join("pty"),
            pty_binary: "pty".into(),
            fleet_id: None,
            configured_peers: vec![],
            client_relay: None,
            native_session_home: None,
            planner_default: Default::default(),
        };
        let request = Fence::new("agent/eval.worker", "current", "delivery");
        let agent_dir = source_agent_dir(&state, &request);
        harness_events::enable(&agent_dir, "current").unwrap();
        let sequence =
            harness_state::claim(&agent_dir, "eval.worker", "codex", "provider").unwrap();
        let owner = Authority {
            provider: "codex".into(),
            session: "provider".into(),
            sequence,
            pid: std::process::id(),
            process_token: st_runtime::process_start_token(std::process::id()).unwrap(),
        };
        let fence = store
            .bind_mailbox_with_lease(&request, Some(&owner))
            .unwrap();
        let mut record: Value = serde_json::from_slice(
            &harness_events::read_runtime_state(&agent_dir, "current")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        // The startup placeholder owns custody while proving no native activity.
        assert!(provider_authority(&serde_json::to_vec(&record).unwrap()).is_ok());
        let peer = NativeDeliveryPeer {
            agent: fence.subject.clone(),
            transport: "app-server",
            pid: owner.pid,
            archives_inbox: true,
        };
        let report = json!({"transport":"app-server","pid":peer.pid}).to_string();
        let now = client_now_ms() as u64;
        for stamp in [1, now.saturating_add(3_600_000)] {
            record["writtenAtMs"] = json!(stamp);
            record["state"] = json!("ended");
            record["exit"] = json!("exit 0");
            record["reason"] = Value::Null;
            let raw = serde_json::to_vec(&record).unwrap();
            assert_eq!(
                harness_state::read_raw_at(&raw, None, now).state,
                harness_state::Activity::Unknown
            );
            harness_events::write_snapshot(&agent_dir, "harness-state", &raw).unwrap();
            for operation in ["bind", "repair"] {
                let admitted = std::sync::atomic::AtomicBool::new(false);
                let result = harness_state::with_current_ownership(
                    &agent_dir,
                    &owner.session,
                    sequence,
                    || {
                        current_provider(&state, &fence, &agent_dir, &owner)?;
                        admitted.store(true, std::sync::atomic::Ordering::SeqCst);
                        if operation == "bind" {
                            store.bind_mailbox_with_lease(&fence, Some(&owner))?;
                        } else {
                            store.repair_mailbox(&fence, &owner)?;
                        }
                        Ok(())
                    },
                );
                assert!(result.is_err(), "{operation} admitted terminal authority");
                assert!(!admitted.load(std::sync::atomic::Ordering::SeqCst));
            }
            assert_eq!(
                admit_report(&state, &fence, &peer, &report)
                    .unwrap_err()
                    .code,
                "stale-mailbox-session"
            );
            assert_eq!(
                store.mailbox_lease_authority(&fence).unwrap(),
                Some(owner.clone())
            );
            store.check_mailbox(&fence).unwrap(); // Graph remains running: raw terminal was decisive.
        }
        for state in ["unknown", "future-state"] {
            record["state"] = json!(state);
            record["exit"] = Value::Null;
            let raw = serde_json::to_vec(&record).unwrap();
            assert!(decode_provider_authority(&raw).is_err());
        }
        assert!(decode_provider_authority(b"{broken").is_err());
    }
}
