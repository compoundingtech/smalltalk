//! Process-local fixture tools. Only the separate `st3-fixture` executable enables fixture mode;
//! the production CLI has no argument or environment switch for it.

use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(feature = "test-support")]
pub mod events;

pub use crate::startup::login_shell;
#[cfg(feature = "test-support")]
pub use crate::startup::pause_startup_replay;

/// A temporary directory whose path is already canonical. macOS puts temporary directories
/// below the `/var` and `/tmp` symlinks, and st3 records the canonical path it resolves, so a
/// test that compares paths needs the same spelling the code under test returns. The macOS
/// per-user temporary directory is long even before canonicalizing adds `/private`, and a Unix
/// socket path may not pass 103 bytes, so macOS tests root below `/tmp` instead.
pub fn canonical_tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .tempdir_in(canonical_temp_base())
        .expect("temporary directory")
}

fn canonical_temp_base() -> PathBuf {
    let base = if cfg!(target_os = "macos") {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    std::fs::canonicalize(base).expect("canonical temporary directory")
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
    crate::startup::set_login_shell(shell);
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

/// Whether this executable was built in the `release` profile (`<target dir>/release/deps/NAME`, or
/// `<target dir>/<triple>/release/deps/NAME`). The benchmark and load tests measure time, so they
/// skip in the unoptimized dev and test profiles. They used `cfg!(debug_assertions)` for that, which
/// stops meaning "unoptimized" now that the dev and test profiles build without debug assertions.
pub fn optimized_build() -> bool {
    std::env::current_exe().is_ok_and(|path| built_in_release(&path))
}

/// Only the profile directory two levels above the executable counts, so a custom `CARGO_TARGET_DIR`
/// (any path, even one with a `release` component) cannot change the answer, and a custom profile
/// that merely inherits release gets its own directory name and is treated as not optimized.
fn built_in_release(executable: &Path) -> bool {
    executable
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .is_some_and(|profile| profile == "release")
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

/// The guardian captures this root after reaping its writers, even after SIGKILL.
/// Runtime roots stay short for Unix sockets; only diagnostics go in the CI artifact tree.
pub fn evidence_tempdir() -> tempfile::TempDir {
    use std::io::Write as _;

    let mut root = tempfile::Builder::new()
        .prefix("restart-")
        .tempdir_in(canonical_temp_base())
        .unwrap();
    let Some(manifest) = std::env::var_os("SMALLTALK_TEST_EVIDENCE_MANIFEST") else {
        return root;
    };
    let artifacts = Path::new(test_env!("CARGO_MANIFEST_DIR")).join("../../target/boot-canaries");
    std::fs::create_dir_all(&artifacts).unwrap();
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("daemon-restart").replace("::", "-");
    let evidence = tempfile::Builder::new()
        .prefix(&format!("{name}-"))
        .tempdir_in(artifacts)
        .unwrap()
        .keep()
        .join("evidence");
    std::fs::create_dir_all(&evidence).unwrap();
    let mut journal = std::fs::OpenOptions::new()
        .append(true)
        .open(manifest)
        .unwrap();
    let mut registration = serde_json::to_vec(&serde_json::json!({
        "root": root.path(), "evidence": evidence,
    }))
    .unwrap();
    registration.push(b'\n');
    journal.write_all(&registration).unwrap();
    root.disable_cleanup(true);
    eprintln!("daemon-process evidence: {}", evidence.display());
    root
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

#[cfg(test)]
mod optimized_build_tests {
    use super::built_in_release;
    use std::path::Path;

    #[test]
    fn only_the_profile_directory_above_deps_counts() {
        for (path, release) in [
            ("/w/target/release/deps/t-1", true),
            ("/w/target/x86_64-unknown-linux-gnu/release/deps/t-1", true),
            ("/anywhere/else/release/deps/t-1", true),
            ("/w/target/debug/deps/t-1", false),
            ("/work/release/target/debug/deps/t-1", false),
            ("/release/debug/deps/t-1", false),
            ("/w/target/release-with-debuginfo/deps/t-1", false),
            ("/w/target/debug/release", false),
            ("t-1", false),
        ] {
            assert_eq!(built_in_release(Path::new(path)), release, "{path}");
        }
    }
}
