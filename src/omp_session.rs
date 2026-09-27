//! Controlled omp launch with a session-owned presence lease.
//!
//! omp is pi-family: its integration point is a pi-style extension loaded into the interactive
//! process, which reaches st2 by spawning `st2 driver omp-channel` (`hooks/omp-channel.ts`,
//! forked from the pi channel — see `docs/vrs/06-omp-driver/spec.md` for the measured
//! divergences). The launch body itself is shared with pi in [`crate::pi_family_session`].
//!
//! Unlike pi, the wrapper hard-gates the provider version (OMP-R05): the delivery-critical
//! surface — event names, the sampled idle edge, the approval events — is versioned behavior, not
//! an API contract, so an unverified MINOR stays refused until the admission checks are repeated.
//! Patches inside an admitted minor launch without new evidence (decision 0007-omp-is-a-fifth-native-driver-with-its-own-channel-and-a-hard-version-gate).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::harness_version;
use crate::pi_family_session::{self, HarnessKind};

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
/// The exact native session that a cold residency launch must resume.
pub const CHANNEL_EXPECTED_NATIVE_SESSION: &str = "ST2_OMP_CHANNEL_EXPECTED_NATIVE_SESSION";
/// The cold residency generation whose exact native session the channel must prove.
pub const CHANNEL_RESUME_GENERATION: &str = "ST2_OMP_CHANNEL_RESUME_GENERATION";

const BINDING_SCHEMA: &str = "st2.omp-session-binding.v1";
const CHECKPOINT_SCHEMA: &str = "st2.omp-residency-checkpoint.v1";
const BINDING_FILE: &str = "binding.json";
const PENDING_BINDING_FILE: &str = "binding.pending.json";
const CHECKPOINT_FILE: &str = "residency-checkpoint.json";
const RESUME_FENCE_ENV: [&str; 2] = [CHANNEL_EXPECTED_NATIVE_SESSION, CHANNEL_RESUME_GENERATION];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OmpSessionBinding {
    schema: String,
    agent: String,
    runtime_id: String,
    runtime_incarnation: String,
    native_session_id: String,
    resume_generation: Option<crate::residency::Generation>,
    ready: bool,
}

