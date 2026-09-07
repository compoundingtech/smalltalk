//! Controlled pi launch with a session-owned presence lease.
//!
//! pi has no MCP and no app-server: its integration point is an extension loaded into the
//! interactive process, and that extension reaches st2 by spawning `st2 driver pi-channel`. Two
//! facts have to be handed to it, and neither is discoverable from inside pi. The first is *which*
//! st2 to run — resolving `st2` from `PATH` would let a replaced control plane and its live agents
//! disagree, which the R11 control-plane replacement guarantee exists to prevent — so the wrapper
//! exports its own executable path. The second is the catalog and identity the channel must bind.
//!
//! The wrapper also owns presence for the same reason the Claude wrapper does: the extension lives
//! only as long as pi's process, and a measured SIGKILL of pi produces no terminal record at all
//! (`docs/vrs/.experiments/2026-08-18-pi-harness-integration.md`). Presence therefore decays by
//! staleness, exactly as for the other harnesses.

use std::path::Path;
use std::process::ExitStatus;

use anyhow::{Context as _, Result};

use crate::provider_session::{
    PROVIDER_POLL, ProviderOutcome, STOP, install_signal_handler, run_provider_observed,
};
use crate::{harness_state, hooks, message, status};

/// The extension file inside this binary's immutable hook set.
const EXTENSION: &str = "pi-channel.ts";

/// The pi release the harness-context producer's arithmetic was measured against (HC-R13).
///
/// pi has no runtime version gate — unlike omp, whose wrapper refuses an unadmitted minor — so the
/// only place this repository couples itself to a pi build is `flake.nix`'s extension check, which
/// type-checks and runtime-smokes the shipped asset against exactly this tarball. That makes the
/// flake pin the gate for this constant, and `pi_channel`'s fixture asserts the two agree: a pi
/// bump that changes what `getContextUsage().tokens` means must move both together or fail.
pub const MEASURED_CONTEXT_VERSION: &str = "0.84.2";

/// The exact st2 executable the pi extension must spawn for its channel.
pub const CHANNEL_BIN: &str = "ST2_PI_CHANNEL_BIN";
/// The catalog root that executable must be pointed at.
pub const CHANNEL_CATALOG: &str = "ST2_PI_CHANNEL_CATALOG";
/// The host-qualified bus identity the channel binds.
pub const CHANNEL_IDENTITY: &str = "ST2_PI_CHANNEL_IDENTITY";
/// The wrapper's runtime/task ID — the pty session whose liveness vouches for observed state.
pub const CHANNEL_RUNTIME_ID: &str = "ST2_PI_CHANNEL_RUNTIME_ID";
/// The session incarnation token the wrapper mints. The channel adopts it so the wrapper's
/// terminal record owns — and thereby fences — the live records the channel writes.
pub const CHANNEL_SESSION: &str = "ST2_PI_CHANNEL_SESSION";
/// The ownership sequence the wrapper claimed at startup — exported beside the token so the
/// channel's writes act under the same directional claim.
pub const CHANNEL_SEQ: &str = "ST2_PI_CHANNEL_SEQ";

/// pi's startup network work, which a supervised seat should not be doing.
///
/// A managed agent that update-checks or self-updates at boot makes its own launch latency and its
/// own behaviour depend on the network, and lets a release change a running fleet. Each is applied
/// only when the operator has not already set it, so a declaration's `env` still wins.
const OFFLINE_DEFAULTS: [(&str, &str); 2] = [("PI_OFFLINE", "1"), ("PI_SKIP_VERSION_CHECK", "1")];

