//! The daemon and its children share the account's interactive login-shell environment.
//! Never mutate the process environment: the daemon has concurrent readers and children.

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

type Environment = BTreeMap<String, String>;

// LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-16-env-fractal-dual-read — DELETE at contraction — https://github.com/compoundingtech/smalltalk/issues/3032
/// Resolve one source layer before overlaying it: empty/conflicted values mask lower layers.
/// ST3 suffixes are explicit because Fractal folds ST_ and ST3_ into one namespace.
fn old_alias(name: &str) -> Option<String> {
    let suffix = name.strip_prefix("FRACTAL_")?;
    // The collision-free ST3 inventory recorded in #3233, including test/tool knobs.
    let prefix = if matches!(suffix,
        "ACCOUNT" | "AGENT_CORPUS" | "ATTACHMENT_TAG" | "ATTENTION_LIST" | "BIN"
        | "BINARY" | "BLOB_RETENTION_HOURS" | "BODY_MAX_CHARS" | "BOOTSTRAP_TEST_BINARY" | "BRIDGE_NATIVE_TEST_BIN"
        | "BUILD_REVISION" | "CHANNEL" | "CHANNEL_RESUME" | "CHECKPOINT_DAY" | "CHECKPOINT_ORIGIN"
        | "CHECKPOINT_SKIP_PROOF" | "CHECKPOINT_STORE" | "CHECKPOINT_SUBJECT" | "CLEANUP_FIXTURE" | "CLEANUP_FIXTURE_MODE"
        | "CLI_INVENTORY_CAPTURE_HELPER" | "CLI_INVENTORY_CAPTURE_MARKER" | "CLI_OTEL" | "COMPAT_BIN" | "COMPLETION_DEADLINE_MS"
        | "CONTEXT_VARIABLES" | "CONVERGENCE_RUNS" | "CONVERGENCE_SEED" | "CONVERGENCE_STEPS" | "COVERAGE_BINARY"
        | "COVERAGE_ROOT" | "DAEMON_WAIT" | "DIFFERENTIAL_FIRST" | "DIFFERENTIAL_SEEDS" | "DISABLED_DAEMON_METRICS_CHILD"
        | "DIVERGENCE_CHECK_INTERVAL_MS" | "DRIVER_RESUME" | "DRIVER_STATE_DIR" | "ENDPOINT" | "ENV_BEGIN"
        | "ENV_END" | "FIXTURE_BASH" | "FIXTURE_MODE" | "FIXTURE_PATH" | "FIXTURE_TERMINAL_COMPLETION"
        | "FLEET_PORT_RESERVATION_PROBE" | "GITHUB_API_URL" | "GIT_EVIDENCE_ISOLATED_TEST" | "GIT_EVIDENCE_REAL_GIT" | "IDLE_PREFLIGHT_START_UNIX"
        | "IDLE_REPORT_DURATION_SECONDS" | "IDLE_REPORT_LOG" | "IDLE_REPORT_MAX_GAP_SECONDS" | "IDLE_REPORT_MIN_QUIET" | "IDLE_REPORT_MIN_QUIET_SECONDS"
        | "IDLE_REPORT_MIN_SAMPLES" | "IDLE_REPORT_NOW_UNIX" | "IDLE_REPORT_REMOTE" | "INCARNATION" | "INCREMENTAL"
        | "KDL_VERSION" | "LAUNCH_DESIRED_TOKEN" | "MAILBOX_TRANSPORT" | "MALLOC_TRIM_INTERVAL_SECS" | "MALLOC_TRIM_TEST_CHILD"
        | "MALLOC_TRIM_THRESHOLD_BYTES" | "MARKETPLACE" | "MARKETPLACE_MANIFEST" | "MCP_CONFIG" | "MESSAGE_BODIES_ACTOR_MAX_MB"
        | "MESSAGE_BODIES_MAX_MB" | "MESSAGE_BODIES_RETENTION_DAYS" | "MESSAGE_KEY_HOUR" | "MESSAGE_ROOT" | "MESSAGING_COMPAT_BIN"
        | "MESSAGING_FAULTS_EVIDENCE" | "MESSAGING_ROOT" | "MESSAGING_SCRATCH" | "MFE_TEST_BINARY" | "NATIVE_CONTINUE_PATH"
        | "NATIVE_CONTINUE_SESSION" | "NATIVE_RESUME_PATH" | "NATIVE_RESUME_SESSION" | "NATIVE_SELECTOR_SCOPE" | "OMP_PENDING_ASK"
        | "OP_AUDIT_BODY_BYTES" | "OP_AUDIT_CHILD_AUDIT" | "OP_AUDIT_CHILD_STORE" | "OP_AUDIT_DIR" | "OP_AUDIT_HUGE_CLAIMS"
        | "OP_AUDIT_OPERATIONS" | "OP_AUDIT_RESULT" | "OP_AUDIT_TOMBSTONES" | "OTELITE_BIN" | "OTEL_REPLICATION_SCENARIO_ROOT"
        | "OTEL_REQUIRE" | "OTEL_STORAGE_STAGE_CHILD" | "PACKAGE" | "PAIRING_PROOF_VECTOR_OUT" | "PERF_INCREMENTAL"
        | "PERF_NODE" | "PERF_RUNTIMES" | "PERF_STORE" | "PERSON" | "PLUGIN"
        | "PLUGIN_MANIFEST" | "POLICY_FILE" | "PRE_PIN_BIN" | "PROFILE_CANCELLATION_TEST_CHILD" | "PROFILE_DIR"
        | "PROFILE_PTRACER" | "PROFILE_SLOW_MS" | "PROFILE_TRIM_PHASE_TEST_CHILD" | "PROVENANCE_COMPAT_BIN" | "PUBLISHED_LISTS"
        | "QUIET_OWNED_PASS_BUDGET_CHILD" | "RECORDER_GH_DEPTH" | "RECORDER_GIT_DEPTH" | "REEXEC_TEST_SCRIPT" | "REEXEC_TEST_STATE"
        | "RELAY_TRACE_ACTOR" | "RELAY_TRACE_CALLER" | "RELAY_TRACE_DIGEST" | "RELAY_TRACE_FORWARD_DIGEST" | "RELAY_TRACE_PATH"
        | "RELAY_TRACE_PEER" | "REPLICATION_PAGE_LIMIT" | "ROLLOUT_OPERATION" | "ROLLOUT_PREDECESSOR" | "RUN_DIR"
        | "SCENARIO_ENDPOINT" | "SCENARIO_OUTPUT" | "SCENARIO_SCRIPT" | "SCENARIO_STATE_DIR" | "SCHEMA_CRASH_MARKER"
        | "SCHEMA_CRASH_PATH" | "SCHEMA_CRASH_PHASE" | "SERVER_NAME" | "SET_MARKER" | "SHA256_TAG"
        | "SOAK_DEADLINE_SECONDS" | "SOAK_EXAMPLE_MAC_BIN" | "SOAK_HOST" | "SOAK_IDLE_STATE_DIR" | "SOAK_INTERVAL_SECONDS"
        | "SOAK_JQ_BIN" | "SOAK_REPORT_LOG" | "SOAK_REPORT_MAX_GAP_SECONDS" | "SOAK_REPORT_MIN_DURATION_SECONDS" | "SOAK_REPORT_MIN_EACH_DIRECTION"
        | "SOAK_REPORT_NOW_UNIX" | "SOAK_REPORT_SKIP_RUNTIME_CHECKS" | "SOAK_STATE_DIR" | "SOAK_TEST_REAL_FABRIC_BIN" | "SOAK_TEST_REAL_ST3_BIN"
        | "SOAK_TEST_RESPONSE_MODE" | "SOAK_TIMEOUT_BIN" | "START_COST_DIR" | "START_OPERATION" | "STORAGE_PHASE_SPANS"
        | "SUBAGENTS_EVAL_OLD_SETTINGS" | "SUBAGENT_LEASE_MS" | "SUBAGENT_SILENCE_MS" | "SUBJECT" | "TERMINALS_COMPAT_BIN"
        | "TERMINAL_CAPABILITY" | "TEST_ADMITTED_REPORTS" | "TEST_DISABLE_ANCESTRY" | "TEST_DROP_JOIN_ANSWER" | "TEST_KEEP"
        | "TEST_NATIVE_SEED_CONFLICT" | "TEST_NATIVE_SEED_ROOT" | "TEST_REMOVAL_WRITE_BARRIER" | "TEST_REPLICATION_EXPORT_DELAY_MS" | "TEST_STARTUP_REPLAY_BARRIER"
        | "TOKEN_BUDGET" | "TO_TAG" | "WORKER_INTERVAL_MS" | "WRITE_FIXTURES")
    { "ST3_" } else { "ST_" };
    Some(format!("{prefix}{suffix}"))
}