impl OmpSessionBinding {
    pub fn native_session_id(&self) -> &str {
        &self.native_session_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OmpResidencyCheckpoint {
    schema: String,
    source_generation: crate::residency::Generation,
    resume_generation: crate::residency::Generation,
    binding: OmpSessionBinding,
}

/// The omp MINORS verified against the admission checks in `docs/vrs/06-omp-driver/spec.md`.
///
/// 18.0 was measured twice: at 18.0.3 on 2026-08-25 and again at 18.0.9 on 2026-08-28.
/// 18.1 was measured at 18.1.2 on 2026-09-02.
/// 18.3 was measured at 18.3.0 on 2026-09-24.
///
/// Admission is per minor, per decision 0007-omp-is-a-fifth-native-driver-with-its-own-channel-and-a-hard-version-gate ("hard version gate on the minor, 18.x initially")
/// and OMP-R05 ("a later minor stays rejected"). Any patch inside an admitted minor launches
/// without new evidence: omp releases near-daily, so gating patches blocked the fleet on changes
/// the capture already covered — 18.0.10 shipped within hours of 18.0.9 being admitted. A new
/// MINOR still costs the five OMP-R05 probes.
const SUPPORTED_OMP_MINORS: [(u32, u32); 3] = [(18, 0), (18, 1), (18, 3)];

/// The omp builds the harness-context producer's arithmetic was measured against (HC-R13, HC-T03).
///
/// This is a different question from the launch gate above and the two must not be collapsed. The
/// gate admits a MINOR SERIES, because a patch inside an admitted minor costs no new evidence; the
/// fixture pins the EXACT BUILDS a number's meaning was measured on, because omp's `tokens` being
/// prompt-only input is a property of a specific build and not of any documented contract. Both
/// builds were probed on 2026-08-29 and agree, which is what makes the minor gate defensible here.
/// `the_measured_context_builds_are_admitted_by_this_gate` keeps them from drifting apart.
pub const MEASURED_CONTEXT_VERSIONS: [&str; 2] = ["18.0.9", "18.0.3"];

/// omp's half of the pi-family launch fork. The version gate rides on the descriptor so the shared
/// body runs it where omp has always run it: after the empty-argv check and before the ownership
/// claim, so an unadmitted minor fails without claiming the seat.
pub(crate) const OMP_KIND: HarnessKind = HarnessKind {
    label: "omp",
    extension: EXTENSION,
    bin_env: CHANNEL_BIN,
    catalog_env: CHANNEL_CATALOG,
    identity_env: CHANNEL_IDENTITY,
    runtime_id_env: CHANNEL_RUNTIME_ID,
    session_env: CHANNEL_SESSION,
    seq_env: CHANNEL_SEQ,
    verify_version: Some(verify_supported_version),
};

pub fn state_dir(catalog_root: &Path, identity: &str) -> PathBuf {
    let mut hash = Sha256::new();
    for value in [
        catalog_root.as_os_str().as_encoded_bytes(),
        identity.as_bytes(),
    ] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }
    let digest = format!("{:x}", hash.finalize());
    crate::run::state_root()
        .join("st2")
        .join("omp")
        .join(&digest[..24])
}

pub fn record_channel_binding(
    state_dir: &Path,
    agent: &str,
    runtime_id: &str,
    runtime_incarnation: &str,
    native_session_id: &str,
    resume_generation: Option<crate::residency::Generation>,
    expected_native_session: Option<&str>,
) -> Result<OmpSessionBinding> {
    anyhow::ensure!(
        !runtime_incarnation.is_empty(),
        "OMP channel has no runtime incarnation"
    );
    anyhow::ensure!(
        !native_session_id.is_empty(),
        "OMP channel has no native session id"
    );
    anyhow::ensure!(
        resume_generation.is_some() == expected_native_session.is_some(),
        "OMP channel has an incomplete mandatory resume fence"
    );
    if let Some(expected) = expected_native_session {
        anyhow::ensure!(
            native_session_id == expected,
            "OMP native session {native_session_id:?} does not match required resume {expected:?}"
        );
    }
    let binding = OmpSessionBinding {
        schema: BINDING_SCHEMA.into(),
        agent: agent.into(),
        runtime_id: runtime_id.into(),
        runtime_incarnation: runtime_incarnation.into(),
        native_session_id: native_session_id.into(),
        resume_generation,
        ready: false,
    };
    crate::residency::atomic_json(&state_dir.join(PENDING_BINDING_FILE), &binding)?;
    Ok(binding)
}

pub fn confirm_channel_binding(
    state_dir: &Path,
    agent_dir: &Path,
    agent: &str,
    runtime_id: &str,
    runtime_incarnation: &str,
    ownership_seq: u64,
    native_session_id: &str,
    resume_generation: Option<crate::residency::Generation>,
) -> Result<OmpSessionBinding> {
    let mut binding = load_pending_binding(state_dir, agent, runtime_id)?
        .context("OMP channel has no pending native session binding")?;
    anyhow::ensure!(
        binding.runtime_incarnation == runtime_incarnation
            && binding.native_session_id == native_session_id
            && binding.resume_generation == resume_generation,
        "OMP channel readiness belongs to a different native session binding"
    );
    binding.ready = true;
    crate::harness_state::with_current_ownership(
        agent_dir,
        runtime_incarnation,
        ownership_seq,
        || {
            crate::residency::atomic_json(&state_dir.join(BINDING_FILE), &binding)?;
            Ok(())
        },
    )?;
    // The candidate is non-authoritative after promotion. Leaving it behind is safer than
    // invalidating a completed handshake because cleanup failed.
    let _ = fs::remove_file(state_dir.join(PENDING_BINDING_FILE));
    Ok(binding)
}

pub fn checkpoint_residency(
    state_dir: &Path,
    agent: &str,
    runtime_id: &str,
    source_generation: crate::residency::Generation,
    resume_generation: crate::residency::Generation,
) -> Result<OmpResidencyCheckpoint> {
    anyhow::ensure!(
        source_generation.0.checked_add(1) == Some(resume_generation.0),
        "OMP residency checkpoint generation is not monotonic"
    );
    let binding = load_binding(state_dir, agent, runtime_id)?
        .with_context(|| format!("OMP runtime {runtime_id:?} has no native session binding"))?;
    anyhow::ensure!(
        binding.ready,
        "OMP runtime native session binding is not ready"
    );
    let checkpoint = OmpResidencyCheckpoint {
        schema: CHECKPOINT_SCHEMA.into(),
        source_generation,
        resume_generation,
        binding,
    };
    crate::residency::atomic_json(&state_dir.join(CHECKPOINT_FILE), &checkpoint)?;
    Ok(checkpoint)
}

pub fn required_residency_resume(
    state_dir: &Path,
    agent: &str,
    runtime_id: &str,
    resume_generation: crate::residency::Generation,
    authored_args: &[String],
) -> Result<String> {
    ensure_no_authored_session_selection(authored_args)?;
    let checkpoint = load_checkpoint(state_dir, agent, runtime_id, resume_generation)?;
    let current = load_binding(state_dir, agent, runtime_id)?
        .with_context(|| format!("OMP runtime {runtime_id:?} has no native session binding"))?;
    anyhow::ensure!(
        current == checkpoint.binding,
        "OMP native session binding changed after residency checkpoint"
    );
    Ok(current.native_session_id)
}

pub fn residency_ready(
    state_dir: &Path,
    agent: &str,
    runtime_id: &str,
    resume_generation: crate::residency::Generation,
    expected_runtime_incarnation: &str,
) -> Result<bool> {
    let checkpoint = load_checkpoint(state_dir, agent, runtime_id, resume_generation)?;
    let Some(current) = load_binding(state_dir, agent, runtime_id)? else {
        return Ok(false);
    };
    if current == checkpoint.binding {
        return Ok(false);
    }
    if !current.ready {
        return Ok(false);
    }
    anyhow::ensure!(
        current.resume_generation == Some(resume_generation),
        "OMP native session binding does not prove the required residency generation"
    );
    anyhow::ensure!(
        current.native_session_id == checkpoint.binding.native_session_id,
        "OMP resumed a different native session than the residency checkpoint"
    );
    Ok(current.runtime_incarnation == expected_runtime_incarnation)
}

fn load_binding(
    state_dir: &Path,
    agent: &str,
    runtime_id: &str,
) -> Result<Option<OmpSessionBinding>> {
    load_binding_file(&state_dir.join(BINDING_FILE), agent, runtime_id)
}

fn load_pending_binding(
    state_dir: &Path,
    agent: &str,
    runtime_id: &str,
) -> Result<Option<OmpSessionBinding>> {
    load_binding_file(
        &state_dir.join(PENDING_BINDING_FILE),
        agent,
        runtime_id,
    )
}

fn load_binding_file(
    path: &Path,
    agent: &str,
    runtime_id: &str,
) -> Result<Option<OmpSessionBinding>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let binding: OmpSessionBinding = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        binding.schema == BINDING_SCHEMA,
        "unsupported OMP native session binding schema"
    );
    anyhow::ensure!(
        binding.agent == agent && binding.runtime_id == runtime_id,
        "OMP native session binding belongs to a different agent runtime"
    );
    anyhow::ensure!(
        !binding.runtime_incarnation.is_empty() && !binding.native_session_id.is_empty(),
        "OMP native session binding is incomplete"
    );
    Ok(Some(binding))
}

