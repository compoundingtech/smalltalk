//! The controlled-launch body shared by the pi-family wrappers (pi and omp).
//!
//! Both harnesses integrate the same way: an extension loaded into the interactive process, which
//! reaches st2 by spawning `st2 driver <harness>-channel`. Two facts have to be handed to that
//! extension, and neither is discoverable from inside the harness. The first is *which* st2 to run
//! — resolving `st2` from `PATH` would let a replaced control plane and its live agents disagree,
//! which the R11 control-plane replacement guarantee exists to prevent — so the wrapper exports its
//! own executable path. The second is the catalog and identity the channel must bind.
//!
//! Catalog wrappers also own their status lease for the same reason Claude does: the extension lives
//! only as long as the provider's process, and a measured SIGKILL of pi produces no terminal record
//! at all (`docs/vrs/.experiments/2026-08-18-pi-harness-integration.md`). Presence therefore decays
//! by staleness, exactly as for the other harnesses. Native launches omit that status transport
//! and retain session-owned observed records and terminal fencing.
//!
//! The fork between the two harnesses is a [`HarnessKind`] descriptor, mirroring the
//! [`crate::pi_channel::ChannelKind`] fork the family's channel side already uses: the same fork
//! solved the same way twice rather than two different ways. What stays in each harness module is
//! what is genuinely per-harness — its module doc, its extension asset, its channel env NAMES (the
//! whole point of two sets is that an omp seat can never adopt a stray pi configuration), and, for
//! omp, its version gate.

use std::path::Path;

use anyhow::{Context as _, Result};

use crate::provider_session::{
    PROVIDER_POLL, ProviderOutcome, STOP, describe_exit, install_signal_handler,
    run_provider_observed_with_env_removals,
};
use crate::{harness_state, hooks, message, status};

/// pi's startup network work, which a supervised seat should not be doing.
///
/// A managed agent that update-checks or self-updates at boot makes its own launch latency and its
/// own behaviour depend on the network, and lets a release change a running fleet. Each is applied
/// only when the operator has not already set it, so a declaration's `env` still wins.
///
/// omp reads its pi ancestor's env fallbacks, so the same defaults apply to it. Whether they
/// suppress the update banner in interactive boots is still open (DQ-OMP-5); shipping them is
/// harmless either way.
const OFFLINE_DEFAULTS: [(&str, &str); 2] = [("PI_OFFLINE", "1"), ("PI_SKIP_VERSION_CHECK", "1")];

/// A managed omp seat already has an explicit model, credentials, prompt, and policy. Its
/// interactive first-run wizard would sit in front of that prompt while the channel appeared
/// alive, so an unattended launch skips the wizard unless the operator explicitly chose another
/// value.
const OMP_UNATTENDED_DEFAULTS: [(&str, &str); 1] = [("OMP_SKIP_SETUP", "1")];

/// The harness-specific facts the shared wrapper body needs: the label that goes on records and
/// errors, the extension asset to inject, the env names the shipped extension reads, and the
/// launch gate — if any — this harness enforces.
pub(crate) struct HarnessKind {
    /// The harness word on observed records, claims and launch errors.
    pub(crate) label: &'static str,
    /// The extension file inside this binary's immutable hook set.
    pub(crate) extension: &'static str,
    /// The exact st2 executable the extension must spawn for its channel.
    pub(crate) bin_env: &'static str,
    /// The catalog root that executable must be pointed at.
    pub(crate) catalog_env: &'static str,
    /// The host-qualified bus identity the channel binds.
    pub(crate) identity_env: &'static str,
    /// The wrapper's runtime/task ID — the pty session whose liveness vouches for observed state.
    pub(crate) runtime_id_env: &'static str,
    /// The session incarnation token the wrapper mints.
    pub(crate) session_env: &'static str,
    /// The ownership sequence the wrapper claimed at startup.
    pub(crate) seq_env: &'static str,
    /// The launch-time measured producer/extension admission gate (OMP-R05). Pi has no
    /// runtime admission contract, so its slot is `None`.
    pub(crate) verify_version: Option<fn(&str, &Path, &Path) -> Result<()>>,
}

/// What the wrapper hands the provider process: the channel environment plus the launch argv with
/// the channel extension spliced in.
type PreparedLaunch = (Vec<(String, String)>, Vec<String>);

