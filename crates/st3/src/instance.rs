//! A named, fully separate st on one machine.
//!
//! `st --instance NAME` (or `ST_INSTANCE=NAME`) runs a second Smalltalk next to the default one for
//! testing and for a clean first run. Everything the instance owns lives under one directory,
//! `~/.st-instance/NAME`, so removing that directory (plus the service definitions that point into
//! it) removes the instance and nothing else:
//!
//! ```text
//! ~/.st-instance/NAME/
//!   state/   daemon state, graph, drivers      (XDG_STATE_HOME)
//!   config/  config.toml and the fleet file    (XDG_CONFIG_HOME)
//!   data/    the install manifest              (XDG_DATA_HOME)
//!   cache/   derived caches                    (XDG_CACHE_HOME)
//!   run/     sockets                           (XDG_RUNTIME_DIR)
//!   bin/     the st3, st and pty setup installs
//!   claude/  the instance's Claude account     (CLAUDE_CONFIG_DIR)
//!   codex/   the instance's Codex account      (CODEX_HOME)
//!   agents/  default workspaces for new agents
//!   app/     where a macOS app bundle for the instance goes
//! ```
//!
//! A bare `HOME` override is not enough: on macOS launchd labels and the app bundle live outside
//! `HOME`'s control and would collide. So the instance also gets its own service names, a launchd
//! label and a systemd unit that carry the name, and a default node name that does too.
//!
//! The instance is applied to the environment once, at startup, before any thread exists. Every
//! child (the daemon, its seats, their harnesses) inherits it, which is how the places that read
//! `XDG_*_HOME` themselves land in the instance without each knowing about instances. The
//! service manager is the one thing that must keep talking to the real user session, so the
//! original values are kept in `ST_INSTANCE_HOST_*` and restored for those commands.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result};

/// Names the instance for this process and everything it starts.
pub const ENV: &str = "ST_INSTANCE";

/// The directory below `HOME` that holds every instance.
const ROOT_DIRECTORY: &str = ".st-instance";

/// The XDG variables an instance replaces, each paired with the variable that remembers the
/// host's value for the service manager.
const XDG: [(&str, &str); 4] = [
    ("XDG_STATE_HOME", "ST_INSTANCE_HOST_XDG_STATE_HOME"),
    ("XDG_CONFIG_HOME", "ST_INSTANCE_HOST_XDG_CONFIG_HOME"),
    ("XDG_DATA_HOME", "ST_INSTANCE_HOST_XDG_DATA_HOME"),
    ("XDG_RUNTIME_DIR", "ST_INSTANCE_HOST_XDG_RUNTIME_DIR"),
];

const LONGEST_NAME: usize = 24;

/// The only names an instance may have: short enough for a Unix socket path below `HOME`, and
/// plain enough to be a directory, a unit name and a launchd label without escaping.
pub fn validate(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty() && name.len() <= LONGEST_NAME,
        "an instance name is 1 to {LONGEST_NAME} characters"
    );
    anyhow::ensure!(
        name.bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
        "an instance name uses lowercase letters, digits and hyphens"
    );
    anyhow::ensure!(
        !name.starts_with('-') && !name.ends_with('-') && !name.contains("--"),
        "an instance name does not start or end with a hyphen or repeat one"
    );
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instance {
    name: String,
    root: PathBuf,
}

