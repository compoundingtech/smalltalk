//! Controlled omp launch with a session-owned presence lease.
//!
//! omp is pi-family: its integration point is a pi-style extension loaded into the interactive
//! process, which reaches st2 by spawning `st2 driver omp-channel` (`hooks/omp-channel.ts`,
//! forked from the pi channel — see `docs/vrs/06-omp-driver/spec.md` for the measured
//! divergences). The wrapper owns presence for the same reason the pi wrapper does: the extension
//! lives only as long as omp's process, and a SIGKILL of omp produces no terminal record at all,
//! so presence decays by staleness exactly as for the other harnesses.
//!
//! Unlike pi, the wrapper hard-gates the provider version (OMP-R05): the delivery-critical
//! surface — event names, the sampled idle edge, the approval events — is versioned behavior, not
//! an API contract, so an unverified MINOR stays refused until the admission checks are repeated.
//! Patches inside an admitted minor launch without new evidence (decision 0007-omp-is-a-fifth-native-driver-with-its-own-channel-and-a-hard-version-gate).

use std::path::Path;
use std::process::ExitStatus;

use anyhow::{Context as _, Result};

use crate::provider_session::{
    install_signal_handler, run_provider_observed, ProviderOutcome, PROVIDER_POLL, STOP,
};
use crate::{harness_state, harness_version, hooks, message, status};

/// The extension file inside this binary's immutable hook set.
const EXTENSION: &str = "omp-channel.ts";

/// The exact st2 executable the omp extension must spawn for its channel.
pub const CHANNEL_BIN: &str = "ST2_OMP_CHANNEL_BIN";
/// The catalog root that executable must be pointed at.
pub const CHANNEL_CATALOG: &str = "ST2_OMP_CHANNEL_CATALOG";
/// The host-qualified bus identity the channel binds.
pub const CHANNEL_IDENTITY: &str = "ST2_OMP_CHANNEL_IDENTITY";
/// The wrapper's runtime/task ID — the pty session whose liveness vouches for observed state.
pub const CHANNEL_RUNTIME_ID: &str = "ST2_OMP_CHANNEL_RUNTIME_ID";
/// The session incarnation token the wrapper mints. The channel adopts it so the wrapper's
/// terminal record owns — and thereby fences — the live records the channel writes.
pub const CHANNEL_SESSION: &str = "ST2_OMP_CHANNEL_SESSION";
/// The ownership sequence the wrapper claimed at startup.
pub const CHANNEL_SEQ: &str = "ST2_OMP_CHANNEL_SEQ";

/// omp reads its pi ancestor's env fallbacks, so the same offline defaults apply. Whether they
/// suppress the update banner in interactive boots is still open (DQ-OMP-5); shipping them is
/// harmless either way.
const OFFLINE_DEFAULTS: [(&str, &str); 2] = [("PI_OFFLINE", "1"), ("PI_SKIP_VERSION_CHECK", "1")];

/// The omp MINORS verified against the admission checks in `docs/vrs/06-omp-driver/spec.md`.
///
/// 18.0 was measured twice: at 18.0.3 on 2026-08-25 and again at 18.0.9 on 2026-08-28.
/// 18.1 was measured at 18.1.2 on 2026-09-02.
///
/// Admission is per minor, per decision 0007-omp-is-a-fifth-native-driver-with-its-own-channel-and-a-hard-version-gate ("hard version gate on the minor, 18.x initially")
/// and OMP-R05 ("a later minor stays rejected"). Any patch inside an admitted minor launches
/// without new evidence: omp releases near-daily, so gating patches blocked the fleet on changes
/// the capture already covered — 18.0.10 shipped within hours of 18.0.9 being admitted. A new
/// MINOR still costs the five OMP-R05 probes.
const SUPPORTED_OMP_MINORS: [(u32, u32); 2] = [(18, 0), (18, 1)];

/// The omp builds the harness-context producer's arithmetic was measured against (HC-R13, HC-T03).
///
/// This is a different question from the launch gate above and the two must not be collapsed. The
/// gate admits a MINOR SERIES, because a patch inside an admitted minor costs no new evidence; the
/// fixture pins the EXACT BUILDS a number's meaning was measured on, because omp's `tokens` being
/// prompt-only input is a property of a specific build and not of any documented contract. Both
/// builds were probed on 2026-08-29 and agree, which is what makes the minor gate defensible here.
/// `the_measured_context_builds_are_admitted_by_this_gate` keeps them from drifting apart.
pub const MEASURED_CONTEXT_VERSIONS: [&str; 2] = ["18.0.9", "18.0.3"];

/// What the wrapper hands the provider process: the channel environment plus the launch argv with
/// the channel extension spliced in.
type PreparedLaunch = (Vec<(String, String)>, Vec<String>);