pub(crate) fn normalize_aliases(environment: &mut Environment) {
    let names: std::collections::BTreeSet<_> = environment.keys().filter_map(|name| {
        if name.starts_with("ST_") || name.starts_with("ST3_") {
            Some(name.clone())
        } else {
            old_alias(name)
        }
    }).collect();
    for old in names {
        let new = st_drivers::contracts::fractal_env_name(&old).unwrap();
        if environment.get(&old).zip(environment.get(&new)).is_some_and(|(old, new)| old == new) {
            continue;
        }
        let value = st_drivers::contracts::env_with(&old, &|name| environment.get(name).cloned())
            .unwrap_or_default();
        environment.insert(old, value.clone());
        environment.insert(new, value);
    }
}

/// A launcher-owned assignment overrides both spellings, independently of authored layers.
pub(crate) fn set(environment: &mut Environment, name: &str, value: String) {
    if let Some(new) = st_drivers::contracts::fractal_env_name(name) {
        environment.insert(new, value.clone());
    }
    environment.insert(name.to_owned(), value);
}

/// Merge independently resolved layers, so a higher alias replaces either lower spelling.
pub(crate) fn overlay(
    mut inherited: Environment, declared: &Environment, executable: &std::path::Path,
) -> Result<Environment> {
    normalize_aliases(&mut inherited);
    let mut declared = declared.clone();
    normalize_aliases(&mut declared);
    st_runtime::overlay_environment(inherited, &declared, executable)
}