fn load_checkpoint(
    state_dir: &Path,
    agent: &str,
    runtime_id: &str,
    resume_generation: crate::residency::Generation,
) -> Result<OmpResidencyCheckpoint> {
    let path = state_dir.join(CHECKPOINT_FILE);
    let bytes = fs::read(&path)
        .with_context(|| format!("reading OMP residency checkpoint {}", path.display()))?;
    let checkpoint: OmpResidencyCheckpoint = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        checkpoint.schema == CHECKPOINT_SCHEMA,
        "unsupported OMP residency checkpoint schema"
    );
    anyhow::ensure!(
        checkpoint.resume_generation == resume_generation
            && checkpoint.source_generation.0.checked_add(1) == Some(resume_generation.0),
        "OMP residency checkpoint belongs to a different generation"
    );
    anyhow::ensure!(
        checkpoint.binding.agent == agent && checkpoint.binding.runtime_id == runtime_id,
        "OMP residency checkpoint belongs to a different agent runtime"
    );
    anyhow::ensure!(
        checkpoint.binding.schema == BINDING_SCHEMA
            && checkpoint.binding.ready
            && !checkpoint.binding.runtime_incarnation.is_empty(),
        "OMP residency checkpoint has an invalid native session binding"
    );
    anyhow::ensure!(
        !checkpoint.binding.native_session_id.is_empty(),
        "OMP residency checkpoint has an empty native session id"
    );
    Ok(checkpoint)
}

