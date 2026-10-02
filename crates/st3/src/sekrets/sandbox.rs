//! How the gateway runs a command: as the sekrets user, inside bubblewrap, seeing
//!
//! - the system read-only, with the sekrets store, the gateway socket and every home hidden;
//! - this one profile's home, read-write, and no other profile's;
//! - the caller's checkout, bound read-only from the directory descriptors the caller passed,
//!   with the repository's git configuration replaced by a copy that keeps only data (remotes,
//!   branches, identity) and its hooks directory replaced by an empty one.
//!
//! The command and every tool it runs come from the gateway's trusted path, never the caller's,
//! and the environment is built from nothing. A command that runs code from the checkout can at
//! worst use the one profile it runs as, never read another or the store.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};

/// Whether `path` and every directory above it can be changed only by root or `owner`: each is
/// owned by one of them, and a directory others may write to has the sticky bit. Symlinks are
/// resolved first, so the check is of the file that would run.
pub fn trusted_path(path: &Path, owner: u32) -> Result<PathBuf> {
    let resolved = fs::canonicalize(path).with_context(|| format!("resolve {}", path.display()))?;
    let mut current = Some(resolved.as_path());
    while let Some(entry) = current {
        let metadata =
            fs::metadata(entry).with_context(|| format!("inspect {}", entry.display()))?;
        let uid = metadata.uid();
        if uid != 0 && uid != owner {
            bail!(
                "{} is owned by uid {uid}, not root; the gateway runs only files root controls",
                entry.display()
            );
        }
        let mode = metadata.permissions().mode();
        let others_write = mode & 0o022 != 0;
        if others_write && !(metadata.is_dir() && mode & 0o1000 != 0) {
            bail!(
                "{} can be changed by others (mode {:o}); the gateway runs only files root controls",
                entry.display(),
                mode & 0o7777
            );
        }
        current = entry.parent();
    }
    Ok(resolved)
}

/// Find `name` in the trusted directories and check it.
pub fn resolve_tool(name: &str, directories: &[PathBuf], owner: u32) -> Result<PathBuf> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        bail!("`{name}`: name a command, not a path; the gateway finds it on its own path");
    }
    for directory in directories {
        let candidate = directory.join(name);
        if let Ok(metadata) = fs::metadata(&candidate)
            && metadata.is_file()
            && metadata.permissions().mode() & 0o111 != 0
        {
            return trusted_path(&candidate, owner);
        }
    }
    bail!(
        "`{name}` is not on the gateway's path ({})",
        directories
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(":")
    )
}

/// The kernel's path for a directory descriptor.
pub fn descriptor_path(fd: &OwnedFd) -> Result<PathBuf> {
    let path = fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))
        .context("read a passed directory's path")?;
    if !path.is_absolute() || path.to_string_lossy().ends_with(" (deleted)") {
        bail!("a passed directory no longer exists");
    }
    Ok(path)
}