pub(crate) fn remove_aliases(environment: &mut Environment, name: &str) {
    environment.remove(name);
    if let Some(new) = st_drivers::contracts::fractal_env_name(name) {
        environment.remove(&new);
    }
}
// LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-16-env-fractal-dual-read

#[derive(Default)]
struct Cache {
    captured: Option<(Instant, Result<Environment, String>)>,
}

impl Cache {
    fn get(&mut self, capture: impl FnOnce() -> Result<Environment>) -> Result<Environment> {
        if self
            .captured
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= REFRESH_INTERVAL)
        {
            // Refresh on use, including shell configuration changes and credentials exported
            // by shell startup files. Keep failures visible, rather than using stale secrets.
            self.captured = Some((
                Instant::now(),
                capture().map_err(|error| format!("{error:#}")),
            ));
        }
        self.captured
            .as_ref()
            .unwrap()
            .1
            .clone()
            .map_err(anyhow::Error::msg)
    }
}

/// How long each capture at daemon start may wait for the login shell's startup files. A deploy
/// restarts the daemon while the machine is busy building, and a shell that takes seconds when idle
/// can take over a minute then; giving up on the first slow capture leaves the machine without a
/// daemon. Only a timeout is retried: a missing shell or a failing startup file will not heal.
const STARTUP_ATTEMPTS: [Duration; 3] = [
    Duration::from_secs(10),
    Duration::from_secs(30),
    Duration::from_secs(60),
];
const STARTUP_BACKOFF: Duration = Duration::from_secs(2);

