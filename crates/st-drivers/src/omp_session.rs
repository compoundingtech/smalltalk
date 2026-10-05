//! Controlled omp launch with a session-owned presence lease.
//!
//! omp is pi-family: its integration point is a pi-style extension loaded into the interactive
//! process, which reaches st2 by spawning `st2 driver omp-channel` (`hooks/omp-channel.ts`,
//! forked from the pi channel — see `docs/vrs/06-omp-driver/spec.md` for the measured
//! divergences). The launch body itself is shared with pi in [`crate::pi_family_session`].
//!
//! Unlike pi, the wrapper hard-gates the provider version (OMP-R05): the delivery-critical
//! surface — event names, the sampled idle edge, the approval events — is versioned behavior, not
//! an API contract: an installed build stays refused until its isolated admission checks pass.
//! Every unseen exact producer/extension identity needs its own measured evidence.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::pi_family_session::{self, HarnessKind};

/// The extension file inside this binary's immutable hook set.
const EXTENSION: &str = "omp-channel.ts";

/// The exact st2 executable the omp extension must spawn for its channel.
pub const CHANNEL_BIN: &str = "ST_OMP_CHANNEL_BIN";
/// The catalog root that executable must be pointed at.
pub const CHANNEL_CATALOG: &str = "ST_OMP_CHANNEL_CATALOG";
/// The host-qualified bus identity the channel binds.
pub const CHANNEL_IDENTITY: &str = "ST_OMP_CHANNEL_IDENTITY";
/// The wrapper's runtime/task ID — the pty session whose liveness vouches for observed state.
pub const CHANNEL_RUNTIME_ID: &str = "ST_OMP_CHANNEL_RUNTIME_ID";
/// The session incarnation token the wrapper mints. The channel adopts it so the wrapper's
/// terminal record owns — and thereby fences — the live records the channel writes.
pub const CHANNEL_SESSION: &str = "ST_OMP_CHANNEL_SESSION";
/// The ownership sequence the wrapper claimed at startup.
pub const CHANNEL_SEQ: &str = "ST_OMP_CHANNEL_SEQ";
/// The exact native session that a cold residency launch must resume.
pub const CHANNEL_EXPECTED_NATIVE_SESSION: &str = "ST_OMP_CHANNEL_EXPECTED_NATIVE_SESSION";
/// The cold residency generation whose exact native session the channel must prove.
pub const CHANNEL_RESUME_GENERATION: &str = "ST_OMP_CHANNEL_RESUME_GENERATION";

const BINDING_SCHEMA: &str = "st.omp-session-binding.v1";
const CHECKPOINT_SCHEMA: &str = "st.omp-residency-checkpoint.v1";
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

/// The omp builds the harness-context producer's arithmetic was measured against (HC-R13, HC-T03).
///
/// Context arithmetic remains evidence for these exact builds, separate from admission of the
/// delivery contract. A passing launch probe cannot certify token semantics or pricing.
pub const MEASURED_CONTEXT_VERSIONS: [&str; 2] = ["18.0.9", "18.0.3"];

/// omp's half of the pi-family launch fork. The version gate rides on the descriptor so the shared
/// body runs it where omp has always run it: after the empty-argv check and before the ownership
/// claim, so a failed admission leaves live ownership unchanged.
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
    crate::run::harness_state_root()
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
    let schema = load_binding(state_dir, agent, runtime_id)?
        .filter(|binding| binding.runtime_incarnation == runtime_incarnation)
        .map_or_else(|| BINDING_SCHEMA.to_owned(), |binding| binding.schema);
    let binding = OmpSessionBinding {
        schema,
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
        schema: crate::contracts::schema_for_owner(&binding.schema, CHECKPOINT_SCHEMA),
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

/// Current, ready native session for an owner read. A pending switch is not a read authority.
pub fn bound_native_session(
    state_dir: &Path,
    agent: &str,
    runtime_id: &str,
    runtime_incarnation: &str,
) -> Result<Option<String>> {
    let Some(binding) = load_binding(state_dir, agent, runtime_id)? else { return Ok(None) };
    if !binding.ready || binding.runtime_incarnation != runtime_incarnation {
        return Ok(None);
    }
    if let Some(pending) = load_pending_binding(state_dir, agent, runtime_id)?
        && pending.runtime_incarnation == runtime_incarnation
        && pending.native_session_id != binding.native_session_id {
        return Ok(None);
    }
    Ok(Some(binding.native_session_id))
}

fn load_pending_binding(
    state_dir: &Path,
    agent: &str,
    runtime_id: &str,
) -> Result<Option<OmpSessionBinding>> {
    load_binding_file(&state_dir.join(PENDING_BINDING_FILE), agent, runtime_id)
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
        crate::contracts::schema_matches(&binding.schema, BINDING_SCHEMA),
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
        crate::contracts::schema_matches(&checkpoint.schema, CHECKPOINT_SCHEMA),
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
        crate::contracts::schema_matches(&checkpoint.binding.schema, BINDING_SCHEMA)
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
        true,
    )
}

/// Resume supervising an omp provider a predecessor driver image launched and released.
pub fn adopt(
    catalog_root: &Path,
    identity: String,
    runtime_id: String,
    pid: u32,
    session: String,
    seq: u64,
) -> Result<()> {
    pi_family_session::adopt_for(
        catalog_root,
        identity,
        runtime_id,
        &OMP_KIND,
        pid,
        session,
        seq,
        true,
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
        (CHANNEL_EXPECTED_NATIVE_SESSION.to_string(), native_session),
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
        true,
    )
}

