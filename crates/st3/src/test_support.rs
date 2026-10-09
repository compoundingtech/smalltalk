//! Process-local fixture tools. Only the separate `st3-fixture` executable enables fixture mode;
//! the production CLI has no argument or environment switch for it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

static LOGIN_SHELL: OnceLock<PathBuf> = OnceLock::new();

pub(crate) fn login_shell() -> Option<&'static Path> {
    LOGIN_SHELL.get().map(PathBuf::as_path)
}

/// Called once, before threads start, exclusively by the fixture executable.
#[cfg(feature = "test-support")]
pub fn initialize_fixture() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().expect("fixture shell directory");
    let shell = root.path().join("login-shell");
    // Bypass both /etc startup files and the real account's passwd shell. Fixture profiles
    // still exercise PATH capture, credential changes, and slow-start retry behavior.
    let bash = env!("ST3_FIXTURE_BASH");
    let default_path = if std::env::var_os("PATH").is_none() {
        format!(
            "export PATH='{}'\n",
            env!("ST3_FIXTURE_PATH").replace('\'', "'\\''")
        )
    } else {
        String::new()
    };
    std::fs::write(
        &shell,
        format!(
            "#!{bash}\n{default_path}[ ! -f \"$HOME/.bash_profile\" ] || . \"$HOME/.bash_profile\"\nexec '{bash}' --noprofile --norc \"$@\"\n"
        ),
    )
    .expect("write fixture shell");
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700))
        .expect("executable fixture shell");
    LOGIN_SHELL
        .set(shell)
        .expect("initialize fixture shell once");
    root
}

/// Remove the launching seat's identity without changing the test runner's own environment.
pub fn clear_seat_environment(command: &mut Command) {
    let names = std::env::vars_os()
        .map(|(name, _)| name)
        .chain(command.get_envs().map(|(name, _)| name.to_owned()))
        .collect::<Vec<_>>();
    for name in names {
        let key = name.to_string_lossy();
        if key == "ST_AGENT"
            || key.starts_with("ST3_")
            || key == "ST_HOOKS"
            || key.starts_with("BASH_FUNC_")
        {
            command.env_remove(name);
        }
    }
    // Noninteractive Bash must not load a host-selected startup file.
    for name in ["BASH_ENV", "ENV"] {
        command.env_remove(name);
    }
}

/// A fixture subprocess; explicit identities added by the test remain available.
pub fn command(binary: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(binary);
    clear_seat_environment(&mut command);
    command
}

pub fn async_command(binary: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    command(binary).into()
}

/// Run a process-spawning integration test inside a Linux subreaper. The outer
/// test remains the guardian's liveness owner even when nextest kills it outright.
/// Call first in the test body, before creating runtimes' tasks or fixture files.
/// The inner test keeps all its assertions and timing bounds unchanged.
pub fn supervise_test() -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::io::Write as _;

        let thread = std::thread::current();
        let name = thread.name().expect("libtest names each test thread");
        if std::env::var("SMALLTALK_TEST_SUPERVISED").as_deref() == Ok(name) {
            return false;
        }
        let script =
            Path::new(test_env!("CARGO_MANIFEST_DIR")).join("../../scripts/st3_test_process.py");
        let output = Command::new("python3")
            .arg(script)
            .args(["--owner", &std::process::id().to_string(), "--"])
            .arg(std::env::current_exe().expect("integration test executable"))
            .args(["--exact", name, "--nocapture", "--include-ignored"])
            .env_remove("ST_AGENT")
            .env_remove("ST3_SUBJECT")
            .env("SMALLTALK_TEST_SUPERVISED", name)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .output()
            .expect("launch isolated test guardian");
        let _ = std::io::stdout().write_all(&output.stdout);
        let _ = std::io::stderr().write_all(&output.stderr);
        assert!(
            output.status.success(),
            "supervised test {name}: {}",
            output.status
        );
        true
    }
    #[cfg(not(target_os = "linux"))]
    false
}

/// Git for throwaway repositories. Keep this on the command, never in the runner environment,
/// so real repository commits still obey the host's identity, hook and signing policy.
pub fn git() -> Command {
    let mut command = Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Example")
        .env("GIT_AUTHOR_EMAIL", "example@example.invalid")
        .env("GIT_COMMITTER_NAME", "Example")
        .env("GIT_COMMITTER_EMAIL", "example@example.invalid")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ]);
    command
}

