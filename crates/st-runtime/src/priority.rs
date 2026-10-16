//! Keeps st's live path ahead of the work it hosts.
//!
//! The st daemon and each PTY server answer people: a command, an attach. The harness a PTY hosts
//! runs builds and tests. Each gets its own CPU and IO share, so a saturated host still answers.
//!
//! On Linux with a systemd user manager, `pty run` starts inside the transient scope st names for
//! the session, and the PTY server forks the harness there. st then moves the server alone into a
//! scope of its own at [`LIVE_WEIGHT`]. The harness and everything it starts stay behind at
//! systemd's default weight of 100. The daemon's unit carries [`LIVE_WEIGHT`] too.
//!
//! macOS has no cgroups. There the harness, and each exec task such as a gate's tests, starts
//! under `taskpolicy -b`, which throttles it and every child it starts, while the daemon and the PTY servers keep
//! their launchd agent's default QoS.
//!
//! Linux scope weights only divide a resource between cgroups, and need the user manager to
//! delegate the controller. A harness therefore also starts at [`BACKGROUND_NICE`] and IO class
//! best-effort [`BACKGROUND_IO_LEVEL`], which every child inherits and which a fresh user install
//! needs nothing for. [`SeatPriority::Normal`] drops both for a seat that should have the cores.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

use crate::PtyObservation;

/// The CPU and IO weight of the daemon and every PTY server. systemd's default is 100.
pub const LIVE_WEIGHT: u64 = 1000;

/// A PTY server's memory stays resident up to this much once the user manager itself has a
/// memory.low to pass down. It is a few times the resident size of a `pty` server.
const LIVE_MEMORY_LOW: u64 = 64 * 1024 * 1024;

const MOVE_TIMEOUT: Duration = Duration::from_secs(5);
const MACOS_TASKPOLICY: &str = "/usr/sbin/taskpolicy";

/// Where a PTY server runs after [`protect_server`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerPlacement {
    /// The server moved into this scope.
    Moved(String),
    /// The server was already in its own scope.
    Already,
    /// The server is not in the scope st started it in, so st leaves it where it is.
    Foreign,
    /// This host places PTY servers without systemd scopes.
    Unsupported,
}

/// The scope a PTY server runs in on its own. It names the server's pid, so one runtime's next
/// incarnation gets a new scope.
pub fn server_unit(runtime_id: &str, pid: u32) -> String {
    format!(
        "st3-pty-server-{}-{pid}.scope",
        crate::isolate::sanitize(runtime_id)
    )
}

/// The environment variable that carries a seat's [`SeatPriority`] into its launch. The daemon sets
/// it from the install's `seat_priority`; a seat that declares its own value keeps it.
pub const PRIORITY_ENV: &str = "ST_SEAT_PRIORITY";

/// The nice value a background seat starts at on Linux.
pub const BACKGROUND_NICE: u8 = 10;

/// The best-effort IO level (0 highest, 7 lowest) a background seat starts at on Linux.
pub const BACKGROUND_IO_LEVEL: u8 = 7;

/// How a seat's harness and everything it starts rank against the st daemon for CPU and IO.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SeatPriority {
    /// Below the daemon: nice 10 and best-effort IO level 7 on Linux, `taskpolicy -b` on macOS.
    #[default]
    Background,
    /// The daemon's own priority.
    Normal,
}

impl SeatPriority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Background => "background",
            Self::Normal => "normal",
        }
    }

    /// The value a launch's environment names. An unset or unrecognized value reads as
    /// [`Self::Background`], so a typo never gives a seat more CPU than the default.
    pub fn from_environment(environment: &BTreeMap<String, String>) -> Self {
        environment
            .get(PRIORITY_ENV)
            .and_then(|value| value.parse().ok())
            .unwrap_or_default()
    }
}

impl std::str::FromStr for SeatPriority {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "background" => Ok(Self::Background),
            "normal" => Ok(Self::Normal),
            _ => anyhow::bail!("a seat priority must be `background` or `normal`, not `{value}`"),
        }
    }
}