/// Run one ordinary interactive pi-family provider.
pub(crate) fn run_for(
    catalog_root: &Path,
    identity: String,
    runtime_id: String,
    provider_argv: Vec<String>,
    kind: &HarnessKind,
) -> Result<()> {
    run_for_with_environment(
        catalog_root,
        identity,
        runtime_id,
        provider_argv,
        kind,
        &[],
        &[],
        None,
        true,
    )
}

/// Run one interactive pi-family provider and maintain its presence until it exits.
pub(crate) fn run_for_with_environment(
    catalog_root: &Path,
    identity: String,
    runtime_id: String,
    provider_argv: Vec<String>,
    kind: &HarnessKind,
    additional_env: &[(String, String)],
    removed_env: &[&str],
    required_incarnation: Option<String>,
    legacy_presence: bool,
) -> Result<()> {
    let label = kind.label;
    let agent_dir =
        message::resolve_declared_dir(catalog_root, &identity, &crate::run::detect_host())?
            .with_context(|| format!("{label} driver agent '{identity}' is not declared"))?;
    run_for_paths(
        catalog_root,
        &agent_dir,
        identity,
        runtime_id,
        provider_argv,
        kind,
        additional_env,
        removed_env,
        required_incarnation,
        legacy_presence,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_for_paths(
    catalog_root: &Path,
    agent_dir: &Path,
    identity: String,
    runtime_id: String,
    provider_argv: Vec<String>,
    kind: &HarnessKind,
    additional_env: &[(String, String)],
    removed_env: &[&str],
    required_incarnation: Option<String>,
    legacy_presence: bool,
) -> Result<()> {
    let label = kind.label;
    anyhow::ensure!(
        !provider_argv.is_empty(),
        "{label} driver '{runtime_id}' has no provider argv"
    );
    // Before the claim on purpose: an unadmitted provider must fail without taking ownership of
    // the seat's observed record, so a refused launch leaves the predecessor's state alone.
    let admitted_set = if let Some(verify_version) = kind.verify_version {
        let set = channel_extension_set(kind, &runtime_id)?;
        verify_version(&provider_argv[0], &agent_dir, &set.join(kind.extension))?;
        Some(set)
    } else {
        None
    };
    let executable = std::env::current_exe()
        .with_context(|| format!("resolving st executable for the {label} channel"))?;
    let session = required_incarnation.unwrap_or_else(harness_state::session_token);
    // The claim is written: it supersedes whatever the predecessor left — including a
    // still-fresh live record — before the channel or terminal writer act under it.
    let seq = harness_state::claim(agent_dir, identity.clone(), label, &session)?;
    // Every fallible step past the claim must end the record honestly on failure — the claim
    // placeholder standing as the last word would read as a takeover, not a launch that never
    // ran.
    let prepared = (|| -> Result<PreparedLaunch> {
        let mut env = channel_env(
            kind,
            &executable,
            catalog_root,
            &identity,
            &runtime_id,
            &session,
            seq,
        )?;
        if !legacy_presence {
            env.retain(|(name, _)| name != kind.catalog_env);
        }
        env.extend_from_slice(additional_env);
        env.extend(offline_defaults(|key| std::env::var_os(key).is_some()));
        if label == "omp" {
            env.extend(omp_unattended_defaults(|key| {
                std::env::var_os(key).is_some()
            }));
        }
        let managed_push = std::env::var("ST3_MAILBOX_TRANSPORT").as_deref() == Ok("push");
        if managed_push {
            for (key, _) in &mut env {
                *key = key.replacen("ST2_", "ST_", 1);
            }
        }
        let set = match &admitted_set {
            Some(set) => set.clone(),
            None => channel_extension_set(kind, &runtime_id)?,
        };
        // Pi otherwise derives one project directory beneath the global credential profile. Two
        // supervised seats that start together can then race creation of that directory and one
        // provider accepts a turn without ever creating its transcript. A seat-owned directory is
        // deterministic, exists before launch, and is also the right inventory boundary for
        // listing/importing unmanaged sessions later.
        let session_dir = match additional_env
            .iter()
            .find(|(name, _)| name == crate::driver_paths::SESSION_DIR_ENV)
        {
            Some((_, path)) if !legacy_presence => {
                std::path::PathBuf::from(path).join("provider-sessions")
            }
            _ => agent_dir.join("provider-sessions"),
        };
        std::fs::create_dir_all(&session_dir).with_context(|| {
            format!(
                "creating {label} driver session directory {}",
                session_dir.display()
            )
        })?;
        let provider_argv = with_channel_extension(provider_argv, &set, kind.extension)?;
        Ok((env, with_managed_session_dir(provider_argv, &session_dir)?))
    })();
    let (env, provider_argv) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let mut writer = harness_state::Writer::new(
                agent_dir,
                identity.clone(),
                label,
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
        agent_dir,
        &identity,
        label,
        &runtime_id,
        &session,
        seq,
    );
    let mut native_removals = removed_env.to_vec();
    if !legacy_presence {
        native_removals.extend([kind.catalog_env, "CATALOG", "ST_ROOT"]);
    }
    let outcome = run_provider_observed_with_env_removals(
        label,
        legacy_presence
            .then(|| status::status_path(agent_dir))
            .as_deref(),
        &provider_argv,
        &env,
        &native_removals,
        crate::provider_session::SESSION_REFRESH,
        PROVIDER_POLL,
        &STOP,
        Some(&observer),
    )
    .with_context(|| format!("running {label} driver '{runtime_id}'"))?;
    finish_session(
        agent_dir,
        &identity,
        &runtime_id,
        &session,
        seq,
        outcome,
        kind,
    )
}

/// Resume supervising a pi-family provider a predecessor driver image launched and released for
/// adoption. The channel the extension spawned keeps the live record; this image keeps presence
/// and the terminal record, exactly as the launching image did.
pub(crate) fn adopt_for(
    catalog_root: &Path,
    identity: String,
    runtime_id: String,
    kind: &HarnessKind,
    pid: u32,
    session: String,
    seq: u64,
    legacy_presence: bool,
) -> Result<()> {
    let label = kind.label;
    let agent_dir =
        message::resolve_declared_dir(catalog_root, &identity, &crate::run::detect_host())?
            .with_context(|| format!("{label} driver agent '{identity}' is not declared"))?;
    adopt_paths(
        &agent_dir,
        identity,
        runtime_id,
        kind,
        pid,
        session,
        seq,
        legacy_presence,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn adopt_paths(
    agent_dir: &Path,
    identity: String,
    runtime_id: String,
    kind: &HarnessKind,
    pid: u32,
    session: String,
    seq: u64,
    legacy_presence: bool,
) -> Result<()> {
    let label = kind.label;
    let observer = crate::provider_session::SessionObserver::terminal_only(
        agent_dir,
        &identity,
        label,
        &runtime_id,
        &session,
        seq,
    );
    let outcome = crate::provider_session::adopt_provider_observed(
        label,
        legacy_presence
            .then(|| status::status_path(agent_dir))
            .as_deref(),
        pid,
        crate::provider_session::SESSION_REFRESH,
        PROVIDER_POLL,
        &STOP,
        Some(&observer),
    )
    .with_context(|| format!("supervising adopted {label} driver '{runtime_id}'"))?;
    finish_session(
        agent_dir,
        &identity,
        &runtime_id,
        &session,
        seq,
        outcome,
        kind,
    )
}

fn finish_session(
    agent_dir: &Path,
    identity: &str,
    runtime_id: &str,
    session: &str,
    seq: u64,
    outcome: ProviderOutcome,
    kind: &HarnessKind,
) -> Result<()> {
    let label = kind.label;
    if let ProviderOutcome::Detached(pid) = outcome {
        // The provider keeps running under the next driver image; it has not ended.
        return Err(crate::provider_session::Detached {
            session: crate::provider_session::DetachedSession::Provider {
                pid,
                session: session.to_owned(),
                seq,
            },
        }
        .into());
    }
    record_session_end(
        agent_dir, identity, runtime_id, session, seq, &outcome, kind,
    );
    match outcome {
        ProviderOutcome::Exited(exit) => {
            anyhow::ensure!(exit.success(), "{label} provider exited with {exit}");
            Ok(())
        }
        ProviderOutcome::Stopped(_) | ProviderOutcome::Detached(_) => Ok(()),
    }
}

/// The wrapper's one write into observed harness state: the terminal record. Live states and
/// heartbeats belong to the harness channel, which sees the provider's own turn events over stdio;
/// the wrapper sees exactly one fact the channel cannot — that the provider process is gone — so
/// that is the one fact it records. The `Writer` is constructed at the terminal edge on purpose: it
/// re-reads whatever the channel last wrote and continues its transition counter, and by the time
/// the wrapper has reaped the provider the extension (and with it the channel) is already gone.
fn record_session_end(
    agent_dir: &Path,
    identity: &str,
    runtime_id: &str,
    session: &str,
    seq: u64,
    outcome: &ProviderOutcome,
    kind: &HarnessKind,
) {
    let label = match outcome {
        ProviderOutcome::Exited(exit) | ProviderOutcome::Stopped(Some(exit)) => {
            describe_exit(*exit)
        }
        ProviderOutcome::Stopped(None) => "stopped".to_string(),
        ProviderOutcome::Detached(_) => return,
    };
    let mut writer = harness_state::Writer::new(
        agent_dir,
        identity,
        kind.label,
        Some(runtime_id.to_string()),
    )
    .with_ownership(session, seq);
    if let Err(error) = writer.ended(label) {
        tracing::warn!(
            "st {} driver: recording session end failed: {error}",
            kind.label
        );
    }
}

fn channel_extension_set(kind: &HarnessKind, runtime_id: &str) -> Result<std::path::PathBuf> {
    // st3 publishes a marked set with its environment substitutions applied. Admission and
    // the live launch must select that same asset rather than looking for an st2-only receipt.
    let exported = if std::env::var("ST3_MAILBOX_TRANSPORT").as_deref() == Ok("push") {
        std::env::var_os("ST_HOOKS")
            .map(std::path::PathBuf::from)
            .filter(|dir| {
                dir.join(hooks::ST3_SET_MARKER).is_file() && dir.join(kind.extension).is_file()
            })
    } else {
        hooks::exported_set_holding(kind.extension)
    };
    match exported {
        Some(set) => Ok(set),
        None => hooks::verify_required_set().with_context(|| {
            format!(
                "{} driver '{runtime_id}' needs this binary's verified hook set for {}; the daemon publishes it when it starts",
                kind.label, kind.extension
            )
        }),
    }
}

/// Load the channel extension from the verified set, immediately after the provider program.
///
/// The declaration deliberately carries no path to it: a rendered machine-local path would pin one
/// host's layout into a catalog, and a `$ST_HOOKS` token in an argv would resolve to the
/// receipt-bearing root rather than the selected set. Resolving it here means a launch uses the
/// exact asset this binary was built with.
fn with_channel_extension(
    mut argv: Vec<String>,
    set: &Path,
    extension: &str,
) -> Result<Vec<String>> {
    let extension = set.join(extension);
    let extension = extension
        .to_str()
        .context("verified hook set path is not UTF-8")?
        .to_owned();
    argv.splice(1..1, ["-e".to_string(), extension]);
    Ok(argv)
}

/// Pin provider transcripts to the exact managed seat instead of the shared credential profile.
fn with_managed_session_dir(mut argv: Vec<String>, session_dir: &Path) -> Result<Vec<String>> {
    let session_dir = session_dir
        .to_str()
        .context("managed pi-family session directory is not UTF-8")?
        .to_owned();
    argv.splice(1..1, ["--session-dir".to_string(), session_dir]);
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

fn omp_unattended_defaults(is_set: impl Fn(&str) -> bool) -> Vec<(String, String)> {
    OMP_UNATTENDED_DEFAULTS
        .iter()
        .filter(|(key, _)| !is_set(key))
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect()
}

/// The environment the shipped extension reads to reach this exact control plane.
///
/// The names come from the descriptor rather than being shared: an omp seat must never adopt a
/// stray pi channel configuration.
fn channel_env(
    kind: &HarnessKind,
    executable: &Path,
    catalog_root: &Path,
    identity: &str,
    runtime_id: &str,
    session: &str,
    seq: u64,
) -> Result<Vec<(String, String)>> {
    let executable = executable
        .to_str()
        .context("st executable path is not UTF-8")?;
    let catalog_root = catalog_root.to_str().context("catalog root is not UTF-8")?;
    Ok(vec![
        (kind.bin_env.to_string(), executable.to_string()),
        (kind.catalog_env.to_string(), catalog_root.to_string()),
        (kind.identity_env.to_string(), identity.to_string()),
        (kind.runtime_id_env.to_string(), runtime_id.to_string()),
        (kind.session_env.to_string(), session.to_string()),
        (kind.seq_env.to_string(), seq.to_string()),
    ])
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::process::ExitStatus;

    use super::*;
    use crate::omp_session::OMP_KIND;
    use crate::pi_session::PI_KIND;

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
            &PI_KIND,
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
            &PI_KIND,
        );
        let observed = crate::harness_state::read(&record, None).unwrap();
        assert_eq!(observed.exit.as_deref(), Some("signal 9"));
    }

    /// Each harness injects its OWN extension asset from the verified set, not a path the
    /// declaration carried, and it lands immediately after the provider program.
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
            PI_KIND.extension,
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

        let argv = with_channel_extension(
            vec!["omp".into(), "--model".into(), "x".into()],
            &PathBuf::from("/state/st2/hooks/sets/sha256-abc"),
            OMP_KIND.extension,
        )
        .unwrap();

        assert_eq!(
            argv,
            vec![
                "omp",
                "-e",
                "/state/st2/hooks/sets/sha256-abc/omp-channel.ts",
                "--model",
                "x",
            ]
        );
    }

    #[test]
    fn every_managed_seat_gets_an_explicit_session_directory() {
        let argv = with_managed_session_dir(
            vec![
                "pi".into(),
                "-e".into(),
                "/hooks/pi-channel.ts".into(),
                "Start work.".into(),
            ],
            &PathBuf::from("/agents/worker/provider-sessions"),
        )
        .unwrap();

        assert_eq!(
            argv,
            vec![
                "pi",
                "--session-dir",
                "/agents/worker/provider-sessions",
                "-e",
                "/hooks/pi-channel.ts",
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

        assert_eq!(
            omp_unattended_defaults(|_| false),
            vec![("OMP_SKIP_SETUP".to_string(), "1".to_string())]
        );
        assert!(omp_unattended_defaults(|key| key == "OMP_SKIP_SETUP").is_empty());
    }

    #[test]
    fn the_extension_receives_this_binary_not_a_path_lookup() {
        let env = channel_env(
            &PI_KIND,
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
                (
                    crate::pi_session::CHANNEL_BIN.to_string(),
                    "/opt/st2/bin/st2".to_string()
                ),
                (
                    crate::pi_session::CHANNEL_CATALOG.to_string(),
                    "/catalog".to_string()
                ),
                (
                    crate::pi_session::CHANNEL_IDENTITY.to_string(),
                    "host.worker".to_string()
                ),
                (
                    crate::pi_session::CHANNEL_RUNTIME_ID.to_string(),
                    "host.worker-task".to_string()
                ),
                (
                    crate::pi_session::CHANNEL_SESSION.to_string(),
                    "session-test".to_string()
                ),
                (crate::pi_session::CHANNEL_SEQ.to_string(), "7".to_string()),
            ]
        );
    }

    /// The two harnesses carry DISJOINT env names on purpose: an omp seat that inherited a stray
    /// pi channel configuration would point its channel at another harness's control plane.
    #[test]
    fn the_two_harnesses_export_disjoint_channel_variable_names() {
        let names = |kind: &HarnessKind| {
            channel_env(
                kind,
                &PathBuf::from("/opt/st2/bin/st2"),
                &PathBuf::from("/catalog"),
                "host.worker",
                "host.worker-task",
                "session-test",
                7,
            )
            .unwrap()
            .into_iter()
            .map(|(key, _)| key)
            .collect::<Vec<_>>()
        };
        let pi = names(&PI_KIND);
        let omp = names(&OMP_KIND);
        assert!(
            pi.iter().all(|name| name.starts_with("ST_PI_CHANNEL_")),
            "{pi:?}"
        );
        assert!(
            omp.iter().all(|name| name.starts_with("ST_OMP_CHANNEL_")),
            "{omp:?}"
        );
        assert!(
            pi.iter().all(|name| !omp.contains(name)),
            "the two harnesses must share no channel variable name"
        );
    }
}