impl Cache {
    /// Capture with growing patience, and keep the result so the daemon's first uses of the cache
    /// do not capture again.
    fn start(
        &mut self,
        mut capture: impl FnMut(Duration) -> Result<Environment>,
        attempts: &[Duration],
        backoff: Duration,
    ) -> Result<Environment> {
        let mut result = Err(anyhow::anyhow!("no environment capture was attempted"));
        for (index, timeout) in attempts.iter().enumerate() {
            result = capture(*timeout);
            match &result {
                Err(error)
                    if st_runtime::is_shell_startup_timeout(error)
                        && index + 1 < attempts.len() =>
                {
                    eprintln!(
                        "st: the login shell is slow to start ({error:#}); retrying with {} seconds (attempt {} of {})",
                        attempts[index + 1].as_secs(),
                        index + 2,
                        attempts.len()
                    );
                    std::thread::sleep(backoff * (index as u32 + 1));
                }
                _ => break,
            }
        }
        self.captured = Some((
            Instant::now(),
            result
                .as_ref()
                .map(Clone::clone)
                .map_err(|error| format!("{error:#}")),
        ));
        result
    }
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

fn capture_within(timeout: Duration) -> Result<Environment> {
    #[cfg(feature = "test-support")]
    if let Some(shell) = crate::test_support::login_shell() {
        return st_runtime::login_environment_from(shell, timeout);
    }
    st_runtime::login_environment_within(timeout)
}

/// Forget the captured environment, so the next use reads the login shell again. Setup does this
/// when the person asks to look for an agent they have just installed.
pub fn refresh() {
    if let Ok(mut cache) = cache().lock() {
        cache.captured = None;
    }
}

/// The environment the daemon starts with, captured patiently (see [`STARTUP_ATTEMPTS`]).
pub fn snapshot_at_startup() -> Result<Environment> {
    cache()
        .lock()
        .map_err(|_| anyhow::anyhow!("daemon environment cache is poisoned"))?
        .start(capture_within, &STARTUP_ATTEMPTS, STARTUP_BACKOFF)
        .context(
            "capture the daemon login-shell environment; check the account's shell startup files",
        )
}

pub fn snapshot() -> Result<Environment> {
    cache()
        .lock()
        .map_err(|_| anyhow::anyhow!("daemon environment cache is poisoned"))?
        .get(|| capture_within(Duration::from_secs(10)))
        .context(
            "capture the daemon login-shell environment; check the account's shell startup files",
        )
}

pub const HARNESSES: &[&str] = &["claude", "codex", "opencode", "pi", "omp"];

/// The same login PATH that the daemon resolves before starting a seat.
pub fn available_harnesses() -> Result<Vec<String>> {
    let environment = snapshot()?;
    Ok(HARNESSES
        .iter()
        .filter(|name| st_runtime::resolve_executable(name, &environment).is_ok())
        .map(|name| (*name).into())
        .collect())
}

/// What building and gating this repository needs on the login PATH. The repository selects
/// mold for Linux links only, so other platforms do not need it.
fn build_tools() -> Vec<&'static str> {
    let mut tools = vec!["cargo", "rustc"];
    if cfg!(target_os = "linux") {
        tools.push("mold");
    }
    tools.extend(["sccache", "gh", "git", "nix"]);
    tools
}

const LINK_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct BuildTools {
    pub found: Vec<&'static str>,
    pub missing: Vec<&'static str>,
    pub link: LinkResult,
}

pub(crate) enum LinkResult {
    Linked,
    /// Building needs cargo and rustc, so no crate was built.
    NotAttempted,
    Failed(String),
}