/// The program and arguments that start a harness below the live path. A PTY launch runs them
/// before its own argv. `environment` is the launch's: it names the seat's [`SeatPriority`] and the
/// `PATH` that finds `nice` and `ionice`. On Linux the PTY server moves out of the harness's scope
/// as well, which this prefix does not replace.
pub fn work_prefix(environment: &BTreeMap<String, String>) -> Vec<String> {
    let find = |program: &str| crate::resolve_executable(program, environment).ok();
    work_prefix_for(
        SeatPriority::from_environment(environment),
        cfg!(target_os = "macos"),
        Path::new(MACOS_TASKPOLICY).is_file(),
        find("nice"),
        find("ionice"),
    )
}

fn work_prefix_for(
    priority: SeatPriority,
    macos: bool,
    taskpolicy: bool,
    nice: Option<PathBuf>,
    ionice: Option<PathBuf>,
) -> Vec<String> {
    if priority == SeatPriority::Normal {
        return Vec::new();
    }
    if macos {
        return if taskpolicy {
            vec![MACOS_TASKPOLICY.into(), "-b".into()]
        } else {
            Vec::new()
        };
    }
    let path = |program: PathBuf| program.to_string_lossy().into_owned();
    let mut prefix = Vec::new();
    if let Some(nice) = nice {
        prefix.extend([path(nice), "-n".into(), BACKGROUND_NICE.to_string()]);
    }
    if let Some(ionice) = ionice {
        prefix.extend([
            path(ionice),
            "-c".into(),
            "2".into(),
            "-n".into(),
            BACKGROUND_IO_LEVEL.to_string(),
        ]);
    }
    prefix
}