fn ensure_no_authored_session_selection(authored_args: &[String]) -> Result<()> {
    anyhow::ensure!(
        !authored_args.iter().any(|argument| {
            matches!(
                argument.as_str(),
                "-c" | "--continue"
                    | "-r"
                    | "--resume"
                    | "--from-claude"
                    | "--from-codex"
                    | "--no-session"
            ) || argument.starts_with("--resume=")
                || argument.starts_with("-r=")
        }),
        "authored OMP session selection conflicts with mandatory residency resume"
    );
    Ok(())
}

fn with_required_resume(mut argv: Vec<String>, native_session_id: &str) -> Result<Vec<String>> {
    anyhow::ensure!(!argv.is_empty(), "OMP provider argv is empty");
    ensure_no_authored_session_selection(&argv[1..])?;
    argv.splice(
        1..1,
        ["--resume".to_string(), native_session_id.to_string()],
    );
    Ok(argv)
}

/// Run one interactive omp provider and maintain its presence until it exits.
pub fn run(
    catalog_root: &Path,
    identity: String,
    runtime_id: String,
    omp_argv: Vec<String>,
) -> Result<()> {
    pi_family_session::run_for_with_environment(
        catalog_root,
        identity,
        runtime_id,
        omp_argv,
        &OMP_KIND,
        &[],
        &RESUME_FENCE_ENV,
        None,
    )
}

/// Run one host-owned cold-residency attempt under its exact incarnation.
pub fn run_residency_attempt(
    catalog_root: &Path,
    identity: String,
    runtime_id: String,
    omp_argv: Vec<String>,
    resume_generation: crate::residency::Generation,
    required_incarnation: String,
) -> Result<()> {

    anyhow::ensure!(
        !required_incarnation.is_empty(),
        "OMP required runtime incarnation is empty"
    );
    run_with_required_resume(
        catalog_root,
        identity,
        runtime_id,
        omp_argv,
        resume_generation,
        required_incarnation,
    )
}

