//! Git worktrees that an agent declares with `checkout`.
//!
//! `checkout "REPOSITORY" base="origin/main" branch="NAME"` asks st3 to create the agent's
//! workspace as a Git worktree of REPOSITORY before the agent starts. `remove-at-run-end=#true`
//! removes a clean worktree after the agent's run ends and its runtime stops.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serde_json::Value;

/// A fetch runs inside the reconciler, so a stalled remote must not hold it for long.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const LOCAL_GIT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub(crate) struct BranchInUse {
    branch: String,
    workspace: String,
}
impl std::fmt::Display for BranchInUse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "branch {} is already checked out in {}; choose another branch or redeclare the seat after releasing that worktree",
            self.branch, self.workspace
        )
    }
}
impl std::error::Error for BranchInUse {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Checkout {
    pub(crate) repository: PathBuf,
    pub(crate) base: String,
    pub(crate) branch: String,
    pub(crate) remove_at_run_end: bool,
}

impl Checkout {
    /// An existing directory satisfies a checkout only when it is the declared repository's
    /// working tree on the declared branch. A plain directory must not bypass checkout creation.
    pub(crate) fn validate_workspace(&self, workspace: &Path) -> Result<()> {
        let repository = std::fs::canonicalize(&self.repository)
            .with_context(|| format!("repository {} is unavailable", self.repository.display()))?;
        let repository =
            crate::repositories::workspace_repository(&repository).unwrap_or(repository);
        anyhow::ensure!(
            workspace.join(".git").exists()
                && crate::repositories::workspace_repository(workspace).as_ref()
                    == Some(&repository)
                && crate::resource::checked_out_branch(workspace).as_deref() == Some(&self.branch),
            "workspace {} is not a worktree of {} on branch {}; choose a new workspace or the matching branch",
            workspace.display(),
            self.repository.display(),
            self.branch
        );
        Ok(())
    }