/// Measure the installed producer and exact shipped extension before claiming the live seat.
fn verify_supported_version(binary: &str, agent_dir: &Path, extension: &Path) -> Result<()> {
    use crate::driver_diagnostic::{Driver, Publisher};
    let admission = crate::harness_admission::admit(binary, Driver::Omp, Some(extension));
    let mut publisher = Publisher::new(
        agent_dir,
        Driver::Omp,
        admission.version.clone(),
        admission.support(),
    );
    admission.publish(&mut publisher);
    anyhow::ensure!(admission.passed(), "{}", admission.explanation(Driver::Omp));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    struct FakeExecutable {
        _directory: tempfile::TempDir,
        path: PathBuf,
    }

    impl FakeExecutable {
        fn new(body: &str) -> Self {
            let directory = tempfile::Builder::new()
                .prefix("st-omp-version-")
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
        let fake = crate::harness_admission::tests::fake_omp(temp.path());
        let hooks = temp.path().join("hooks");
        crate::hooks::install_at(&hooks, false).unwrap();

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "omp_session::tests::ordinary_launch_clears_inherited_resume_fences",
                "--nocapture",
            ])
            .env(CHILD_ROOT, temp.path())
            .env(CHILD_PROVIDER, &fake)
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
    fn failed_admission_diagnoses_without_claiming_or_launching_the_live_seat() {
        const CHILD: &str = "ST_TEST_ADMISSION_REFUSAL_ROOT";
        if let Some(root) = std::env::var_os(CHILD) {
            let root = PathBuf::from(root);
            let agent_dir = root.join("catalog/agent");
            let state = crate::harness_state::harness_state_path(&agent_dir);
            claim_omp(&agent_dir, "predecessor");
            let before = std::fs::read(&state).unwrap();
            let error = run(&root.join("catalog"), "worker".into(), "worker".into(),
                vec![root.join("omp").display().to_string()]).unwrap_err();
            assert!(error.to_string().contains("admissionNativeConsumption"), "{error:#}");
            assert_eq!(std::fs::read(&state).unwrap(), before);
            assert!(!root.join("provider-env").exists());
            let observed = crate::driver_diagnostic::read(&crate::driver_diagnostic::path(&agent_dir));
            assert!(matches!(observed, crate::driver_diagnostic::Observed::Failure(failure)
                if failure.reason == crate::driver_diagnostic::Reason::AdmissionNativeConsumption));
            assert!(error.to_string().contains("st admission override omp"));
            // A person's exact-build exception permits the real launch path without replacing
            // the refused measurement. This exercises the shipped asset and selected state root.
            let hooks = crate::hooks::verify_installed().unwrap();
            let cache = crate::run::harness_state_root().join("harness-admission");
            crate::harness_admission::override_build(
                root.join("omp").to_str().unwrap(),
                crate::driver_diagnostic::Driver::Omp,
                Some(&hooks.join("omp-channel.ts")),
                &cache,
                "isolated regression operator exception",
            ).unwrap();
            run(&root.join("catalog"), "worker".into(), "worker".into(),
                vec![root.join("omp").display().to_string()]).unwrap();
            assert!(root.join("provider-env").exists(), "overridden omp seat launches");
            assert_ne!(std::fs::read(&state).unwrap(), before);
            return;
        }
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("catalog/agent")).unwrap();
        let host = crate::run::detect_host();
        std::fs::write(root.path().join("catalog/agent/agent.kdl"),
            format!(r#"agent "worker" {{ host "{host}"; command "true" }}"#)).unwrap();
        crate::harness_admission::tests::fake_omp(root.path());
        let mut fixture = crate::harness_admission::tests::measured_omp();
        crate::harness_admission::tests::fail_omp(&mut fixture, crate::harness_admission::Check::NativeConsumption);
        std::fs::write(root.path().join("capture.json"), serde_json::to_vec(&fixture).unwrap()).unwrap();
        let hooks = root.path().join("hooks");
        crate::hooks::install_at(&hooks, false).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "omp_session::tests::failed_admission_diagnoses_without_claiming_or_launching_the_live_seat", "--nocapture"])
            .env(CHILD, root.path()).env("ST_HOOKS", hooks)
            .env("XDG_STATE_HOME", root.path().join("state")).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
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

/// Launch a native graph-owned session without legacy presence transport.
pub fn run_native(
    paths: &crate::driver_paths::Paths,
    identity: String,
    runtime_id: String,
    argv: Vec<String>,
    // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
    extra_environment: &[(String, String)],
    // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
) -> Result<()> {
    let environment = paths.environment(&identity);
    // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
    let environment = {
        let mut environment = environment;
        environment.extend_from_slice(extra_environment);
        environment
    };
    // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
    pi_family_session::run_for_paths(
        &paths.root,
        &paths.agent_dir,
        identity,
        runtime_id,
        argv,
        &OMP_KIND,
        &environment,
        &RESUME_FENCE_ENV,
        None,
        false,
    )
}

/// Adopt a native graph-owned session without legacy presence transport.
#[allow(clippy::too_many_arguments)]
pub fn adopt_native(
    paths: &crate::driver_paths::Paths,
    identity: String,
    runtime_id: String,
    pid: u32,
    session: String,
    seq: u64,
) -> Result<()> {
    pi_family_session::adopt_paths(
        &paths.agent_dir,
        identity,
        runtime_id,
        &OMP_KIND,
        pid,
        session,
        seq,
        false,
    )
}