/// Where a checkout directory may be bound: under one of `roots`, by a plain path.
pub fn check_checkout_path(path: &Path, roots: &[PathBuf]) -> Result<()> {
    if path
        .components()
        .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
    {
        bail!("{} is not a plain absolute path", path.display());
    }
    if !roots
        .iter()
        .any(|root| path.starts_with(root) && path != root)
    {
        bail!(
            "{} is not under a checkout root the gateway serves ({})",
            path.display(),
            roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

/// Git configuration keys a command run for someone else may keep: data, never a program, a path
/// to run, a hook, a credential helper, a URL rewrite or an include.
fn keep_git_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    let (section, rest) = lower.split_once('.').unwrap_or((&lower, ""));
    let name = rest.rsplit('.').next().unwrap_or("");
    let has_subsection = rest.contains('.');
    match section {
        "core" if !has_subsection => matches!(
            name,
            "repositoryformatversion"
                | "bare"
                | "filemode"
                | "logallrefupdates"
                | "ignorecase"
                | "precomposeunicode"
                | "symlinks"
                | "autocrlf"
                | "eol"
                | "abbrev"
        ),
        "extensions" => !has_subsection,
        "remote" if has_subsection => {
            matches!(name, "url" | "pushurl" | "fetch" | "push" | "gh-resolved")
        }
        "branch" if has_subsection => matches!(name, "remote" | "merge" | "pushremote"),
        "push" if !has_subsection => matches!(name, "default" | "autosetupremote"),
        "user" if !has_subsection => matches!(name, "name" | "email"),
        "init" if !has_subsection => name == "defaultbranch",
        _ => false,
    }
}

/// Parse `git config --list --null` output into ordered key/value pairs.
fn parse_config_list(bytes: &[u8]) -> Vec<(String, String)> {
    bytes
        .split(|b| *b == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let text = String::from_utf8_lossy(entry);
            match text.split_once('\n') {
                Some((key, value)) => (key.to_owned(), value.to_owned()),
                None => (text.into_owned(), String::from("true")),
            }
        })
        .collect()
}

fn quote_config(value: &str) -> String {
    let mut quoted = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// A git config file with only the kept keys of `pairs`.
pub fn sanitized_git_config(pairs: &[(String, String)]) -> String {
    // Keyed by section and subsection, such as ("remote", Some("origin")).
    type Header = (String, Option<String>);
    let mut sections: BTreeMap<Header, Vec<(String, String)>> = BTreeMap::new();
    let mut order = Vec::new();
    for (key, value) in pairs {
        if !keep_git_key(key) {
            continue;
        }
        let Some((section, rest)) = key.split_once('.') else {
            continue;
        };
        let (subsection, name) = match rest.rsplit_once('.') {
            Some((subsection, name)) => (Some(subsection.to_owned()), name.to_owned()),
            None => (None, rest.to_owned()),
        };
        let header = (section.to_ascii_lowercase(), subsection);
        if !sections.contains_key(&header) {
            order.push(header.clone());
        }
        sections
            .entry(header)
            .or_default()
            .push((name, value.clone()));
    }
    let mut text = String::new();
    for header in order {
        match &header.1 {
            Some(subsection) => {
                text.push_str(&format!("[{} {}]\n", header.0, quote_config(subsection)))
            }
            None => text.push_str(&format!("[{}]\n", header.0)),
        }
        for (name, value) in &sections[&header] {
            text.push_str(&format!("\t{name} = {}\n", quote_config(value)));
        }
    }
    text
}

/// Read a repository config file as data with git's own parser: `--file` reads no includes and
/// runs nothing.
pub fn read_git_config(git: &Path, file: &Path) -> Result<Vec<(String, String)>> {
    let output = Command::new(git)
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", "/nonexistent")
        .args(["config", "--no-includes", "--null", "--list", "--file"])
        .arg(file)
        .output()
        .with_context(|| format!("read {}", file.display()))?;
    if !output.status.success() {
        bail!(
            "git could not read {}: {}",
            file.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(parse_config_list(&output.stdout))
}

/// Configuration every sandboxed git gets on its command line, which overrides the repository's:
/// no hooks, no file system monitor, no local or external transports, and checkouts owned by the
/// caller count as safe.
pub const GIT_OVERRIDES: &[(&str, &str)] = &[
    ("core.hooksPath", "/dev/null"),
    ("core.fsmonitor", "false"),
    ("core.sshCommand", "ssh"),
    ("core.pager", "cat"),
    ("core.askPass", "false"),
    ("core.editor", "false"),
    ("sequence.editor", "false"),
    ("protocol.allow", "never"),
    ("protocol.https.allow", "always"),
    ("protocol.ssh.allow", "always"),
    ("safe.directory", "*"),
    ("safe.bareRepository", "explicit"),
    ("submodule.recurse", "false"),
    ("push.recurseSubmodules", "no"),
    ("fetch.recurseSubmodules", "false"),
    ("diff.ignoreSubmodules", "all"),
    ("status.submoduleSummary", "false"),
    ("push.gpgSign", "false"),
    ("commit.gpgSign", "false"),
    ("tag.gpgSign", "false"),
    ("gpg.program", "false"),
    ("gpg.ssh.program", "false"),
    ("gpg.x509.program", "false"),
];

/// One directory the caller passed, bound at the path the kernel gives it.
pub struct BoundDirectory {
    pub fd: OwnedFd,
    pub path: PathBuf,
}

/// A file the gateway wrote, bound over a path in a bound directory.
pub struct Overlay {
    pub source: PathBuf,
    pub target: PathBuf,
}

pub struct Sandbox<'a> {
    pub bwrap: &'a Path,
    pub tool: &'a Path,
    pub args: &'a [String],
    pub home: &'a Path,
    /// Directories the command must not see at all.
    pub hidden: &'a [PathBuf],
    pub path: &'a [PathBuf],
    pub directories: &'a [BoundDirectory],
    pub overlays: &'a [Overlay],
    pub empty_dirs: &'a [PathBuf],
    pub cwd: &'a Path,
    pub env: &'a [(String, String)],
    pub term: Option<&'a str>,
    pub user: &'a str,
    /// Run in a new session: a command without a terminal must not reach the caller's.
    pub new_session: bool,
}

impl Sandbox<'_> {
    /// The bubblewrap command. The bound directories' descriptors must stay open, and not
    /// close-on-exec, until bubblewrap starts.
    pub fn command(&self) -> Command {
        let mut command = Command::new(self.bwrap);
        command.env_clear();
        let mut env: Vec<(String, OsString)> = vec![
            ("HOME".into(), self.home.into()),
            ("USER".into(), self.user.into()),
            ("LOGNAME".into(), self.user.into()),
            (
                "PATH".into(),
                std::env::join_paths(self.path).unwrap_or_default(),
            ),
            ("XDG_CONFIG_HOME".into(), self.home.join(".config").into()),
            ("XDG_CACHE_HOME".into(), self.home.join(".cache").into()),
            (
                "XDG_DATA_HOME".into(),
                self.home.join(".local/share").into(),
            ),
            (
                "XDG_STATE_HOME".into(),
                self.home.join(".local/state").into(),
            ),
            ("TMPDIR".into(), "/tmp".into()),
            ("LANG".into(), "C.UTF-8".into()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            // Git looks for a repository no higher than the passed checkout.
            (
                "GIT_CEILING_DIRECTORIES".into(),
                std::env::join_paths(self.hidden).unwrap_or_default(),
            ),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("GIT_PAGER".into(), "cat".into()),
            ("PAGER".into(), "cat".into()),
            ("GIT_EDITOR".into(), "false".into()),
            ("EDITOR".into(), "false".into()),
            ("VISUAL".into(), "false".into()),
            ("GH_PROMPT_DISABLED".into(), "1".into()),
            (
                "GIT_CONFIG_COUNT".into(),
                GIT_OVERRIDES.len().to_string().into(),
            ),
        ];
        if let Some(term) = self.term {
            env.push(("TERM".into(), term.into()));
            env.retain(|(name, _)| name != "GH_PROMPT_DISABLED");
        }
        for (index, (key, value)) in GIT_OVERRIDES.iter().enumerate() {
            env.push((format!("GIT_CONFIG_KEY_{index}"), (*key).into()));
            env.push((format!("GIT_CONFIG_VALUE_{index}"), (*value).into()));
        }
        for (name, value) in self.env {
            env.retain(|(existing, _)| existing != name);
            env.push((name.clone(), value.into()));
        }
        command.envs(env);
        command.args([
            "--die-with-parent",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-cgroup-try",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/home",
            "--tmpfs",
            "/root",
            "--tmpfs",
            "/var/tmp",
        ]);
        if self.new_session {
            command.arg("--new-session");
        }
        for hidden in self.hidden {
            if hidden.exists() {
                command.arg("--tmpfs").arg(hidden);
            }
        }
        // The trusted tool directories stay visible even under a hidden directory.
        for directory in self.path {
            if let Ok(directory) = fs::canonicalize(directory)
                && directory.is_dir()
            {
                command.arg("--ro-bind").arg(&directory).arg(&directory);
            }
        }
        command.arg("--bind").arg(self.home).arg(self.home);
        for directory in self.directories {
            command
                .arg("--ro-bind-fd")
                .arg(directory.fd.as_raw_fd().to_string())
                .arg(&directory.path);
        }
        for overlay in self.overlays {
            command
                .arg("--ro-bind")
                .arg(&overlay.source)
                .arg(&overlay.target);
        }
        for empty in self.empty_dirs {
            command.arg("--tmpfs").arg(empty);
        }
        command.arg("--chdir").arg(self.cwd);
        command.arg("--").arg(self.tool).args(self.args);
        command
    }
}

/// Lexically clean an absolute path: drop `.`, resolve `..`, keep it absolute.
fn clean(path: &Path) -> PathBuf {
    let mut cleaned = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::Normal(part) => cleaned.push(part),
            Component::ParentDir => {
                cleaned.pop();
            }
            _ => {}
        }
    }
    cleaned
}

/// What a checkout looks like inside the sandbox: sanitized git configuration over the
/// repository's own, and empty directories over its hooks and submodule repositories.
#[derive(Default)]
pub struct CheckoutView {
    pub overlays: Vec<Overlay>,
    pub empty_dirs: Vec<PathBuf>,
}

/// Where the gateway reads `path`, a path inside one of the bound directories: through that
/// directory's descriptor, so it never needs the right to walk the caller's home.
fn host_path(directories: &[BoundDirectory], path: &Path) -> Option<PathBuf> {
    let directory = directories
        .iter()
        .filter(|directory| path.starts_with(&directory.path))
        .max_by_key(|directory| directory.path.components().count())?;
    let relative = path.strip_prefix(&directory.path).ok()?;
    Some(PathBuf::from(format!("/proc/self/fd/{}", directory.fd.as_raw_fd())).join(relative))
}

/// The git directory a `.git` entry at `level` names, and its common directory.
/// The git directory a `.git` entry at `level` names, and its common directory. Each must be a
/// real directory inside the passed checkout: git follows a symbolic link or a `gitdir:` line
/// anywhere it can see, including directories every user may write such as `/var/tmp`, and a
/// git directory there would keep its own configuration.
fn git_dirs_at(directories: &[BoundDirectory], level: &Path) -> Result<Option<Vec<PathBuf>>> {
    let entry = level.join(".git");
    let Some(host) = host_path(directories, &entry) else {
        return Ok(None);
    };
    let Ok(metadata) = fs::symlink_metadata(&host) else {
        return Ok(None);
    };
    let git_dir = if metadata.is_dir() {
        entry
    } else if metadata.is_file() {
        let text =
            fs::read_to_string(&host).with_context(|| format!("read {}", entry.display()))?;
        let Some(named) = text.trim().strip_prefix("gitdir:") else {
            bail!("{} is not a git directory pointer", entry.display());
        };
        clean(&level.join(named.trim()))
    } else {
        bail!(
            "{} is a symbolic link; the gateway serves only a checkout whose .git is a directory or a gitdir file",
            entry.display()
        );
    };
    let mut found = vec![git_dir.clone()];
    if let Some(host) = host_path(directories, &git_dir.join("commondir"))
        && let Ok(text) = fs::read_to_string(host)
    {
        found.push(clean(&git_dir.join(text.trim())));
    }
    for git_dir in &found {
        let real = host_path(directories, git_dir)
            .and_then(|host| fs::symlink_metadata(host).ok())
            .is_some_and(|metadata| metadata.is_dir());
        if !real {
            bail!(
                "{} is not a directory inside the checkout passed to the gateway",
                git_dir.display()
            );
        }
    }
    Ok(Some(found))
}

/// Find every git directory a command in `cwd` could open and plan its sanitized view: the
/// repository above `cwd`, its common directory, and any passed directory that is itself one.
pub fn prepare_checkout(
    directories: &[BoundDirectory],
    cwd: &Path,
    git: Option<&Path>,
    scratch: &Path,
) -> Result<CheckoutView> {
    let mut git_dirs = Vec::<PathBuf>::new();
    for level in cwd.ancestors() {
        if host_path(directories, level).is_none() {
            break;
        }
        if let Some(found) = git_dirs_at(directories, level)? {
            git_dirs.extend(found);
            break;
        }
    }
    for directory in directories {
        let base = PathBuf::from(format!("/proc/self/fd/{}", directory.fd.as_raw_fd()));
        if base.join("HEAD").is_file() && base.join("config").is_file() {
            git_dirs.push(directory.path.clone());
        }
    }
    git_dirs.sort();
    git_dirs.dedup();
    let mut view = CheckoutView::default();
    for (index, git_dir) in git_dirs.iter().enumerate() {
        for name in ["config", "config.worktree"] {
            let target = git_dir.join(name);
            let Some(host) = host_path(directories, &target) else {
                continue;
            };
            match fs::symlink_metadata(&host) {
                Ok(metadata) if metadata.is_file() => {}
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!(
                        "{} is a symbolic link; the gateway reads only a plain file",
                        target.display()
                    )
                }
                _ => continue,
            }
            let Some(git) = git else {
                bail!(
                    "{} has git configuration, and the gateway has no trusted git to read it with",
                    target.display()
                );
            };
            // git reads a copy: it cannot open the gateway's descriptor paths.
            let original = scratch.join(format!("{index}-{name}.original"));
            fs::copy(&host, &original).with_context(|| format!("read {}", target.display()))?;
            let pairs = read_git_config(git, &original)?;
            let source = scratch.join(format!("{index}-{name}"));
            fs::write(&source, sanitized_git_config(&pairs))
                .with_context(|| format!("write {}", source.display()))?;
            view.overlays.push(Overlay { source, target });
        }
        for name in ["hooks", "modules"] {
            let target = git_dir.join(name);
            if host_path(directories, &target).is_some_and(|host| host.is_dir()) {
                view.empty_dirs.push(target);
            }
        }
    }
    Ok(view)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_data_survives_in_a_sanitized_git_config() {
        let pairs = vec![
            ("core.repositoryformatversion".into(), "0".into()),
            ("core.hookspath".into(), "/tmp/evil".into()),
            ("core.fsmonitor".into(), "/tmp/evil".into()),
            ("core.sshcommand".into(), "evil".into()),
            ("remote.origin.url".into(), "https://example.com/a/b".into()),
            ("remote.origin.receivepack".into(), "evil".into()),
            ("remote.origin.uploadpack".into(), "evil".into()),
            ("remote.origin.proxy".into(), "evil".into()),
            (
                "branch.agent/x.y.merge".into(),
                "refs/heads/agent/x.y".into(),
            ),
            ("branch.agent/x.y.remote".into(), "origin".into()),
            (
                "url.https://evil/.insteadof".into(),
                "https://example.com/".into(),
            ),
            ("include.path".into(), "/tmp/evil.conf".into()),
            ("credential.helper".into(), "!cat ~/.git-credentials".into()),
            ("filter.lfs.process".into(), "evil".into()),
            ("diff.external".into(), "evil".into()),
            ("user.name".into(), "Ada \"A\" Example".into()),
            ("extensions.worktreeconfig".into(), "true".into()),
            ("gpg.program".into(), "evil".into()),
        ];
        let text = sanitized_git_config(&pairs);
        assert!(!text.contains("evil"), "{text}");
        assert!(text.contains("[remote \"origin\"]\n\turl = \"https://example.com/a/b\""));
        assert!(text.contains("[branch \"agent/x.y\"]\n\tmerge = \"refs/heads/agent/x.y\""));
        assert!(text.contains("name = \"Ada \\\"A\\\" Example\""));
        assert!(text.contains("worktreeconfig"));
    }

    #[test]
    fn git_reads_the_sanitized_config_back() {
        let Ok(git) = which("git") else { return };
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("config");
        fs::write(
            &original,
            "[core]\n\thooksPath = /tmp/x\n[remote \"origin\"]\n\turl = https://example.com/r\n\
             [branch \"a.b\"]\n\tmerge = refs/heads/a.b\n[include]\n\tpath = other\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("other"),
            "[core]\n\tfsmonitor = /tmp/x\n",
        )
        .unwrap();
        let pairs = read_git_config(&git, &original).unwrap();
        assert!(!pairs.iter().any(|(k, _)| k == "core.fsmonitor"));
        let clean = directory.path().join("clean");
        fs::write(&clean, sanitized_git_config(&pairs)).unwrap();
        let back = read_git_config(&git, &clean).unwrap();
        assert_eq!(
            back,
            vec![
                (
                    "remote.origin.url".to_owned(),
                    "https://example.com/r".to_owned()
                ),
                ("branch.a.b.merge".to_owned(), "refs/heads/a.b".to_owned()),
            ]
        );
    }

    #[test]
    fn checkout_paths_stay_under_their_roots() {
        let roots = vec![PathBuf::from("/home")];
        assert!(check_checkout_path(Path::new("/home/example/src/web"), &roots).is_ok());
        assert!(check_checkout_path(Path::new("/home"), &roots).is_err());
        assert!(check_checkout_path(Path::new("/usr/bin"), &roots).is_err());
        assert!(check_checkout_path(Path::new("/home/../usr/bin"), &roots).is_err());
        assert!(check_checkout_path(Path::new("/homes/x"), &roots).is_err());
    }

    #[test]
    fn tools_run_only_from_root_controlled_paths() {
        let directory = tempfile::tempdir().unwrap();
        let tool = directory.path().join("tool");
        fs::write(&tool, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let me = unsafe { libc::getuid() };
        if me != 0 {
            // Owned by this user, under /tmp: trusted only for this user, never for another.
            assert!(trusted_path(&tool, me + 1).is_err());
        }
        assert!(resolve_tool("../tool", &[directory.path().into()], me).is_err());
        assert!(resolve_tool("/bin/sh", &[directory.path().into()], me).is_err());
        if let Ok(sh) = trusted_path(Path::new("/bin/sh"), u32::MAX) {
            assert!(sh.is_absolute());
        }
    }

    fn which(name: &str) -> Result<PathBuf> {
        for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        bail!("no {name}")
    }
}
