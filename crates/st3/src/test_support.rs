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
        if key == "ST_AGENT" || key.starts_with("ST3_") || key == "ST_HOOKS"
            || key.starts_with("BASH_FUNC_") {
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