/// Run one interactive pi provider and maintain its presence until it exits.
pub fn run(
    catalog_root: &Path,
    identity: String,
    runtime_id: String,
    pi_argv: Vec<String>,
) -> Result<()> {
    let agent_dir =
        message::resolve_declared_dir(catalog_root, &identity, &crate::run::detect_host())?
            .with_context(|| format!("pi driver agent '{identity}' is not declared"))?;
    anyhow::ensure!(
        !pi_argv.is_empty(),
        "pi driver '{runtime_id}' has no provider argv"
    );
    let executable =
        std::env::current_exe().context("resolving st2 executable for the pi channel")?;
    let session = harness_state::session_token();
    // The claim is written: it supersedes whatever the predecessor left — including a
    // still-fresh live record — before the channel or terminal writer act under it.
    let seq = harness_state::claim(&agent_dir, identity.clone(), "pi", &session)?;
    // Every fallible step past the claim must end the record honestly on failure — the claim
    // placeholder standing as the last word would read as a takeover, not a launch that never
    // ran.
    let prepared = (|| -> Result<(Vec<(String, String)>, Vec<String>)> {
        let mut env = channel_env(
            &executable,
            catalog_root,
            &identity,
            &runtime_id,
            &session,
            seq,
        )?;
        env.extend(offline_defaults(|key| std::env::var_os(key).is_some()));
        let set = hooks::verify_required_set().with_context(|| {
            format!(
                "pi driver '{runtime_id}' needs this binary's verified hook set for {EXTENSION}; run `st2 hooks install`"
            )
        })?;
        Ok((env, with_channel_extension(pi_argv, &set)?))
    })();
    let (env, pi_argv) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let mut writer = harness_state::Writer::new(
                &agent_dir,
                identity.clone(),
                "pi",
                Some(runtime_id.clone()),
            )
            .with_ownership(session.clone(), seq);
            let _ = writer.observe(
                harness_state::Observation::new(
                    harness_state::Activity::Ended,
                    harness_state::BlockedOn::None,
                    harness_state::InputBuffer::Unknown,
                )
                .with_reason("launch-error")
                .with_exit("exit unknown"),
            );
            return Err(error);
        }
    };
    install_signal_handler();
    // Terminal-only: the channel owns the live record and its heartbeat, but only this wrapper
    // survives long enough to see the stop path — its pre-escalation `ended` write is the one
    // that makes `Stopped(None)` observable at all. Same token as the channel, so the terminal
    // record fences exactly this session's live records.
    let observer = crate::provider_session::SessionObserver::terminal_only(
        &agent_dir,
        &identity,
        "pi",
        &runtime_id,
        &session,
        seq,
    );
    let outcome = run_provider_observed(
        "pi",
        &status::status_path(&agent_dir),
        &pi_argv,
        &env,
        status::STATUS_REFRESH,
        PROVIDER_POLL,
        &STOP,
        Some(&observer),
    )
    .with_context(|| format!("running pi driver '{runtime_id}'"))?;
    record_session_end(&agent_dir, &identity, &runtime_id, &session, seq, &outcome);
    match outcome {
        ProviderOutcome::Exited(exit) => {
            anyhow::ensure!(exit.success(), "pi provider exited with {exit}");
            Ok(())
        }
        ProviderOutcome::Stopped(_) => Ok(()),
    }
}

/// The wrapper's one write into observed harness state: the terminal record. Live states and
/// heartbeats belong to the pi channel, which sees pi's own turn events over stdio; the wrapper
/// sees exactly one fact the channel cannot — that the provider process is gone — so that is the
/// one fact it records. The `Writer` is constructed at the terminal edge on purpose: it re-reads
/// whatever the channel last wrote and continues its transition counter, and by the time the
/// wrapper has reaped pi the extension (and with it the channel) is already gone.
fn record_session_end(
    agent_dir: &Path,
    identity: &str,
    runtime_id: &str,
    session: &str,
    seq: u64,
    outcome: &ProviderOutcome,
) {
    let label = match outcome {
        ProviderOutcome::Exited(exit) | ProviderOutcome::Stopped(Some(exit)) => exit_label(*exit),
        ProviderOutcome::Stopped(None) => "stopped".to_string(),
    };
    let mut writer =
        harness_state::Writer::new(agent_dir, identity, "pi", Some(runtime_id.to_string()))
            .with_ownership(session, seq);
    if let Err(error) = writer.ended(label) {
        tracing::warn!("st2 pi driver: recording session end failed: {error}");
    }
}