/// Look for each build tool on the environment's PATH, then build and link a small crate the way
/// the repository does.
pub(crate) fn check_build_tools(environment: &Environment) -> BuildTools {
    let (found, missing): (Vec<_>, Vec<_>) = build_tools()
        .into_iter()
        .partition(|tool| st_runtime::resolve_executable(tool, environment).is_ok());
    let can_link = ["cargo", "rustc"].iter().all(|tool| found.contains(tool));
    let link = if can_link {
        match link_small_crate(environment, LINK_TIMEOUT) {
            Ok(()) => LinkResult::Linked,
            Err(error) => LinkResult::Failed(error),
        }
    } else {
        LinkResult::NotAttempted
    };
    BuildTools {
        found,
        missing,
        link,
    }
}

fn link_small_crate(environment: &Environment, timeout: Duration) -> Result<(), String> {
    use std::os::unix::process::CommandExt as _;
    let scratch =
        tempfile::tempdir().map_err(|error| format!("create a scratch crate: {error}"))?;
    let write = |path: &str, content: &str| {
        let path = scratch.path().join(path);
        std::fs::create_dir_all(path.parent().expect("scratch files have a parent"))
            .and_then(|()| std::fs::write(&path, content))
            .map_err(|error| format!("write {}: {error}", path.display()))
    };
    // The empty workspace keeps cargo from adopting a workspace above a temporary directory.
    write(
        "Cargo.toml",
        "[package]\nname = \"st-doctor-link\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )?;
    write("src/main.rs", "fn main() {}\n")?;
    write(
        ".cargo/config.toml",
        "[target.'cfg(target_os = \"linux\")']\nrustflags = [\"-C\", \"link-arg=-fuse-ld=mold\"]\n",
    )?;
    let errors = scratch.path().join("cargo.stderr");
    let mut command = command_in("cargo", environment).map_err(|error| format!("{error:#}"))?;
    let mut child = command
        .args(["build", "--offline", "--quiet"])
        .current_dir(scratch.path())
        .env("CARGO_TARGET_DIR", scratch.path().join("target"))
        .env("CARGO_INCREMENTAL", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&errors).map_err(|error| error.to_string())?)
        // Its own group lets a timeout stop cargo and the compiler and linker it started.
        .process_group(0)
        .spawn()
        .map_err(|error| format!("start cargo: {error}"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                // SAFETY: signalling the process group that this function created.
                unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
                let _ = child.wait();
                return Err(format!(
                    "cargo build took longer than {}s",
                    timeout.as_secs()
                ));
            }
            Err(error) => return Err(format!("wait for cargo: {error}")),
        }
    };
    if status.success() {
        return Ok(());
    }
    let stderr = std::fs::read_to_string(&errors).unwrap_or_default();
    let lines = stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let tail = lines[lines.len().saturating_sub(3)..].join(" | ");
    Err(format!(
        "cargo build {status}: {}",
        tail.chars().take(400).collect::<String>()
    ))
}

pub(crate) fn command(program: &str) -> Result<Command> {
    command_in(program, &snapshot()?)
}