fn run_with_required_resume(
    catalog_root: &Path,
    identity: String,
    runtime_id: String,
    omp_argv: Vec<String>,
    resume_generation: crate::residency::Generation,
    required_incarnation: String,
) -> Result<()> {
    anyhow::ensure!(
        !omp_argv.is_empty(),
        "omp driver '{runtime_id}' has no provider argv"
    );
    let native_session = required_residency_resume(
        &state_dir(catalog_root, &identity),
        &identity,
        &runtime_id,
        resume_generation,
        &omp_argv[1..],
    )?;
    let omp_argv = with_required_resume(omp_argv, &native_session)?;
    let residency_env = [
        (
            CHANNEL_RESUME_GENERATION.to_string(),
            resume_generation.0.to_string(),
        ),
        (
            CHANNEL_EXPECTED_NATIVE_SESSION.to_string(),
            native_session,
        ),
    ];
    pi_family_session::run_for_with_environment(
        catalog_root,
        identity,
        runtime_id,
        omp_argv,
        &OMP_KIND,
        &residency_env,
        &RESUME_FENCE_ENV,
        Some(required_incarnation),
    )
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

    fn claim_omp(agent_dir: &Path, incarnation: &str) -> u64 {
        crate::harness_state::claim(agent_dir, "h.worker", "omp", incarnation).unwrap()
    }

    /// Pins the admitted set itself — the intent carried over from the exact-version gate.
    /// Iterating the constant cannot catch a minor that was added without measuring it, so the
    /// set is asserted literally: widening has to be a deliberate edit here, next to the
    /// `.experiments/` capture that justifies it.
    #[test]
    fn admitted_minors_are_exactly_the_measured_set() {
        assert_eq!(SUPPORTED_OMP_MINORS, [(18, 0), (18, 1), (18, 3)]);
    }

    /// Every exact build that admitted a minor must still launch. Keeping the literals here makes
    /// each OMP-R05 capture a deliberate part of the gate rather than inferring evidence from the
    /// admitted series.
    #[test]
    fn version_gate_admits_every_admission_capture() {
        for version in ["18.0.3", "18.0.9", "18.1.2", "18.3.0"] {
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
        for version in [
            "18.0.9-rc1",
            "18.0.9+meta",
            "18.1.2-rc1",
            "18.1.2+meta",
            "18.3.0-rc1",
            "18.3.0+meta",
        ] {
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

    /// The gate must run before the wrapper claims the seat: an unadmitted minor that took
    /// ownership would leave the seat's record owned by a session that never launched.
    #[test]
    fn the_version_gate_is_wired_into_the_shared_launch_fork() {
        assert!(
            OMP_KIND.verify_version.is_some(),
            "omp must carry a launch-time version gate"
        );
        let fake = FakeExecutable::new("#!/bin/sh\nprintf '18.2.0\\n'\n");
        let gate = OMP_KIND.verify_version.unwrap();
        assert!(
            gate(fake.path().to_str().unwrap()).is_err(),
            "the descriptor's gate must be the refusing one"
        );
    }

    #[test]
    fn ordinary_launch_clears_inherited_resume_fences() {
        const CHILD_ROOT: &str = "ST2_TEST_OMP_ORDINARY_CHILD_ROOT";
        const CHILD_PROVIDER: &str = "ST2_TEST_OMP_ORDINARY_CHILD_PROVIDER";

        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let provider = std::env::var(CHILD_PROVIDER).unwrap();
            run(
                &PathBuf::from(root).join("catalog"),
                "worker".into(),
                "worker".into(),
                vec![provider, "boot".into()],
            )
            .unwrap();
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let catalog = temp.path().join("catalog");
        let agent_dir = catalog.join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let host = crate::run::detect_host();
        std::fs::write(
            agent_dir.join("agent.kdl"),
            format!(r#"agent "worker" {{ host "{host}"; command "true" }}"#),
        )
        .unwrap();
        let marker = temp.path().join("provider-env");
        let fake = FakeExecutable::new(&format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then printf 'omp v18.1.7\\n'; exit 0; fi\n\
             printf '%s|%s\\n' \"${{ST2_OMP_CHANNEL_EXPECTED_NATIVE_SESSION-unset}}\" \
             \"${{ST2_OMP_CHANNEL_RESUME_GENERATION-unset}}\" > '{}'\n",
            marker.display()
        ));
        let hooks = temp.path().join("hooks");
        crate::hooks::install_at(&hooks, false).unwrap();

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "omp_session::tests::ordinary_launch_clears_inherited_resume_fences",
                "--nocapture",
            ])
            .env(CHILD_ROOT, temp.path())
            .env(CHILD_PROVIDER, fake.path())
            .env("ST_HOOKS", hooks)
            .env("XDG_STATE_HOME", temp.path().join("state"))
            .env(CHANNEL_EXPECTED_NATIVE_SESSION, "ambient-session")
            .env(CHANNEL_RESUME_GENERATION, "99")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "nested ordinary launch failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "unset|unset\n");
    }

    #[test]
    fn mandatory_residency_validation_precedes_any_provider_child() {
        let temp = tempfile::tempdir().unwrap();
        let empty_incarnation = run_residency_attempt(
            temp.path(),
            "residency-no-spawn.worker".into(),
            "residency-no-spawn.worker".into(),
            vec!["omp".into()],
            crate::residency::Generation(2),
            String::new(),
        )
        .unwrap_err();
        assert!(
            empty_incarnation
                .to_string()
                .contains("required runtime incarnation is empty")
        );
        let marker = temp.path().join("provider-started");
        let fake = FakeExecutable::new(&format!(
            "#!/bin/sh\ntouch '{}'\nprintf 'omp v18.1.7\\n'\n",
            marker.display()
        ));

        let error = run_residency_attempt(
            temp.path(),
            "residency-no-spawn.worker".into(),
            "residency-no-spawn.worker".into(),
            vec![fake.path().display().to_string(), "boot".into()],
            crate::residency::Generation(2),
            "attempt-test".into(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("residency checkpoint"));
        assert!(
            !marker.exists(),
            "the OMP provider started before mandatory resume validation"
        );
    }

    #[test]
    fn residency_resume_binds_the_exact_native_session_generation_and_incarnation() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let agent_dir = temp.path().join("agent");
        let prior_seq = claim_omp(&agent_dir, "runtime-prior");
        record_channel_binding(
            &state,
            "h.worker",
            "h.worker",
            "runtime-prior",
            "session-exact",
            None,
            None,
        )
        .unwrap();
        confirm_channel_binding(
            &state,
            &agent_dir,
            "h.worker",
            "h.worker",
            "runtime-prior",
            prior_seq,
            "session-exact",
            None,
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

        let native = required_residency_resume(
            &state,
            "h.worker",
            "h.worker",
            crate::residency::Generation(2),
            &["--model".into(), "test".into(), "boot".into()],
        )
        .unwrap();
        assert_eq!(native, "session-exact");
        assert_eq!(
            with_required_resume(
                vec!["omp".into(), "--model".into(), "test".into(), "boot".into()],
                &native,
            )
            .unwrap(),
            [
                "omp",
                "--resume",
                "session-exact",
                "--model",
                "test",
                "boot",
            ]
        );
        assert!(
            !residency_ready(
                &state,
                "h.worker",
                "h.worker",
                crate::residency::Generation(2),
                "runtime-next",
            )
            .unwrap()
        );

        let next_seq = claim_omp(&agent_dir, "runtime-next");
        record_channel_binding(
            &state,
            "h.worker",
            "h.worker",
            "runtime-stale",
            "session-exact",
            Some(crate::residency::Generation(2)),
            Some("session-exact"),
        )
        .unwrap();
        assert!(
            !residency_ready(
                &state,
                "h.worker",
                "h.worker",
                crate::residency::Generation(2),
                "runtime-next",
            )
            .unwrap(),
            "binding publication alone is not channel readiness"
        );
        assert_eq!(
            required_residency_resume(
                &state,
                "h.worker",
                "h.worker",
                crate::residency::Generation(2),
                &[],
            )
            .unwrap(),
            "session-exact",
            "an unconfirmed channel attempt must leave the checkpoint retryable"
        );
        let stale_error = confirm_channel_binding(
            &state,
            &agent_dir,
            "h.worker",
            "h.worker",
            "runtime-stale",
            next_seq,
            "session-exact",
            Some(crate::residency::Generation(2)),
        )
        .unwrap_err();
        assert!(stale_error.to_string().contains("ownership was superseded"));
        assert!(
            !residency_ready(
                &state,
                "h.worker",
                "h.worker",
                crate::residency::Generation(2),
                "runtime-next",
            )
            .unwrap(),
            "a stale prior-attempt binding became ready"
        );

        record_channel_binding(
            &state,
            "h.worker",
            "h.worker",
            "runtime-next",
            "session-exact",
            Some(crate::residency::Generation(2)),
            Some("session-exact"),
        )
        .unwrap();
        confirm_channel_binding(
            &state,
            &agent_dir,
            "h.worker",
            "h.worker",
            "runtime-next",
            next_seq,
            "session-exact",
            Some(crate::residency::Generation(2)),
        )
        .unwrap();
        assert!(
            residency_ready(
                &state,
                "h.worker",
                "h.worker",
                crate::residency::Generation(2),
                "runtime-next",
            )
            .unwrap()
        );
    }

    #[test]
    fn superseded_wrapper_cannot_promote_over_the_current_owners_binding() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let agent_dir = temp.path().join("agent");
        let predecessor_seq = claim_omp(&agent_dir, "runtime-predecessor");
        let successor_seq = claim_omp(&agent_dir, "runtime-successor");

        record_channel_binding(
            &state,
            "h.worker",
            "h.worker",
            "runtime-successor",
            "session-successor",
            None,
            None,
        )
        .unwrap();
        let successor = confirm_channel_binding(
            &state,
            &agent_dir,
            "h.worker",
            "h.worker",
            "runtime-successor",
            successor_seq,
            "session-successor",
            None,
        )
        .unwrap();

        record_channel_binding(
            &state,
            "h.worker",
            "h.worker",
            "runtime-predecessor",
            "session-predecessor",
            None,
            None,
        )
        .unwrap();
        let error = confirm_channel_binding(
            &state,
            &agent_dir,
            "h.worker",
            "h.worker",
            "runtime-predecessor",
            predecessor_seq,
            "session-predecessor",
            None,
        )
        .unwrap_err();

        assert!(error.to_string().contains("ownership was superseded"));
        assert_eq!(
            load_binding(&state, "h.worker", "h.worker").unwrap(),
            Some(successor)
        );
    }

    #[test]
    fn mandatory_omp_resume_refuses_corrupt_foreign_and_stale_checkpoints() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let agent_dir = temp.path().join("agent");
        let prior_seq = claim_omp(&agent_dir, "runtime-prior");
        record_channel_binding(
            &state,
            "h.worker",
            "h.worker",
            "runtime-prior",
            "session-exact",
            None,
            None,
        )
        .unwrap();
        confirm_channel_binding(
            &state,
            &agent_dir,
            "h.worker",
            "h.worker",
            "runtime-prior",
            prior_seq,
            "session-exact",
            None,
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
        let path = state.join(CHECKPOINT_FILE);
        let checkpoint = std::fs::read(&path).unwrap();

        std::fs::write(&path, b"{").unwrap();
        assert!(
            required_residency_resume(
                &state,
                "h.worker",
                "h.worker",
                crate::residency::Generation(2),
                &[],
            )
            .is_err()
        );

        let mut foreign: serde_json::Value = serde_json::from_slice(&checkpoint).unwrap();
        foreign["binding"]["agent"] = serde_json::json!("h.other");
        std::fs::write(&path, serde_json::to_vec(&foreign).unwrap()).unwrap();
        let error = required_residency_resume(
            &state,
            "h.worker",
            "h.worker",
            crate::residency::Generation(2),
            &[],
        )
        .unwrap_err();
        assert!(error.to_string().contains("different agent runtime"));

        let mut stale: serde_json::Value = serde_json::from_slice(&checkpoint).unwrap();
        stale["resumeGeneration"] = serde_json::json!(3);
        std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
        let error = required_residency_resume(
            &state,
            "h.worker",
            "h.worker",
            crate::residency::Generation(2),
            &[],
        )
        .unwrap_err();
        assert!(error.to_string().contains("different generation"));
    }

    #[test]
    fn mandatory_omp_resume_refuses_fallback_selection_and_binding_mismatch() {
        for authored in [
            vec!["--continue".into()],
            vec!["-c".into()],
            vec!["--resume".into(), "other".into()],
            vec!["--resume=other".into()],
            vec!["-r".into(), "other".into()],
            vec!["--from-claude".into()],
            vec!["--from-codex".into()],
            vec!["--no-session".into()],
        ] {
            assert!(
                with_required_resume(
                    std::iter::once("omp".into()).chain(authored).collect(),
                    "session-exact",
                )
                .is_err()
            );
        }

        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let error = record_channel_binding(
            &state,
            "h.worker",
            "h.worker",
            "runtime-next",
            "session-other",
            Some(crate::residency::Generation(2)),
            Some("session-exact"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("does not match required resume"));
        assert!(!state.join("binding.json").exists());
    }
}