fn exit_label(exit: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt as _;
    match (exit.code(), exit.signal()) {
        (Some(code), _) => format!("exit {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => "exited".to_string(),
    }
}

/// Load the channel extension from the verified set, immediately after the provider program.
///
/// The declaration deliberately carries no path to it: a rendered machine-local path would pin one
/// host's layout into a catalog, and a `$ST_HOOKS` token in an argv would resolve to the
/// receipt-bearing root rather than the selected set. Resolving it here means a launch uses the
/// exact asset this binary was built with.
fn with_channel_extension(mut argv: Vec<String>, set: &Path) -> Result<Vec<String>> {
    let extension = set.join(EXTENSION);
    let extension = extension
        .to_str()
        .context("verified hook set path is not UTF-8")?
        .to_owned();
    argv.splice(1..1, ["-e".to_string(), extension]);
    Ok(argv)
}

/// The offline defaults this launch should add, skipping any the operator already declared.
fn offline_defaults(is_set: impl Fn(&str) -> bool) -> Vec<(String, String)> {
    OFFLINE_DEFAULTS
        .iter()
        .filter(|(key, _)| !is_set(key))
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect()
}

/// The environment the shipped pi extension reads to reach this exact control plane.
fn channel_env(
    executable: &Path,
    catalog_root: &Path,
    identity: &str,
    runtime_id: &str,
    session: &str,
    seq: u64,
) -> Result<Vec<(String, String)>> {
    let executable = executable
        .to_str()
        .context("st2 executable path is not UTF-8")?;
    let catalog_root = catalog_root.to_str().context("catalog root is not UTF-8")?;
    Ok(vec![
        (CHANNEL_BIN.to_string(), executable.to_string()),
        (CHANNEL_CATALOG.to_string(), catalog_root.to_string()),
        (CHANNEL_IDENTITY.to_string(), identity.to_string()),
        (CHANNEL_RUNTIME_ID.to_string(), runtime_id.to_string()),
        (CHANNEL_SESSION.to_string(), session.to_string()),
        (CHANNEL_SEQ.to_string(), seq.to_string()),
    ])
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use super::*;
    use crate::provider_session::run_provider;

    #[test]
    fn idle_pi_provider_refreshes_presence_without_channel_input() {
        let tmp = tempfile::tempdir().unwrap();
        let presence = status::status_path(tmp.path());
        status::set_state(&presence, status::State::Available).unwrap();
        let before = fs::read_to_string(&presence).unwrap();
        let stop = AtomicBool::new(false);

        run_provider(
            "pi",
            &presence,
            // Deliberately cheaper than the Claude twin: this binary already runs one
            // spawn-and-poll presence test, and the codex app-server tests beside it are
            // timing-sensitive. One refresh inside the child's life is all this needs to prove.
            &["sh".into(), "-c".into(), "sleep 0.06".into()],
            &[],
            Duration::from_millis(10),
            Duration::from_millis(5),
            &stop,
            None,
        )
        .unwrap();

        let after = fs::read_to_string(&presence).unwrap();
        assert_ne!(after, before);
        assert_eq!(status::read_state(&presence), status::State::Available);
    }

    /// The wrapper writes the one observation the channel cannot: the terminal record, carrying
    /// the exit. It continues the transition counter of whatever the channel last wrote, so the
    /// death of a session is a transition in the same record, not a new history.
    #[test]
    fn provider_exit_writes_the_terminal_record_with_its_status() {
        use std::os::unix::process::ExitStatusExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path();
        let mut channel_writer =
            crate::harness_state::Writer::new(agent_dir, "h.worker", "pi", Some("h.worker".into()));
        channel_writer
            .observe(crate::harness_state::Observation::new(
                crate::harness_state::Activity::Active,
                crate::harness_state::BlockedOn::None,
                crate::harness_state::InputBuffer::Unknown,
            ))
            .unwrap();
        drop(channel_writer);

        record_session_end(
            agent_dir,
            "h.worker",
            "h.worker",
            "session-test",
            1,
            &ProviderOutcome::Exited(ExitStatus::from_raw(3 << 8)),
        );

        let record = crate::harness_state::harness_state_path(agent_dir);
        let observed = crate::harness_state::read(&record, None).unwrap();
        assert_eq!(observed.state, crate::harness_state::Activity::Ended);
        assert_eq!(observed.exit.as_deref(), Some("exit 3"));
        let raw: serde_json::Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
        assert_eq!(
            raw["transitions"], 1,
            "counter continues the channel's record"
        );

        record_session_end(
            agent_dir,
            "h.worker",
            "h.worker",
            "session-test",
            1,
            &ProviderOutcome::Stopped(Some(ExitStatus::from_raw(9))),
        );
        let observed = crate::harness_state::read(&record, None).unwrap();
        assert_eq!(observed.exit.as_deref(), Some("signal 9"));
    }

    /// The observed variant reports a nonzero exit instead of judging it, which is what lets the
    /// wrapper record the terminal state before failing the launch.
    #[test]
    fn run_provider_observed_reports_the_child_exit_status() {
        let tmp = tempfile::tempdir().unwrap();
        let stop = AtomicBool::new(false);
        let outcome = crate::provider_session::run_provider_observed(
            "pi",
            &status::status_path(tmp.path()),
            &["sh".into(), "-c".into(), "exit 3".into()],
            &[],
            Duration::from_secs(60),
            Duration::from_millis(5),
            &stop,
            None,
        )
        .unwrap();
        match outcome {
            ProviderOutcome::Exited(exit) => assert_eq!(exit.code(), Some(3)),
            other => panic!("expected an exit outcome, got {other:?}"),
        }
    }

    #[test]
    fn the_channel_extension_is_injected_from_the_verified_set_not_the_declaration() {
        let argv = with_channel_extension(
            vec![
                "pi".into(),
                "-a".into(),
                "--model".into(),
                "anthropic/opus".into(),
                "Start work.".into(),
            ],
            &PathBuf::from("/state/st2/hooks/sets/sha256-abc"),
        )
        .unwrap();

        assert_eq!(
            argv,
            vec![
                "pi",
                "-e",
                "/state/st2/hooks/sets/sha256-abc/pi-channel.ts",
                "-a",
                "--model",
                "anthropic/opus",
                "Start work.",
            ]
        );
    }

    /// A supervised seat is offline by default, but an operator who declared otherwise keeps their
    /// value — otherwise the wrapper would silently overrule the declaration.
    #[test]
    fn offline_defaults_apply_only_where_the_operator_declared_nothing() {
        assert_eq!(
            offline_defaults(|_| false),
            vec![
                ("PI_OFFLINE".to_string(), "1".to_string()),
                ("PI_SKIP_VERSION_CHECK".to_string(), "1".to_string()),
            ]
        );
        assert_eq!(
            offline_defaults(|key| key == "PI_OFFLINE"),
            vec![("PI_SKIP_VERSION_CHECK".to_string(), "1".to_string())]
        );
        assert!(offline_defaults(|_| true).is_empty());
    }

    #[test]
    fn the_extension_receives_this_binary_not_a_path_lookup() {
        let env = channel_env(
            &PathBuf::from("/opt/st2/bin/st2"),
            &PathBuf::from("/catalog"),
            "host.worker",
            "host.worker-task",
            "session-test",
            7,
        )
        .unwrap();

        assert_eq!(
            env,
            vec![
                (CHANNEL_BIN.to_string(), "/opt/st2/bin/st2".to_string()),
                (CHANNEL_CATALOG.to_string(), "/catalog".to_string()),
                (CHANNEL_IDENTITY.to_string(), "host.worker".to_string()),
                (
                    CHANNEL_RUNTIME_ID.to_string(),
                    "host.worker-task".to_string()
                ),
                (CHANNEL_SESSION.to_string(), "session-test".to_string()),
                (CHANNEL_SEQ.to_string(), "7".to_string()),
            ]
        );
    }

    /// W6: the terminal-only observer records how the session ended but never re-stamps live
    /// state — the channel owns the heartbeat — and its token makes the write this session's.
    #[test]
    fn the_terminal_only_observer_ends_but_never_heartbeats() {
        use crate::harness_state::{self, Activity};
        let tmp = tempfile::tempdir().unwrap();
        let record = harness_state::harness_state_path(tmp.path());
        let session = harness_state::session_token();
        // Real wiring order: the wrapper's written claim first, then the channel adopts it.
        let seq = harness_state::claim(tmp.path(), "h.worker", "pi", &session).unwrap();
        let mut channel =
            harness_state::Writer::new(tmp.path(), "h.worker", "pi", Some("h.worker".to_string()))
                .with_ownership(session.clone(), seq);
        channel
            .observe(harness_state::Observation::new(
                Activity::Active,
                harness_state::BlockedOn::None,
                harness_state::InputBuffer::Unknown,
            ))
            .unwrap();
        let live = std::fs::read(&record).unwrap();

        let observer = crate::provider_session::SessionObserver::terminal_only(
            tmp.path(),
            "h.worker",
            "pi",
            "h.worker",
            &session,
            seq,
        );
        observer.heartbeat();
        assert_eq!(std::fs::read(&record).unwrap(), live, "no heartbeat");

        observer.ended("signal 9");
        let observed = harness_state::read(&record, None).unwrap();
        assert_eq!(observed.state, Activity::Ended);
        assert_eq!(observed.exit.as_deref(), Some("signal 9"));
    }
}
