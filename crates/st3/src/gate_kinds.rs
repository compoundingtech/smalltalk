//! Built-in gate kinds: what gates shelled out for most, answered by st itself.
//!
//! `document` is a graph predicate. `merged`, `ci-passed` and `cargo-test` are exec gates whose
//! command is an `st gate` subcommand, so they run, check again, break and are checked like any
//! exec gate. A mission stores them as the exec gates they expand to, which every st build reads.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A built-in gate's answer and the sentence that explains it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateAnswer {
    Pass(String),
    NotYet(String),
    Broken(String),
}

impl GateAnswer {
    /// The exit status an exec gate reads: 0 passes, 1 is not yet, 3 is broken.
    pub fn exit_code(&self) -> u8 {
        match self {
            GateAnswer::Pass(_) => 0,
            GateAnswer::NotYet(_) => 1,
            GateAnswer::Broken(_) => 3,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            GateAnswer::Pass(reason) => format!("pass: {reason}"),
            GateAnswer::NotYet(reason) => format!("not yet: {reason}"),
            GateAnswer::Broken(reason) => format!("broken: {reason}"),
        }
    }
}

/// `st gate cargo-test`: whether test target `target` of `package` passes at `reference`.
pub struct CargoTest<'a> {
    pub target: &'a str,
    pub package: &'a str,
    /// A ref git can resolve after fetching its remote, such as `origin/main`.
    pub reference: &'a str,
    /// The repository to test, or a directory inside it.
    pub repository: &'a Path,
    /// The worktree the test runs in. st keeps it, and its `target`, between checks.
    pub worktree: Option<&'a Path>,
}

/// Fetch `reference`, check it out in a worktree st keeps for this repository, build the test
/// target and run it. The commands' output goes to this process's output, which is the gate's.
///
/// A ref that has no such test target or package yet, failing tests and a failed fetch are not
/// yet. A directory that is no repository, a ref git cannot resolve, a worktree git cannot make
/// and a test target that does not build break the gate: `main` passed its own CI, so a build that
/// fails here usually means this host lacks something the build needs, such as a linker.
pub fn cargo_test(gate: &CargoTest<'_>) -> GateAnswer {
    let top = match git(gate.repository, &["rev-parse", "--show-toplevel"]) {
        Ok(top) => PathBuf::from(top),
        Err(error) => {
            return GateAnswer::Broken(format!(
                "{} is not inside a git repository: {error}",
                gate.repository.display()
            ));
        }
    };
    if let Some((remote, _)) = gate.reference.split_once('/')
        && git(&top, &["remote"])
            .is_ok_and(|remotes| remotes.lines().any(|line| line.trim() == remote))
        && let Err(error) = git(&top, &["fetch", "--quiet", remote])
    {
        return GateAnswer::NotYet(format!("git could not fetch {remote}: {error}"));
    }
    let sha = match git(
        &top,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{}^{{commit}}", gate.reference),
        ],
    ) {
        Ok(sha) => sha,
        Err(_) => {
            return GateAnswer::Broken(format!(
                "git cannot resolve `{}` in {}",
                gate.reference,
                top.display()
            ));
        }
    };
    let short = &sha[..sha.len().min(12)];
    let worktree = gate
        .worktree
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_worktree(&top));
    let _lock = match lock(&worktree) {
        Ok(lock) => lock,
        Err(error) => return GateAnswer::Broken(format!("{error:#}")),
    };
    if let Err(error) = checkout(&top, &worktree, &sha) {
        return GateAnswer::Broken(format!(
            "git cannot check {short} out in {}: {error}",
            worktree.display()
        ));
    }
    println!(
        "Testing {} of {} at {} ({short}) in {}",
        gate.target,
        gate.package,
        gate.reference,
        worktree.display()
    );
    let build = Command::new("cargo")
        .args([
            "test",
            "-p",
            gate.package,
            "--test",
            gate.target,
            "--no-run",
        ])
        .current_dir(&worktree)
        .output();
    let build = match build {
        Ok(build) => build,
        Err(error) => return GateAnswer::Broken(format!("cargo cannot run here: {error}")),
    };
    let built = String::from_utf8_lossy(&build.stderr).into_owned()
        + &String::from_utf8_lossy(&build.stdout);
    print!("{built}");
    if !build.status.success() {
        if built.contains("no test target named")
            || built.contains("did not match any packages")
            || built.contains("package(s) `")
        {
            return GateAnswer::NotYet(format!(
                "{} ({short}) has no test target `{}` in package `{}` yet",
                gate.reference, gate.target, gate.package
            ));
        }
        return GateAnswer::Broken(format!(
            "test target `{}` does not build at {} ({short}) on this host; its output ends the log",
            gate.target, gate.reference
        ));
    }
    match Command::new("cargo")
        .args(["test", "-p", gate.package, "--test", gate.target])
        .current_dir(&worktree)
        .status()
    {
        Ok(status) if status.success() => GateAnswer::Pass(format!(
            "test target `{}` passes at {} ({short})",
            gate.target, gate.reference
        )),
        Ok(_) => GateAnswer::NotYet(format!(
            "test target `{}` fails at {} ({short})",
            gate.target, gate.reference
        )),
        Err(error) => GateAnswer::Broken(format!("cargo cannot run here: {error}")),
    }
}

/// A worktree for `top` beneath the daemon's state directory, or the system's temporary
/// directory when run outside a gate.
fn default_worktree(top: &Path) -> PathBuf {
    use sha2::Digest as _;
    let digest = hex::encode(sha2::Sha256::digest(top.to_string_lossy().as_bytes()));
    let name = top
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repository".into());
    let root = std::env::var_os("ST3_DRIVER_STATE_DIR")
        .map(PathBuf::from)
        .and_then(|drivers| drivers.parent().map(Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir);
    root.join("gate-worktrees")
        .join(format!("{name}-{}", &digest[..12]))
}

/// Hold the worktree's lock, so two gates never build in one worktree at once.
fn lock(worktree: &Path) -> anyhow::Result<std::fs::File> {
    use std::os::fd::AsRawFd as _;
    let parent = worktree
        .parent()
        .ok_or_else(|| anyhow::anyhow!("the worktree {} has no parent", worktree.display()))?;
    std::fs::create_dir_all(parent)?;
    let path = worktree.with_extension("lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        anyhow::bail!(
            "st cannot lock {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(file)
}

/// Put `worktree` at `sha`: reuse it when it is a worktree, so its `target` keeps the last build.
fn checkout(top: &Path, worktree: &Path, sha: &str) -> Result<(), String> {
    if git(worktree, &["rev-parse", "--git-dir"]).is_ok() {
        git(
            worktree,
            &["checkout", "--quiet", "--detach", "--force", sha],
        )?;
        // Untracked files go; ignored ones such as `target` stay.
        git(worktree, &["clean", "-fdq"])?;
        return Ok(());
    }
    if worktree.exists() {
        std::fs::remove_dir_all(worktree).map_err(|error| error.to_string())?;
    }
    git(top, &["worktree", "prune"])?;
    let path = worktree.to_string_lossy();
    git(top, &["worktree", "add", "--quiet", "--detach", &path, sha])?;
    Ok(())
}

fn git(directory: &Path, arguments: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .map_err(|error| format!("git cannot run: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

/// One shell word, quoted unless it needs no quoting.
pub(crate) fn shell_word(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@,+%#".contains(c))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}