    /// Read the checkout that an agent declaration asks for.
    pub(crate) fn from_desired(desired: &Value) -> Option<Self> {
        let node = desired
            .get("children")?
            .as_array()?
            .iter()
            .find(|child| child.get("name").and_then(Value::as_str) == Some("checkout"))?;
        let property = |name: &str| node.get("properties")?.get(name);
        Some(Self {
            repository: PathBuf::from(node.get("arguments")?.as_array()?.first()?.as_str()?),
            base: property("base")?.as_str()?.to_owned(),
            branch: property("branch")?.as_str()?.to_owned(),
            remove_at_run_end: property("remove-at-run-end")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    /// Create `workspace` as a worktree on the declared branch. A branch that already exists is
    /// checked out as it is; a new branch starts at the base. When the base names a remote
    /// branch, st3 fetches it first. A failed fetch leaves the local base and returns a warning.
    pub(crate) fn create(&self, workspace: &Path) -> Result<Vec<String>> {
        anyhow::ensure!(
            self.repository.is_absolute(),
            "checkout repository {} is not an absolute path",
            self.repository.display()
        );
        anyhow::ensure!(
            workspace.is_absolute(),
            "checkout workspace {} is not an absolute path",
            workspace.display()
        );
        let mut warnings = Vec::new();
        // Prune registrations whose worktree directory was deleted by hand.
        self.git(&["worktree", "prune"], LOCAL_GIT_TIMEOUT)?;

        // Report a configuration conflict before fetching or creating anything. The reconciler
        // retains this failure until the declaration changes instead of retrying every 30s.
        let worktrees = self.git(
            &["worktree", "list", "--porcelain", "-z"],
            LOCAL_GIT_TIMEOUT,
        )?;
        let mut other_workspace = "";
        for field in worktrees.split('\0') {
            if let Some(path) = field.strip_prefix("worktree ") {
                other_workspace = path;
            }
            if field.strip_prefix("branch refs/heads/") == Some(self.branch.as_str()) {
                return Err(BranchInUse {
                    branch: self.branch.clone(),
                    workspace: other_workspace.into(),
                }
                .into());
            }
        }
        if let Some((remote, branch)) = self.base.split_once('/') {
            let remotes = self.git(&["remote"], LOCAL_GIT_TIMEOUT)?;
            if remotes.lines().any(|line| line == remote)
                && let Err(error) = self.git(&["fetch", "--quiet", remote, branch], FETCH_TIMEOUT)
            {
                warnings.push(format!(
                    "checkout used the local {} because fetching it failed: {error:#}",
                    self.base
                ));
            }
        }
        let workspace = workspace.to_string_lossy();
        let local_branch = format!("refs/heads/{}", self.branch);
        if self
            .git(
                &["rev-parse", "--verify", "--quiet", &local_branch],
                LOCAL_GIT_TIMEOUT,
            )
            .is_ok()
        {
            self.git(
                &["worktree", "add", "--quiet", &workspace, &self.branch],
                LOCAL_GIT_TIMEOUT,
            )?;
        } else {
            self.git(
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "--no-track",
                    "-b",
                    &self.branch,
                    &workspace,
                    &self.base,
                ],
                LOCAL_GIT_TIMEOUT,
            )?;
        }
        Ok(warnings)
    }

    /// Remove a clean worktree. Git refuses a worktree with uncommitted or untracked changes,
    /// so that work stays on disk. The branch stays in the repository.
    pub(crate) fn remove(&self, workspace: &Path) -> Result<()> {
        self.git(
            &["worktree", "remove", &workspace.to_string_lossy()],
            LOCAL_GIT_TIMEOUT,
        )?;
        Ok(())
    }

    fn git(&self, arguments: &[&str], timeout: Duration) -> Result<String> {
        let mut child = crate::environment::command("git")?
            .arg("-C")
            .arg(&self.repository)
            .args(arguments)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("run git")?;
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!(
                    "git {} did not finish within {}s",
                    arguments.join(" "),
                    timeout.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut stdout = String::new();
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stdout.take() {
            pipe.read_to_string(&mut stdout)?;
        }
        if let Some(mut pipe) = child.stderr.take() {
            pipe.read_to_string(&mut stderr)?;
        }
        anyhow::ensure!(
            status.success(),
            "git {} failed: {}",
            arguments.join(" "),
            stderr.trim()
        );
        Ok(stdout)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    pub(crate) fn git(directory: &Path, arguments: &[&str]) {
        let status = crate::test_support::git()
            .arg("-C")
            .arg(directory)
            .args(arguments)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Example Builder")
            .env("GIT_AUTHOR_EMAIL", "builder@example.invalid")
            .env("GIT_COMMITTER_NAME", "Example Builder")
            .env("GIT_COMMITTER_EMAIL", "builder@example.invalid")
            .status()
            .unwrap();
        assert!(status.success(), "git {arguments:?}");
    }

    /// A bare origin with one commit on main, and a clone of it.
    pub(crate) fn repository(root: &Path) -> PathBuf {
        let origin = root.join("origin.git");
        let seed = root.join("seed");
        let clone = root.join("repo");
        git(
            root,
            &["init", "--quiet", "--bare", "-b", "main", "origin.git"],
        );
        git(root, &["init", "--quiet", "-b", "main", "seed"]);
        std::fs::write(seed.join("README"), "first\n").unwrap();
        git(&seed, &["add", "README"]);
        git(&seed, &["commit", "--quiet", "-m", "first"]);
        git(
            &seed,
            &["push", "--quiet", &origin.to_string_lossy(), "main"],
        );
        git(
            root,
            &["clone", "--quiet", &origin.to_string_lossy(), "repo"],
        );
        // A commit that reaches origin after the clone is only visible after a fetch.
        std::fs::write(seed.join("README"), "second\n").unwrap();
        git(&seed, &["commit", "--quiet", "-am", "second"]);
        git(
            &seed,
            &["push", "--quiet", &origin.to_string_lossy(), "main"],
        );
        clone
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{git, repository};
    use super::*;

    #[test]
    fn a_checkout_is_read_from_its_agent_declaration() {
        let desired = serde_json::json!({
            "name": "agent",
            "children": [
                {"name": "workspace", "arguments": ["/work/example--parser"]},
                {
                    "name": "checkout",
                    "arguments": ["/work/example"],
                    "properties": {
                        "base": "origin/main",
                        "branch": "example/parser",
                        "remove-at-run-end": true
                    }
                }
            ]
        });
        assert_eq!(
            Checkout::from_desired(&desired),
            Some(Checkout {
                repository: "/work/example".into(),
                base: "origin/main".into(),
                branch: "example/parser".into(),
                remove_at_run_end: true,
            })
        );
        assert_eq!(
            Checkout::from_desired(&serde_json::json!({"name": "agent", "children": []})),
            None
        );
    }

    #[test]
    fn a_checkout_creates_a_fresh_worktree_and_removes_it_only_when_clean() {
        let root = tempfile::tempdir().unwrap();
        let repository = repository(root.path());
        let checkout = Checkout {
            repository: repository.clone(),
            base: "origin/main".into(),
            branch: "example/parser".into(),
            remove_at_run_end: true,
        };
        let workspace = root.path().join("parser");
        assert!(checkout.create(&workspace).unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(workspace.join("README")).unwrap(),
            "second\n",
            "the worktree starts from the fetched base"
        );
        let head = crate::test_support::git()
            .arg("-C")
            .arg(&workspace)
            .args(["branch", "--show-current"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&head.stdout).trim(),
            "example/parser"
        );

        // Uncommitted work keeps the worktree.
        std::fs::write(workspace.join("notes.txt"), "unfinished\n").unwrap();
        let error = checkout.remove(&workspace).unwrap_err();
        assert!(error.to_string().contains("worktree remove"), "{error:#}");
        assert!(workspace.join("notes.txt").is_file());

        std::fs::remove_file(workspace.join("notes.txt")).unwrap();
        checkout.remove(&workspace).unwrap();
        assert!(!workspace.exists());

        // A later run reuses the branch the first run left behind.
        assert!(checkout.create(&workspace).unwrap().is_empty());
        assert!(workspace.join("README").is_file());
    }

    #[test]
    fn a_checkout_without_a_reachable_remote_uses_the_local_base() {
        let root = tempfile::tempdir().unwrap();
        let repository = repository(root.path());
        git(
            &repository,
            &["remote", "set-url", "origin", "/nonexistent/example.git"],
        );
        let checkout = Checkout {
            repository,
            base: "origin/main".into(),
            branch: "example/offline".into(),
            remove_at_run_end: false,
        };
        let workspace = root.path().join("offline");
        let warnings = checkout.create(&workspace).unwrap();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("used the local origin/main"));
        assert_eq!(
            std::fs::read_to_string(workspace.join("README")).unwrap(),
            "first\n"
        );
    }

    #[test]
    fn an_existing_directory_must_match_the_checkout_repository_and_branch() {
        let root = tempfile::tempdir().unwrap();
        let repository = repository(root.path());
        let checkout = Checkout {
            repository,
            base: "origin/main".into(),
            branch: "parser".into(),
            remove_at_run_end: false,
        };
        assert!(checkout.validate_workspace(root.path()).is_err());
        let workspace = root.path().join("parser");
        checkout.create(&workspace).unwrap();
        checkout.validate_workspace(&workspace).unwrap();
        let wrong_branch = Checkout {
            branch: "other".into(),
            ..checkout.clone()
        };
        assert!(
            wrong_branch
                .validate_workspace(&workspace)
                .unwrap_err()
                .to_string()
                .contains("branch other")
        );
        let missing = Checkout {
            repository: root.path().join("missing"),
            ..checkout
        };
        assert!(
            missing
                .validate_workspace(&workspace)
                .unwrap_err()
                .to_string()
                .contains("unavailable")
        );
    }

    #[test]
    fn a_checkout_needs_absolute_paths() {
        let checkout = Checkout {
            repository: "relative/example".into(),
            base: "main".into(),
            branch: "example/branch".into(),
            remove_at_run_end: false,
        };
        let error = checkout.create(Path::new("/work/example")).unwrap_err();
        assert!(error.to_string().contains("not an absolute path"));
    }
}