/// Run one interactive omp provider and maintain its presence until it exits.
pub fn run(
    catalog_root: &Path,
    identity: String,
    runtime_id: String,
    omp_argv: Vec<String>,
) -> Result<()> {
    let agent_dir =
        message::resolve_declared_dir(catalog_root, &identity, &crate::run::detect_host())?
            .with_context(|| format!("omp driver agent '{identity}' is not declared"))?;
    anyhow::ensure!(
        !omp_argv.is_empty(),
        "omp driver '{runtime_id}' has no provider argv"
    );
    verify_supported_version(&omp_argv[0])?;
    let executable =
        std::env::current_exe().context("resolving st2 executable for the omp channel")?;
    let session = harness_state::session_token();
    // The claim is written: it supersedes whatever the predecessor left — including a
    // still-fresh live record — before the channel or terminal writer act under it.
    let seq = harness_state::claim(&agent_dir, identity.clone(), "omp", &session)?;
    // Every fallible step past the claim must end the record honestly on failure — the claim
    // placeholder standing as the last word would read as a takeover, not a launch that never
    // ran.
    let prepared = (|| -> Result<PreparedLaunch> {
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
                "omp driver '{runtime_id}' needs this binary's verified hook set for {EXTENSION}; run `st2 hooks install`"
            )
        })?;
        Ok((env, with_channel_extension(omp_argv, &set)?))
    })();
    let (env, omp_argv) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let mut writer = harness_state::Writer::new(
                &agent_dir,
                identity.clone(),
                "omp",
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
    // survives long enough to see the stop path. Same token as the channel, so the terminal
    // record fences exactly this session's live records.
    let observer = crate::provider_session::SessionObserver::terminal_only(
        &agent_dir,
        &identity,
        "omp",
        &runtime_id,
        &session,
        seq,
    );
    let outcome = run_provider_observed(
        "omp",
        &status::status_path(&agent_dir),
        &omp_argv,
        &env,
        status::STATUS_REFRESH,
        PROVIDER_POLL,
        &STOP,
        Some(&observer),
    )
    .with_context(|| format!("running omp driver '{runtime_id}'"))?;
    record_session_end(&agent_dir, &identity, &runtime_id, &session, seq, &outcome);
    match outcome {
        ProviderOutcome::Exited(exit) => {
            anyhow::ensure!(exit.success(), "omp provider exited with {exit}");
            Ok(())
        }
        ProviderOutcome::Stopped(_) => Ok(()),
    }
}

/// Refuse any provider whose MINOR this binary was not verified against. Failing loudly at launch
/// is the point (OMP-R05): a silently degraded observed state or delivery path would read as
/// healthy. Patches inside an admitted minor pass, because the admission unit is the minor.
fn verify_supported_version(binary: &str) -> Result<()> {
    let output = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .with_context(|| format!("running {binary} --version for the omp version gate"))?;
    anyhow::ensure!(output.status.success(), "{binary} --version failed");
    // omp prefixes its banner ("omp/18.0.3"), so scan every whitespace-separated token for the
    // first MAJOR.MINOR.PATCH release rather than trusting line order.
    let printed = String::from_utf8_lossy(&output.stdout);
    let (version, release) = harness_version::find_release(&printed, "omp").with_context(|| {
        format!("{binary} --version reported no unambiguous omp release: '{printed}'")
    })?;
    anyhow::ensure!(
        SUPPORTED_OMP_MINORS.contains(&release.series()),
        "omp {version} is unverified (admitted minors: {}); repeat the docs/vrs/06-omp-driver \
         admission checks before extending the gate",
        harness_version::series_display(&SUPPORTED_OMP_MINORS)
    );
    Ok(())
}