pub(crate) fn command_in(program: &str, environment: &Environment) -> Result<Command> {
    let executable = st_runtime::resolve_executable(program, environment)?;
    let mut command = Command::new(executable);
    command.env_clear().envs(environment);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_assignments_and_removals_cover_both_spellings() {
        let mut vars = Environment::from([
            ("ST_AGENT".into(), "agent/seat".into()),
            ("ST3_SUBJECT".into(), "agent/seat".into()),
            ("ST_SUPERVISOR".into(), String::new()),
        ]);
        normalize_aliases(&mut vars);
        assert_eq!(vars["FRACTAL_AGENT"], vars["ST_AGENT"]);
        assert_eq!(vars["FRACTAL_SUBJECT"], vars["ST3_SUBJECT"]);
        assert_eq!(vars["FRACTAL_SUPERVISOR"], "");
        remove_aliases(&mut vars, "ST_AGENT");
        assert!(!vars.contains_key("ST_AGENT") && !vars.contains_key("FRACTAL_AGENT"));
    }

    #[test]
    fn declared_aliases_replace_inherited_families_and_keep_path_expansion() {
        let inherited = Environment::from([
            ("PATH".into(), "/shell".into()),
            ("ST_SUPERVISOR".into(), "agent/old".into()),
        ]);
        for (new, expected) in [("agent/new", "agent/new"), ("", "")] {
            let declared = Environment::from([
                ("FRACTAL_SUPERVISOR".into(), new.into()),
                ("PATH".into(), "/declared:${PATH}".into()),
            ]);
            let merged = overlay(inherited.clone(), &declared, std::path::Path::new("/bin/st3")).unwrap();
            assert_eq!(merged["ST_SUPERVISOR"], expected);
            assert_eq!(merged["FRACTAL_SUPERVISOR"], expected);
            assert_eq!(merged["PATH"], "/bin:/declared:/shell");
        }
        let conflicting = Environment::from([
            ("ST_SUPERVISOR".into(), "agent/left".into()),
            ("FRACTAL_SUPERVISOR".into(), "agent/right".into()),
        ]);
        let merged = overlay(inherited, &conflicting, std::path::Path::new("/bin/st3")).unwrap();
        assert_eq!(merged["ST_SUPERVISOR"], "");
        assert_eq!(merged["FRACTAL_SUPERVISOR"], "");
    }

    #[test]
    fn refreshes_success_and_failure_after_the_interval() {
        let mut cache = Cache::default();
        assert!(cache.get(|| anyhow::bail!("shell unavailable")).is_err());
        cache.captured.as_mut().unwrap().0 = Instant::now() - REFRESH_INTERVAL;
        let first = BTreeMap::from([("PATH".into(), "/first".into())]);
        assert_eq!(cache.get(|| Ok(first.clone())).unwrap(), first);
        assert_eq!(
            cache.get(|| panic!("reuse the fresh snapshot")).unwrap(),
            first
        );
        cache.captured.as_mut().unwrap().0 = Instant::now() - REFRESH_INTERVAL;
        let second = BTreeMap::from([("PATH".into(), "/second".into())]);
        assert_eq!(cache.get(|| Ok(second.clone())).unwrap(), second);
    }

    fn slow() -> anyhow::Error {
        st_runtime::ShellStartupTimeout {
            shell: "/bin/fish".into(),
            seconds: 10.0,
        }
        .into()
    }

    #[test]
    fn startup_retries_a_slow_login_shell_with_more_patience_each_time() {
        let environment = BTreeMap::from([("PATH".into(), "/bin".into())]);
        let attempts = [
            Duration::from_secs(10),
            Duration::from_secs(30),
            Duration::from_secs(60),
        ];
        let mut cache = Cache::default();
        let mut allowed = Vec::new();
        let captured = cache
            .start(
                |timeout| {
                    allowed.push(timeout);
                    if allowed.len() < 3 {
                        Err(slow().context("capture"))
                    } else {
                        Ok(environment.clone())
                    }
                },
                &attempts,
                Duration::ZERO,
            )
            .unwrap();
        assert_eq!(captured, environment);
        assert_eq!(allowed, attempts);
        // The daemon's first uses of the cache reuse the capture rather than run the shell again.
        assert_eq!(
            cache.get(|| panic!("reuse the startup capture")).unwrap(),
            environment
        );
    }

    #[test]
    fn startup_gives_up_after_the_last_slow_attempt_and_names_it() {
        let mut calls = 0;
        let error = Cache::default()
            .start(
                |_| {
                    calls += 1;
                    Err(slow())
                },
                &[Duration::from_secs(1), Duration::from_secs(2)],
                Duration::ZERO,
            )
            .unwrap_err();
        assert_eq!(calls, 2);
        assert!(st_runtime::is_shell_startup_timeout(&error), "{error:#}");
    }

    #[test]
    fn startup_does_not_retry_what_waiting_cannot_fix() {
        let mut calls = 0;
        let error = Cache::default()
            .start(
                |_| {
                    calls += 1;
                    anyhow::bail!("the default shell /bin/fish exited with exit status: 1")
                },
                &[Duration::from_secs(1), Duration::from_secs(2)],
                Duration::ZERO,
            )
            .unwrap_err();
        assert_eq!(calls, 1);
        assert!(!st_runtime::is_shell_startup_timeout(&error));
    }

    fn fake_tool(directory: &std::path::Path, name: &str, script: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        let path = directory.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn tools_environment(directory: &std::path::Path, absent: &[&str], cargo: &str) -> Environment {
        for tool in build_tools() {
            if !absent.contains(&tool) {
                fake_tool(
                    directory,
                    tool,
                    if tool == "cargo" { cargo } else { "exit 0" },
                );
            }
        }
        BTreeMap::from([("PATH".into(), directory.display().to_string())])
    }

    #[test]
    fn reports_each_build_tool_missing_from_the_login_path() {
        let root = tempfile::tempdir().unwrap();
        let environment = tools_environment(root.path(), &["sccache", "nix"], "exit 0");
        let report = check_build_tools(&environment);
        assert_eq!(report.missing, ["sccache", "nix"]);
        assert!(report.found.contains(&"cargo") && report.found.contains(&"git"));
        assert!(matches!(report.link, LinkResult::Linked));
    }

    #[test]
    fn nothing_is_built_without_cargo_or_rustc() {
        for absent in ["cargo", "rustc"] {
            let root = tempfile::tempdir().unwrap();
            let environment = tools_environment(root.path(), &[absent], "exit 0");
            let report = check_build_tools(&environment);
            assert_eq!(report.missing, [absent]);
            assert!(matches!(report.link, LinkResult::NotAttempted));
        }
    }

    #[test]
    fn the_scratch_crate_is_built_offline_with_the_repository_link_setup() {
        let root = tempfile::tempdir().unwrap();
        // Only shell builtins: the environment's PATH holds nothing else.
        let cargo = r#"[ "$1 $2 $3" = "build --offline --quiet" ] || exit 7
[ -f Cargo.toml ] && [ -f src/main.rs ] || exit 8
[ -n "$CARGO_TARGET_DIR" ] || exit 9
mold=no
while IFS= read -r line; do case $line in *link-arg=-fuse-ld=mold*) mold=yes;; esac; done < .cargo/config.toml
[ "$mold" = yes ] || exit 10"#;
        let environment = tools_environment(root.path(), &[], cargo);
        assert!(matches!(
            check_build_tools(&environment).link,
            LinkResult::Linked
        ));
    }

    #[test]
    fn a_failed_build_reports_the_last_lines_of_cargo_stderr() {
        let root = tempfile::tempdir().unwrap();
        let cargo = "for n in 1 2 3 4 5; do echo \"line $n\" >&2; done\nexit 101";
        let environment = tools_environment(root.path(), &[], cargo);
        let LinkResult::Failed(error) = check_build_tools(&environment).link else {
            panic!("the build should fail");
        };
        assert!(error.contains("line 3 | line 4 | line 5"), "{error}");
        assert!(!error.contains("line 2"), "{error}");
    }

    #[test]
    fn a_slow_build_is_stopped_with_the_processes_it_started() {
        let root = tempfile::tempdir().unwrap();
        let environment = tools_environment(root.path(), &[], "while :; do :; done");
        let started = Instant::now();
        let error = link_small_crate(&environment, Duration::from_millis(200)).unwrap_err();
        assert!(error.contains("longer than"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn commands_use_only_the_captured_environment_and_path() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("orchid-tool");
        std::fs::write(&executable, "#!/bin/sh\nprintf '%s' \"$ORCHID_VALUE\"\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let environment = BTreeMap::from([
            ("PATH".into(), root.path().display().to_string()),
            ("ORCHID_VALUE".into(), "from-shell".into()),
        ]);
        let output = command_in("orchid-tool", &environment)
            .unwrap()
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"from-shell");
        assert!(command_in("missing-orchid-tool", &environment).is_err());
    }
}