/// An isolated startup test can inspect the real daemon while its replay is in flight.
#[cfg(feature = "test-support")]
pub(crate) fn pause_startup_replay() {
    if login_shell().is_none() {
        return;
    }
    let Some(directory) = std::env::var_os("ST3_TEST_STARTUP_REPLAY_BARRIER") else {
        return;
    };
    let directory = PathBuf::from(directory);
    std::fs::write(directory.join("entered"), b"replay in progress")
        .expect("publish replay barrier");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !directory.join("release").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "startup test did not release replay"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Fixture-only barrier AFTER the actual owned provider has exited and its wrapper returned.
/// The production binary never calls this function; a process environment cannot opt it in.
#[cfg(feature = "test-support")]
pub fn hold_provider_completion(outcome: &anyhow::Result<()>) -> anyhow::Result<()> {
    let Some(root) = std::env::var_os("ST3_FIXTURE_TERMINAL_COMPLETION").map(PathBuf::from) else {
        return Ok(());
    };
    std::fs::write(root.join("provider-return.json"), serde_json::to_vec(&serde_json::json!({
        "ok": outcome.is_ok(), "error": outcome.as_ref().err().map(|error| format!("{error:#}")),
    }))?)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !root.join("release-provider").exists() {
        anyhow::ensure!(std::time::Instant::now() < deadline, "fixture provider release timed out");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    Ok(())
}

/// Exercise the real Store admission in isolated integration fixtures, without an API bypass
/// in the installed CLI. This module exists only for test/test-support builds.
pub fn bind_fixture_mailbox(store: &crate::store::Store, fence: &crate::mailbox::Fence)
    -> Result<crate::mailbox::Fence, crate::St3Error>
{
    store.bind_mailbox(fence)
}

pub fn check_fixture_mailbox(store: &crate::store::Store, fence: &crate::mailbox::Fence)
    -> Result<(), crate::St3Error>
{
    store.check_mailbox(fence)
}

#[cfg(feature = "test-support")]
type MailboxTransportControls = std::sync::Mutex<
    std::collections::BTreeMap<(usize, String), tokio::sync::watch::Sender<bool>>,
>;

#[cfg(feature = "test-support")]
fn mailbox_transports() -> &'static MailboxTransportControls {
    static CONTROLS: OnceLock<MailboxTransportControls> = OnceLock::new();
    CONTROLS.get_or_init(Default::default)
}

/// Process-local transport loss in an invented integration fixture. No API route,
/// CLI flag or environment variable can select a production seat or activate it.
#[cfg(feature = "test-support")]
pub fn hold_fixture_mailbox(store: &std::sync::Arc<crate::store::Store>, subject: &str, held: bool) {
    assert!(subject.starts_with("agent/eval."), "mailbox loss controls require an invented fixture seat");
    mailbox_transports().lock().unwrap()
        .entry((std::sync::Arc::as_ptr(store) as usize, subject.into()))
        .or_insert_with(|| tokio::sync::watch::channel(false).0).send_replace(held);
}

#[cfg(feature = "test-support")]
pub(crate) fn fixture_mailbox_transport(store: &std::sync::Arc<crate::store::Store>, subject: &str) -> tokio::sync::watch::Receiver<bool> {
    mailbox_transports().lock().unwrap()
        .entry((std::sync::Arc::as_ptr(store) as usize, subject.into()))
        .or_insert_with(|| tokio::sync::watch::channel(false).0).subscribe()
}

/// Build an explicit already-admitted protocol fixture. Callers own its temporary Store,
/// socket and synthetic runtime. This adapter preserves kernel peer subject checks and
/// durable Store fences, but cannot qualify native physical ownership or recovery.
#[cfg(feature = "test-support")]
pub fn admitted_mailbox_protocol_router(state: crate::api::AppState) -> axum::Router {
    crate::api::admitted_mailbox_protocol_router(state)
}

/// An invented example seat in a caller-owned temporary Store. This supplies only a
/// synthetic transport identity, not a physical native lease or provider authority.
#[cfg(feature = "test-support")]
pub fn synthetic_mailbox_protocol_router(state: crate::api::AppState, subject: &str) -> axum::Router {
    crate::api::synthetic_mailbox_protocol_router(state, subject)
}

/// Construct durable observations from the older protocol in history/replication fixtures.
/// Modern current-value behavior is tested through its separate admission and forwarding path.
pub fn append_legacy_claim(
    store: &crate::store::Store,
    input: &crate::model::ClaimInput,
) -> Result<crate::model::ClaimRecord, crate::St3Error> {
    store.append_legacy_claim(input)
}