/// Moves the PTY server `pid` of `runtime_id` out of `work_unit`, the scope st started it in, into
/// [`server_unit`] at [`LIVE_WEIGHT`]. A process in any other scope, st's own included, stays.
pub fn protect_server(runtime_id: &str, pid: u32, work_unit: &str) -> Result<ServerPlacement> {
    if crate::isolation_mode() != crate::Isolation::Scope {
        return Ok(ServerPlacement::Unsupported);
    }
    if pid == std::process::id() {
        return Ok(ServerPlacement::Foreign);
    }
    let unit = server_unit(runtime_id, pid);
    let cgroup = process_cgroup(pid)?;
    let (slice, leaf) = slice_and_leaf(&cgroup);
    if leaf == unit {
        return Ok(ServerPlacement::Already);
    }
    if leaf != work_unit {
        return Ok(ServerPlacement::Foreign);
    }
    let output =
        crate::pty::output_within(move_command(&unit, pid, slice, runtime_id), MOVE_TIMEOUT)
            .context("ask the systemd user manager for a PTY server scope")?;
    anyhow::ensure!(
        output.status.success(),
        "the systemd user manager refused scope {unit}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    // StartTransientUnit returns once the start job is queued. The job moves the process.
    let deadline = Instant::now() + MOVE_TIMEOUT;
    loop {
        if slice_and_leaf(&process_cgroup(pid)?).1 == unit {
            return Ok(ServerPlacement::Moved(unit));
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the systemd user manager did not move PTY server {pid} into {unit} within {}s",
            MOVE_TIMEOUT.as_secs()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether the PTY server `pid` of `runtime_id` runs outside `work_unit`, moving it out first, so
/// that ending `work_unit` leaves the server. A move [`protect_servers`] has under way counts once
/// it lands.
pub(crate) fn keep_server_apart(runtime_id: &str, pid: u32, work_unit: &str) -> bool {
    match protect_server(runtime_id, pid, work_unit) {
        Ok(ServerPlacement::Moved(_) | ServerPlacement::Already | ServerPlacement::Foreign) => true,
        Ok(ServerPlacement::Unsupported) => false,
        Err(_) => {
            // The other mover's transient unit already exists, so this one was refused.
            let deadline = Instant::now() + MOVE_TIMEOUT;
            loop {
                let Ok(cgroup) = process_cgroup(pid) else {
                    // The server has exited.
                    return true;
                };
                if slice_and_leaf(&cgroup).1 != work_unit {
                    return true;
                }
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn move_command(unit: &str, pid: u32, slice: Option<&str>, runtime_id: &str) -> Command {
    let mut command = Command::new("busctl");
    command.args([
        "--user",
        "--timeout=5",
        "call",
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
        "StartTransientUnit",
        "ssa(sv)a(sa(sv))",
        unit,
        "fail",
    ]);
    let weight = LIVE_WEIGHT.to_string();
    let memory_low = LIVE_MEMORY_LOW.to_string();
    let description = format!("st PTY server for {runtime_id}");
    let pid = pid.to_string();
    let mut properties: Vec<[&str; 3]> = vec![
        ["Description", "s", &description],
        ["CPUWeight", "t", &weight],
        ["IOWeight", "t", &weight],
        ["MemoryLow", "t", &memory_low],
    ];
    if let Some(slice) = slice {
        properties.push(["Slice", "s", slice]);
    }
    command.arg((properties.len() + 1).to_string());
    for property in properties {
        command.args(property);
    }
    command.args(["PIDs", "au", "1", &pid, "0"]);
    command.stdin(Stdio::null());
    command
}

fn process_cgroup(pid: u32) -> Result<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .with_context(|| format!("read the cgroup of PTY server {pid}"))?;
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::to_owned)
        .with_context(|| format!("PTY server {pid} is not in a cgroup v2 hierarchy"))
}

/// The enclosing slice, when the parent is one, and the unit a cgroup path ends in.
fn slice_and_leaf(cgroup: &str) -> (Option<&str>, &str) {
    let mut parts = cgroup.rsplit('/');
    let leaf = parts.next().unwrap_or_default();
    let slice = parts.next().filter(|parent| parent.ends_with(".slice"));
    (slice, leaf)
}

static SETTLED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
static MOVING: AtomicBool = AtomicBool::new(false);

/// Moves every running st PTY server in `observations` that still shares its harness's scope, off
/// the caller's thread, once per server incarnation. A server started before this release moves
/// the first time the daemon observes it.
pub fn protect_servers(observations: &[PtyObservation]) {
    if crate::isolation_mode() != crate::Isolation::Scope {
        return;
    }
    let pending = {
        let mut settled = SETTLED.lock().unwrap_or_else(|poison| poison.into_inner());
        // A full set only costs one cgroup read per server to rebuild.
        if settled.len() > 4096 {
            settled.clear();
        }
        observations
            .iter()
            .filter(|observation| observation.status == "running")
            .filter_map(|observation| {
                let pid = observation.pid?;
                let key = format!("{pid}:{}", observation.created_at.as_deref()?);
                let work_unit = observation.tags.get("st3.scope-unit")?.clone();
                (!settled.contains(&key)).then(|| (key, observation.name.clone(), pid, work_unit))
            })
            .collect::<Vec<_>>()
    };
    if pending.is_empty() || MOVING.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(move || {
        for (key, runtime_id, pid, work_unit) in pending {
            match protect_server(&runtime_id, pid, &work_unit) {
                Ok(ServerPlacement::Moved(unit)) => {
                    eprintln!("st3: PTY server {pid} of {runtime_id} runs in {unit}");
                }
                Ok(_) => {}
                Err(error) => eprintln!(
                    "st3: WARN PTY server {pid} of {runtime_id} still shares its harness's scope: {error:#}"
                ),
            }
            SETTLED
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(key);
        }
        MOVING.store(false, Ordering::Release);
    });
}

static CONFIGURED: AtomicU8 = AtomicU8::new(0);

/// Records the install's `seat_priority` for [`report`]. The daemon calls it once at start, from the
/// same value it hands each launch through [`PRIORITY_ENV`].
pub fn set_configured(priority: SeatPriority) {
    CONFIGURED.store(priority as u8, Ordering::Relaxed);
}

fn configured() -> SeatPriority {
    match CONFIGURED.load(Ordering::Relaxed) {
        x if x == SeatPriority::Normal as u8 => SeatPriority::Normal,
        _ => SeatPriority::Background,
    }
}

/// How this host places the live path, for `st doctor`: a status and one line a person can act on.
pub fn report(observations: &[PtyObservation]) -> (&'static str, String) {
    let (status, message) = placement_report(observations);
    let seats = match configured() {
        SeatPriority::Background => {
            // The launch resolves these through the login shell's PATH; the daemon's own PATH is
            // the closest check this process can make.
            let missing = if cfg!(target_os = "macos") {
                (!Path::new(MACOS_TASKPOLICY).is_file()).then_some("taskpolicy")
            } else {
                let on_path = |program: &str| {
                    std::env::var_os("PATH").is_some_and(|path| {
                        std::env::split_paths(&path)
                            .any(|directory| directory.join(program).is_file())
                    })
                };
                (!on_path("nice")).then_some("nice")
            };
            match missing {
                None => {
                    "seats start below the daemon (`seat_priority = \"background\"`)".to_string()
                }
                Some(program) => {
                    return (
                        "warn",
                        format!(
                            "{message}; `seat_priority = \"background\"` but `{program}` is not found, so seats start at the daemon's priority"
                        ),
                    );
                }
            }
        }
        SeatPriority::Normal => {
            "seats start at the daemon's priority (`seat_priority = \"normal\"`)".to_string()
        }
    };
    (status, format!("{message}; {seats}"))
}

fn placement_report(observations: &[PtyObservation]) -> (&'static str, String) {
    match crate::isolation_mode() {
        crate::Isolation::Scope => linux_report(observations),
        crate::Isolation::Detached if cfg!(target_os = "macos") => {
            if configured() == SeatPriority::Background && !Path::new(MACOS_TASKPOLICY).is_file() {
                (
                    "warn",
                    format!("{MACOS_TASKPOLICY} is missing, so harnesses run at the daemon's QoS"),
                )
            } else {
                (
                    "pass",
                    "the daemon and PTY servers keep the default QoS; background throttling applies to harnesses only in the background mode".into(),
                )
            }
        }
        crate::Isolation::Detached | crate::Isolation::DegradedDetached => (
            "warn",
            "without systemd user scopes, PTY servers share CPU and IO with their harnesses".into(),
        ),
    }
}

fn linux_report(observations: &[PtyObservation]) -> (&'static str, String) {
    let mut problems = Vec::new();
    let own = process_cgroup(std::process::id()).unwrap_or_default();
    let own_directory = Path::new("/sys/fs/cgroup").join(own.trim_start_matches('/'));
    let (_, own_unit) = slice_and_leaf(&own);
    if !own_unit.ends_with(".service") {
        problems.push(format!(
            "the daemon runs in {own_unit}, outside a service unit; `st service install` runs it at weight {LIVE_WEIGHT}"
        ));
    }
    let weight = |file: &str| {
        std::fs::read_to_string(own_directory.join(file))
            .ok()
            .and_then(|text| text.split_whitespace().last()?.parse::<u64>().ok())
    };
    let (cpu, io) = (weight("cpu.weight"), weight("io.weight"));
    if own_unit.ends_with(".service")
        && (cpu.is_some_and(|cpu| cpu < LIVE_WEIGHT) || io.is_some_and(|io| io < LIVE_WEIGHT))
    {
        problems.push(format!(
            "the daemon runs at CPU weight {} and IO weight {}; run `systemctl --user set-property {own_unit} CPUWeight={LIVE_WEIGHT} IOWeight={LIVE_WEIGHT}`",
            cpu.map_or("unknown".into(), |value| value.to_string()),
            io.map_or("unknown".into(), |value| value.to_string()),
        ));
    }
    if let Some(manager) = own
        .split('/')
        .find(|part| part.starts_with("user@") && part.ends_with(".service"))
    {
        let manager_directory = own_directory
            .ancestors()
            .find(|path| path.file_name().is_some_and(|name| name == manager))
            .map(Path::to_path_buf);
        let delegated = manager_directory
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path.join("cgroup.subtree_control")).ok())
            .unwrap_or_default();
        if !delegated
            .split_whitespace()
            .any(|controller| controller == "io")
        {
            problems.push(format!(
                "{manager} does not delegate the io controller, so no IO weight applies; as root, add `Delegate=cpu io memory pids` under [Service] in /etc/systemd/system/user@.service.d/60-delegate-io.conf, run `systemctl daemon-reload`, then restart {manager} or log in again"
            ));
        }
    }
    let running = observations
        .iter()
        .filter(|observation| {
            observation.status == "running" && observation.tags.contains_key("st3.scope-unit")
        })
        .filter_map(|observation| Some((observation.name.as_str(), observation.pid?)))
        .collect::<Vec<_>>();
    let shared = running
        .iter()
        .filter(|(runtime_id, pid)| {
            process_cgroup(*pid)
                .is_ok_and(|cgroup| slice_and_leaf(&cgroup).1 != server_unit(runtime_id, *pid))
        })
        .count();
    if shared > 0 {
        problems.push(format!(
            "{shared} of {} PTY servers still share their harness's scope; the daemon moves each one the next time it observes it",
            running.len()
        ));
    }
    if problems.is_empty() {
        (
            "pass",
            format!(
                "the daemon and {} PTY servers run at CPU and IO weight {LIVE_WEIGHT}; their harnesses at the default 100",
                running.len()
            ),
        )
    } else {
        ("warn", problems.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_scope_names_the_runtime_and_the_server_pid() {
        assert_eq!(
            server_unit("fleet/demo worker", 42),
            "st3-pty-server-fleet_demo_worker-42.scope"
        );
    }

    fn tool(name: &str) -> Option<PathBuf> {
        Some(PathBuf::from("/bin").join(name))
    }

    #[test]
    fn a_background_seat_on_macos_needs_taskpolicy() {
        assert_eq!(
            work_prefix_for(SeatPriority::Background, true, true, None, None),
            ["/usr/sbin/taskpolicy", "-b"]
        );
        assert!(
            work_prefix_for(
                SeatPriority::Background,
                true,
                false,
                tool("nice"),
                tool("ionice")
            )
            .is_empty()
        );
    }

    #[test]
    fn a_background_seat_on_linux_runs_under_nice_and_ionice() {
        assert_eq!(
            work_prefix_for(
                SeatPriority::Background,
                false,
                true,
                tool("nice"),
                tool("ionice")
            ),
            ["/bin/nice", "-n", "10", "/bin/ionice", "-c", "2", "-n", "7"]
        );
        assert_eq!(
            work_prefix_for(SeatPriority::Background, false, false, tool("nice"), None),
            ["/bin/nice", "-n", "10"]
        );
        assert_eq!(
            work_prefix_for(SeatPriority::Background, false, false, None, tool("ionice")),
            ["/bin/ionice", "-c", "2", "-n", "7"]
        );
        assert!(work_prefix_for(SeatPriority::Background, false, false, None, None).is_empty());
    }

    #[test]
    fn a_normal_seat_starts_without_a_prefix() {
        for macos in [false, true] {
            assert!(
                work_prefix_for(
                    SeatPriority::Normal,
                    macos,
                    true,
                    tool("nice"),
                    tool("ionice")
                )
                .is_empty()
            );
        }
    }

    #[test]
    fn the_launch_environment_picks_the_priority() {
        let with = |value: Option<&str>| {
            SeatPriority::from_environment(
                &value
                    .map(|value| BTreeMap::from([(PRIORITY_ENV.to_string(), value.to_string())]))
                    .unwrap_or_default(),
            )
        };
        assert_eq!(with(None), SeatPriority::Background);
        assert_eq!(with(Some("background")), SeatPriority::Background);
        assert_eq!(with(Some("normal")), SeatPriority::Normal);
        assert_eq!(with(Some("high")), SeatPriority::Background);
        assert!("high".parse::<SeatPriority>().is_err());
    }

    #[test]
    fn a_cgroup_path_names_its_slice_and_unit() {
        assert_eq!(
            slice_and_leaf("/user.slice/user-1000.slice/user@1000.service/app.slice/st3-w-1.scope"),
            (Some("app.slice"), "st3-w-1.scope")
        );
        assert_eq!(
            slice_and_leaf("/user.slice/user-1000.slice/user@1000.service/init.scope"),
            (None, "init.scope")
        );
    }

    #[test]
    fn the_move_asks_for_live_weights_in_the_same_slice() {
        let command = move_command("st3-pty-server-w-7.scope", 7, Some("app.slice"), "w");
        let arguments = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let tail = arguments[arguments.len() - 5..].join(" ");
        assert_eq!(tail, "PIDs au 1 7 0");
        let joined = arguments.join(" ");
        assert!(joined.contains("st3-pty-server-w-7.scope fail 6 "));
        assert!(joined.contains("CPUWeight t 1000"));
        assert!(joined.contains("IOWeight t 1000"));
        assert!(joined.contains("Slice s app.slice"));
    }

    #[test]
    fn a_process_outside_the_sessions_scope_stays_where_it_is() {
        // This test process never runs in a scope st named for a PTY session.
        let placement = protect_server("work", std::process::id(), "st3-work-1-0.scope").unwrap();
        assert!(matches!(
            placement,
            ServerPlacement::Foreign | ServerPlacement::Unsupported
        ));
        let child = Command::new("sleep").arg("5").spawn().unwrap();
        let placement = protect_server("work", child.id(), "st3-work-1-0.scope").unwrap();
        assert!(matches!(
            placement,
            ServerPlacement::Foreign | ServerPlacement::Unsupported
        ));
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
    }
}