impl Instance {
    pub fn new(home: &Path, name: &str) -> Result<Self> {
        validate(name)?;
        anyhow::ensure!(home.is_absolute(), "HOME is not an absolute directory");
        Ok(Self {
            name: name.to_owned(),
            root: home.join(ROOT_DIRECTORY).join(name),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The one directory the instance owns.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state_home(&self) -> PathBuf {
        self.root.join("state")
    }

    pub fn config_home(&self) -> PathBuf {
        self.root.join("config")
    }

    pub fn data_home(&self) -> PathBuf {
        self.root.join("data")
    }

    pub fn cache_home(&self) -> PathBuf {
        self.root.join("cache")
    }

    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }

    pub fn claude_config_dir(&self) -> PathBuf {
        self.root.join("claude")
    }

    pub fn codex_home(&self) -> PathBuf {
        self.root.join("codex")
    }

    pub fn agents_dir(&self) -> PathBuf {
        self.root.join("agents")
    }

    /// Where a macOS app bundle for this instance is installed, never the shared application folder.
    pub fn app_dir(&self) -> PathBuf {
        self.root.join("app")
    }

    /// The prefix keeps an instance named like a default service (`replication`) from colliding.
    pub fn systemd_service(&self) -> String {
        format!("st3-instance-{}.service", self.name)
    }

    pub fn systemd_replication_service(&self) -> String {
        format!("st3-instance-{}-replication.service", self.name)
    }

    pub fn launchd_label(&self) -> String {
        format!("com.compoundingtech.st3.instance.{}", self.name)
    }

    pub fn launchd_replication_label(&self) -> String {
        format!("com.compoundingtech.st3.instance.{}.replication", self.name)
    }

    /// The default machine name: distinct from the host's, so a fleet sees two machines.
    pub fn node_name(&self, host: &str) -> String {
        format!("{host}-{}", self.name)
    }

    /// The variables that put a process and its children in this instance.
    pub fn environment(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("XDG_STATE_HOME", self.state_home()),
            ("XDG_CONFIG_HOME", self.config_home()),
            ("XDG_DATA_HOME", self.data_home()),
            ("XDG_CACHE_HOME", self.cache_home()),
            ("XDG_RUNTIME_DIR", self.run_dir()),
            ("CLAUDE_CONFIG_DIR", self.claude_config_dir()),
            ("CODEX_HOME", self.codex_home()),
        ]
    }
}

/// The instance this process runs in, if any. `ST_INSTANCE` that is set but not a valid name ends
/// the process: guessing would run the default install when a person asked for a separate one.
pub fn current() -> Option<Instance> {
    let name = env::var(ENV).ok().filter(|name| !name.is_empty())?;
    let home = match env::var_os("HOME").map(PathBuf::from) {
        Some(home) => home,
        None => fail("HOME is not set, so the instance directory cannot be found"),
    };
    match Instance::new(&home, &name) {
        Ok(instance) => Some(instance),
        Err(error) => fail(&format!("{ENV}={name}: {error:#}")),
    }
}

fn fail(message: &str) -> ! {
    eprintln!("st: {message}");
    std::process::exit(2);
}

/// Look for `--instance NAME` or `--instance=NAME` in the command line, which wins over the
/// environment. Arguments after `--` belong to a program the command runs.
pub fn requested(arguments: &[std::ffi::OsString]) -> Option<String> {
    let mut found = None;
    let mut iterator = arguments.iter().skip(1);
    while let Some(argument) = iterator.next() {
        let Some(argument) = argument.to_str() else {
            continue;
        };
        if argument == "--" {
            break;
        }
        if let Some(value) = argument.strip_prefix("--instance=") {
            found = Some(value.to_owned());
        } else if argument == "--instance" {
            found = iterator.next().and_then(|value| value.to_str()).map(str::to_owned);
        }
    }
    found
}

/// Put this process in the instance. Call once, first thing, before any thread starts.
///
/// # Safety
///
/// Changes the process environment, so no other thread may be running.
pub unsafe fn activate(name: &str) -> Result<Instance> {
    validate(name)?;
    if let Ok(active) = env::var(ENV)
        && !active.is_empty()
        && active != name
    {
        anyhow::bail!("this process already runs in instance `{active}`, not `{name}`");
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set, so the instance directory cannot be found")?;
    let instance = Instance::new(&home, name)?;
    let already_inside = env::var(ENV).is_ok_and(|active| active == name);
    for (variable, remembered) in XDG {
        // The host's own value is kept once, on the way in; a child started from inside the
        // instance carries it along rather than overwriting it with the instance's.
        if !already_inside {
            let host = env::var_os(variable).unwrap_or_default();
            unsafe { env::set_var(remembered, host) };
        }
    }
    unsafe { env::set_var(ENV, name) };
    for (variable, value) in instance.environment() {
        unsafe { env::set_var(variable, value) };
    }
    Ok(instance)
}

/// The host's value of an XDG variable, for the service manager. Outside an instance it is the
/// process's own.
pub fn host_xdg(variable: &str) -> Option<PathBuf> {
    let value = match XDG.iter().find(|(name, _)| *name == variable) {
        Some((_, remembered)) if current().is_some() => env::var_os(remembered),
        _ => env::var_os(variable),
    };
    value.filter(|value| !value.is_empty()).map(PathBuf::from)
}

/// The directory systemd reads this user's units from. Units for an instance sit beside the
/// default ones, because systemd looks nowhere else, and are named for the instance.
pub fn host_config_home() -> Option<PathBuf> {
    host_xdg("XDG_CONFIG_HOME")
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
}

/// Make a command talk to the real user session: the service manager, not the instance.
pub fn as_host(command: &mut Command) -> &mut Command {
    if current().is_none() {
        return command;
    }
    for (variable, _) in XDG {
        match host_xdg(variable) {
            Some(value) => command.env(variable, value),
            None => command.env_remove(variable),
        };
    }
    command
}

/// The environment a service definition carries so the daemon it starts runs in the instance.
/// Empty for the default install.
pub fn service_environment() -> Vec<(String, String)> {
    let Some(instance) = current() else {
        return Vec::new();
    };
    let mut variables = vec![(ENV.to_owned(), instance.name().to_owned())];
    for (variable, value) in instance.environment() {
        variables.push((variable.to_owned(), value.display().to_string()));
    }
    for (variable, remembered) in XDG {
        let host = host_xdg(variable).map(|path| path.display().to_string());
        variables.push((remembered.to_owned(), host.unwrap_or_default()));
    }
    variables
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(name: &str) -> Instance {
        Instance::new(Path::new("/home/example"), name).unwrap()
    }

    #[test]
    fn names_are_short_plain_and_safe_to_use_as_a_path_unit_and_label() {
        for good in ["a", "test", "try-2", "0", &"x".repeat(LONGEST_NAME)] {
            validate(good).unwrap();
        }
        for bad in [
            "",
            "Test",
            "a b",
            "a/b",
            "..",
            ".",
            "-a",
            "a-",
            "a--b",
            "a_b",
            "a.b",
            "é",
            &"x".repeat(LONGEST_NAME + 1),
        ] {
            assert!(validate(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn everything_an_instance_owns_is_below_its_one_root() {
        let one = instance("try");
        assert_eq!(one.root(), Path::new("/home/example/.st-instance/try"));
        for path in [
            one.state_home(),
            one.config_home(),
            one.data_home(),
            one.cache_home(),
            one.run_dir(),
            one.bin_dir(),
            one.claude_config_dir(),
            one.codex_home(),
            one.agents_dir(),
            one.app_dir(),
        ] {
            assert!(path.starts_with(one.root()), "{}", path.display());
            assert!(path.parent() == Some(one.root()), "{}", path.display());
        }
        for (variable, value) in one.environment() {
            assert!(value.starts_with(one.root()), "{variable}");
        }
    }

    #[test]
    fn two_instances_and_the_default_install_share_no_directory() {
        let (a, b) = (instance("a"), instance("b"));
        assert!(!a.root().starts_with(b.root()) && !b.root().starts_with(a.root()));
        let defaults = [
            "/home/example/.local/state/st3",
            "/home/example/.config/st3",
            "/home/example/.local/share/st3",
            "/home/example/.cache",
            "/home/example/.local/bin",
            "/home/example/.claude",
            "/home/example/.codex",
            "/home/example/st/agents",
        ];
        for path in defaults {
            for one in [&a, &b] {
                assert!(!Path::new(path).starts_with(one.root()), "{path}");
                assert!(!one.root().starts_with(path), "{path}");
            }
        }
    }

    #[test]
    fn service_names_and_labels_carry_the_name_and_never_equal_a_default() {
        let defaults = [
            "st3.service",
            "st3-replication.service",
            "com.compoundingtech.st3",
            "com.compoundingtech.st3.replication",
        ];
        // `replication` is the name most likely to collide by construction.
        for name in ["replication", "st3", "try"] {
            let one = instance(name);
            let names = [
                one.systemd_service(),
                one.systemd_replication_service(),
                one.launchd_label(),
                one.launchd_replication_label(),
            ];
            for generated in &names {
                assert!(generated.contains(name), "{generated}");
                assert!(!defaults.contains(&generated.as_str()), "{generated}");
            }
            let unique: std::collections::HashSet<_> = names.iter().collect();
            assert_eq!(unique.len(), names.len(), "{names:?}");
        }
        assert_ne!(instance("a").systemd_service(), instance("b").systemd_service());
    }

    #[test]
    fn the_socket_path_stays_within_the_unix_limit_for_the_longest_name() {
        let longest = Instance::new(Path::new("/home/example/a-very-long-directory-name-here"), &"x".repeat(LONGEST_NAME)).unwrap();
        let socket = longest.run_dir().join("st3-client.sock");
        assert!(socket.as_os_str().len() < 100, "{}", socket.display());
    }

    #[test]
    fn the_node_name_differs_from_the_host() {
        assert_eq!(instance("try").node_name("laptop"), "laptop-try");
    }

    #[test]
    fn the_command_line_names_the_instance_and_stops_at_the_program_boundary() {
        let arguments = |parts: &[&str]| parts.iter().map(Into::into).collect::<Vec<_>>();
        assert_eq!(requested(&arguments(&["st", "--instance", "a", "agents", "ls"])), Some("a".into()));
        assert_eq!(requested(&arguments(&["st", "agents", "--instance=b"])), Some("b".into()));
        assert_eq!(requested(&arguments(&["st", "--instance", "a", "--instance=c"])), Some("c".into()));
        assert_eq!(requested(&arguments(&["st", "sekrets", "--", "tool", "--instance", "x"])), None);
        assert_eq!(requested(&arguments(&["st", "--instance"])), None);
        assert_eq!(requested(&arguments(&["st", "ls"])), None);
    }
}