/// The wrapper's one write into observed harness state: the terminal record. Live states and
/// heartbeats belong to the omp channel; the wrapper sees exactly one fact the channel cannot —
/// that the provider process is gone.
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
        harness_state::Writer::new(agent_dir, identity, "omp", Some(runtime_id.to_string()))
            .with_ownership(session, seq);
    if let Err(error) = writer.ended(label) {
        eprintln!("st2 omp driver: recording session end failed: {error}");
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
/// Resolving it here means a launch uses the exact asset this binary was built with; a rendered
/// machine-local path in a declaration would pin one host's layout into a catalog.
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

/// The environment the shipped omp extension reads to reach this exact control plane.
///
/// Fresh variable names: an omp seat must never adopt a stray pi channel configuration.
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
    use super::*;
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::Barrier;

    struct FakeExecutable {
        _directory: tempfile::TempDir,
        path: PathBuf,
    }

    impl FakeExecutable {
        fn new(body: &str) -> Self {
            let directory = tempfile::Builder::new()
                .prefix("st2-omp-version-")
                .tempdir()
                .unwrap();
            let source = directory.path().join("omp.source");
            let path = directory.path().join("omp");
            std::fs::write(&source, body).unwrap();
            // A writer opened by one libtest thread is inherited by a child forked concurrently
            // from another, which can make the writer's later exec fail with ETXTBSY even after
            // the parent closes it. Let `install` create and close the executable in its own child:
            // the test process never owns a writable descriptor for the file it will execute.
            let output = std::process::Command::new("install")
                .args(["-m", "755"])
                .arg(&source)
                .arg(&path)
                .output()
                .unwrap();
            assert!(output.status.success(), "install failed: {output:?}");
            Self {
                _directory: directory,
                path,
            }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    /// Pins the admitted set itself — the intent carried over from the exact-version gate.
    /// Iterating the constant cannot catch a minor that was added without measuring it, so the
    /// set is asserted literally: widening has to be a deliberate edit here, next to the
    /// `.experiments/` capture that justifies it.
    #[test]
    fn admitted_minors_are_exactly_the_measured_set() {
        assert_eq!(SUPPORTED_OMP_MINORS, [(18, 0), (18, 1)]);
    }

    /// Every exact build that admitted a minor must still launch. Keeping the literals here makes
    /// each OMP-R05 capture a deliberate part of the gate rather than inferring evidence from the
    /// admitted series.
    #[test]
    fn version_gate_admits_every_admission_capture() {
        for version in ["18.0.3", "18.0.9", "18.1.2"] {
            let fake = FakeExecutable::new(&format!(
                "#!/bin/sh\nprintf 'omp v{version}\\n{version}\\n'\n"
            ));
            verify_supported_version(fake.path().to_str().unwrap())
                .unwrap_or_else(|error| panic!("{version} must be admitted: {error}"));
        }
    }

    /// The launch gate and the harness-context fixture answer different questions — a minor series
    /// versus the exact builds a number's meaning was measured on — and they are allowed to differ.
    /// What they may not do is drift apart silently: a build the fixture claims to have measured
    /// but this gate would refuse to launch is evidence for a version the fleet can never run, and
    /// that is exactly the shape HC-T03 asks the gate discipline to bound.
    #[test]
    fn the_measured_context_builds_are_admitted_by_this_gate() {
        for version in MEASURED_CONTEXT_VERSIONS {
            let release = harness_version::parse_release(version)
                .unwrap_or_else(|| panic!("{version} must be a parseable release"));
            assert!(
                SUPPORTED_OMP_MINORS.contains(&release.series()),
                "the harness-context fixture measured {version}, a build this gate refuses to \
                 launch (admitted minors: {})",
                harness_version::series_display(&SUPPORTED_OMP_MINORS)
            );
        }
    }

    /// A pre-release must not be admitted as its base release, even when its base minor is
    /// admitted: it is not the build any capture measured.
    #[test]
    fn version_gate_refuses_a_prerelease_inside_an_admitted_minor() {
        for version in ["18.0.9-rc1", "18.0.9+meta", "18.1.2-rc1", "18.1.2+meta"] {
            let fake = FakeExecutable::new(&format!("#!/bin/sh\nprintf '{version}\\n'\n"));
            assert!(
                verify_supported_version(fake.path().to_str().unwrap()).is_err(),
                "{version} must not be admitted as its base release"
            );
        }
    }

    /// A banner mentioning some other version must not bind the gate to it. Reported in review of
    /// #370: `runtime 18.0.0 omp/18.2.0` would otherwise admit on the unrelated `18.0.0` and then
    /// launch an unverified 18.2 provider. The provider's own label decides; an unlabelled banner
    /// carrying two different releases fails closed.
    #[test]
    fn a_stray_version_in_the_banner_cannot_admit_an_unverified_provider() {
        let fake = FakeExecutable::new("#!/bin/sh\nprintf 'runtime 18.0.0 omp/18.2.0\\n'\n");
        let error = verify_supported_version(fake.path().to_str().unwrap())
            .expect_err("the omp-labelled 18.2.0 must decide, not the stray 18.0.0")
            .to_string();
        assert!(
            error.contains("18.2.0"),
            "must name the provider's own release: {error}"
        );

        let ambiguous = FakeExecutable::new("#!/bin/sh\nprintf 'runtime 18.0.0 18.2.0\\n'\n");
        assert!(
            verify_supported_version(ambiguous.path().to_str().unwrap()).is_err(),
            "an unlabelled banner with two different releases must fail closed"
        );
    }

    /// The refusal must survive omp naming itself with something unreadable. Found by independent
    /// verification of #370 (mutation S6): deleting the labelled fall-through left the suite
    /// byte-identical, and driving this gate with `omp/18.1.0-rc1 18.0.9` ADMITTED the launch,
    /// bound to the stray token rather than to the release omp reported for itself. Latent while
    /// the shipped binary prints one token, but DQ-OMP-5 is open on precisely the update banner
    /// that would add a second.
    #[test]
    fn an_unreadable_own_label_cannot_be_rescued_by_a_stray_admitted_version() {
        let fake = FakeExecutable::new("#!/bin/sh\nprintf 'omp/18.1.0-rc1 18.0.9\\n'\n");
        let error = verify_supported_version(fake.path().to_str().unwrap())
            .expect_err("an unreadable own label must not admit on a stray 18.0.9")
            .to_string();
        assert!(
            error.contains("no unambiguous omp release"),
            "the refusal must say the banner named no release it could read: {error}"
        );
    }

    /// Minors are compared as numbers. `18.10` must not pass on the strength of admitted `18.1`,
    /// which is how a minor gate would decay into "accept anything that starts with 18".
    #[test]
    fn version_gate_refuses_a_neighbouring_minor_that_shares_a_prefix() {
        for version in ["18.10.0", "18.2.0"] {
            let fake = FakeExecutable::new(&format!("#!/bin/sh\nprintf '{version}\\n'\n"));
            let error = verify_supported_version(fake.path().to_str().unwrap())
                .expect_err(version)
                .to_string();
            assert!(error.contains("unverified"), "{version}: {error}");
        }
    }

    /// THE assertion this lane exists for: a patch inside an already-admitted minor must launch
    /// without a new capture (decision 0007-omp-is-a-fifth-native-driver-with-its-own-channel-and-a-hard-version-gate gates on the minor). Fails against the exact-version
    /// allowlist; passes once the gate keys on MAJOR.MINOR.
    #[test]
    fn a_patch_inside_an_admitted_minor_is_accepted_without_new_evidence() {
        for version in ["18.0.11", "18.1.99"] {
            let fake = FakeExecutable::new(&format!(
                "#!/bin/sh\nprintf 'omp v{version}\\n{version}\\n'\n"
            ));
            verify_supported_version(fake.path().to_str().unwrap())
                .unwrap_or_else(|error| panic!("{version} is inside an admitted minor: {error}"));
        }
    }

    #[test]
    fn version_gate_refuses_an_unverified_minor() {
        let fake = FakeExecutable::new("#!/bin/sh\nprintf '18.2.0\\n'\n");
        let error = verify_supported_version(fake.path().to_str().unwrap()).unwrap_err();
        assert!(error.to_string().contains("unverified"), "{error}");
    }

    #[test]
    fn version_gate_refuses_an_unverified_major() {
        let fake = FakeExecutable::new("#!/bin/sh\nprintf '19.0.1\\n'\n");
        let error = verify_supported_version(fake.path().to_str().unwrap()).unwrap_err();
        assert!(error.to_string().contains("unverified"), "{error}");
    }

    #[test]
    fn version_gate_refuses_garbled_output() {
        let fake = FakeExecutable::new("#!/bin/sh\nprintf 'not-a-version\\n'\n");
        assert!(verify_supported_version(fake.path().to_str().unwrap()).is_err());
    }

    #[test]
    fn version_gate_fixtures_are_parallel_safe() {
        const WORKERS: usize = 8;
        const ROUNDS: usize = 16;

        let barrier = Barrier::new(WORKERS);
        let paths = std::thread::scope(|scope| {
            let mut workers = Vec::with_capacity(WORKERS);
            for _ in 0..WORKERS {
                let barrier = &barrier;
                workers.push(scope.spawn(move || {
                    let mut paths = Vec::with_capacity(ROUNDS);
                    barrier.wait();
                    for _ in 0..ROUNDS {
                        let fake = FakeExecutable::new("#!/bin/sh\nprintf '18.2.0\\n'\n");
                        paths.push(fake.path().to_path_buf());
                        let error =
                            verify_supported_version(fake.path().to_str().unwrap()).unwrap_err();
                        assert!(error.to_string().contains("unverified"), "{error}");
                    }
                    paths
                }));
            }
            workers
                .into_iter()
                .flat_map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(paths.iter().collect::<HashSet<_>>().len(), paths.len());
    }

    #[test]
    fn offline_defaults_skip_operator_declared_keys() {
        let defaults = offline_defaults(|key| key == "PI_OFFLINE");
        assert_eq!(
            defaults,
            vec![("PI_SKIP_VERSION_CHECK".to_string(), "1".to_string())]
        );
    }

    #[test]
    fn channel_extension_is_spliced_right_after_the_program() {
        let dir = tempfile::tempdir().unwrap();
        let argv =
            with_channel_extension(vec!["omp".into(), "--model".into(), "x".into()], dir.path())
                .unwrap();
        assert_eq!(argv[0], "omp");
        assert_eq!(argv[1], "-e");
        assert!(argv[2].ends_with("omp-channel.ts"));
        assert_eq!(argv[3], "--model");
    }
}
