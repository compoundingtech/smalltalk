//! A CPU and disk hog at the priority a seat's work gets by default, for the load test to run
//! beside the daemon.
//!
//! On Linux a seat's work runs at nice 10 and best-effort IO priority 7; on macOS under
//! `taskpolicy -b` (background QoS). The hog is shell loops: some burn a core each, some write and
//! sync a file over and over. [`Hog::start`] starts them at that priority; dropping the hog kills
//! them and removes the file.
//!
//! The test process may itself run niced (cargo's tests run in `builds.slice`, whose threads a
//! host timer renices to 10), and a hog no lower than the daemon would not stand for a seat's
//! work. The hog's nice is therefore [`SEAT_NICE`] above the test process's own, up to 19, and the
//! report names both.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How much lower than the daemon a seat's work runs, in nice levels.
pub const SEAT_NICE: i32 = 10;

/// Linux best-effort IO priority level of a seat's work: 7 is the lowest best-effort level.
#[cfg(target_os = "linux")]
pub const SEAT_IO_LEVEL: i32 = 7;

/// The hog's nice for a process running at `own`: [`SEAT_NICE`] lower than it, at most 19.
pub fn hog_nice(own: i32) -> i32 {
    (own + SEAT_NICE).min(19)
}

/// This process's nice.
pub fn own_nice() -> i32 {
    // SAFETY: getpriority reads this process's nice; -1 is a valid value, so errno is cleared first.
    unsafe {
        *errno_location() = 0;
        libc::getpriority(libc::PRIO_PROCESS, 0)
    }
}

#[cfg(target_os = "linux")]
unsafe fn errno_location() -> *mut i32 {
    unsafe { libc::__errno_location() }
}

#[cfg(target_os = "macos")]
unsafe fn errno_location() -> *mut i32 {
    unsafe { libc::__error() }
}

/// The 1-minute load average.
pub fn load_average() -> f64 {
    let mut averages = [0.0_f64; 3];
    // SAFETY: getloadavg fills at most the three doubles it is given.
    let read = unsafe { libc::getloadavg(averages.as_mut_ptr(), 3) };
    if read < 1 { 0.0 } else { averages[0] }
}

/// The host's online CPUs, which the load average counts against. Not `available_parallelism`,
/// which a cgroup limit on the test's own slice can lower below the host's.
pub fn cores() -> usize {
    // SAFETY: sysconf has no preconditions.
    let online = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    usize::try_from(online).ok().filter(|cores| *cores > 0).unwrap_or(1)
}

/// How many processes of each kind to start.
#[derive(Clone, Copy, Debug)]
pub struct Shape {
    pub cpu: usize,
    pub disk: usize,
}

impl Shape {
    /// Twice the cores in CPU burners, so the run queue stays past the core count whatever else
    /// the host runs, and two disk writers.
    pub fn for_host() -> Self {
        Self { cpu: cores() * 2, disk: 2 }
    }
}

pub struct Hog {
    children: Vec<Child>,
    dir: PathBuf,
    pub nice: i32,
    pub shape: Shape,
}

impl Hog {
    /// Start the hog in `dir`, which holds the files the disk writers sync.
    pub fn start(dir: PathBuf, shape: Shape) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let nice = hog_nice(own_nice());
        let mut hog = Self { children: Vec::new(), dir, nice, shape };
        for _ in 0..shape.cpu {
            let child = hog.spawn("while :; do :; done", &[])?;
            hog.children.push(child);
        }
        for index in 0..shape.disk {
            let file = hog.dir.join(format!("hog-{index}.bin"));
            let child = hog.spawn(DISK_LOOP, &[file.to_string_lossy().into_owned()])?;
            hog.children.push(child);
        }
        Ok(hog)
    }

    fn spawn(&self, script: &str, args: &[String]) -> std::io::Result<Child> {
        let mut command = if cfg!(target_os = "macos") {
            let mut command = Command::new("/usr/sbin/taskpolicy");
            command.args(["-b", "/bin/sh", "-c", script, "hog"]);
            command
        } else {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", script, "hog"]);
            command
        };
        command.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        // Its own process group, so one signal ends the loop and the `dd` it is running.
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let nice = self.nice;
        // SAFETY: the closure only makes async-signal-safe syscalls between fork and exec.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut command, move || {
                libc::setpriority(libc::PRIO_PROCESS, 0, nice);
                #[cfg(target_os = "linux")]
                libc::syscall(
                    libc::SYS_ioprio_set,
                    1, // IOPRIO_WHO_PROCESS
                    0,
                    (2 << 13) | SEAT_IO_LEVEL, // IOPRIO_CLASS_BE
                );
                Ok(())
            });
        }
        command.spawn()
    }

    /// The processes still running.
    pub fn running(&mut self) -> usize {
        self.children.iter_mut().map(Child::try_wait).filter(|state| matches!(state, Ok(None))).count()
    }

    /// Wait until the 1-minute load average is past the core count, and return it.
    pub fn ramp(&mut self, timeout: Duration) -> Result<f64, String> {
        let started = Instant::now();
        let cores = cores() as f64;
        loop {
            let load = load_average();
            if load > cores {
                return Ok(load);
            }
            if started.elapsed() > timeout {
                return Err(format!(
                    "the load average {load:.1} stayed at or under the {cores} cores for {}s",
                    timeout.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    /// The pids the hog started, for checking their priority.
    pub fn pids(&self) -> Vec<u32> {
        self.children.iter().map(Child::id).collect()
    }
}

#[cfg(target_os = "linux")]
const DISK_LOOP: &str =
    r#"while :; do dd if=/dev/zero of="$1" bs=1048576 count=128 conv=fsync 2>/dev/null; done"#;
#[cfg(not(target_os = "linux"))]
const DISK_LOOP: &str =
    r#"while :; do dd if=/dev/zero of="$1" bs=1048576 count=128 2>/dev/null; sync; done"#;

impl Drop for Hog {
    fn drop(&mut self) {
        for child in &mut self.children {
            // SAFETY: the child leads its own group; a negative pid signals the whole group.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn the_hog_runs_lower_than_the_test_by_a_seats_margin() {
    assert_eq!(hog_nice(0), 10);
    assert_eq!(hog_nice(10), 19);
    assert_eq!(hog_nice(15), 19);
}

#[test]
fn the_hog_runs_at_seat_priority_and_stops_with_its_drop() {
    let dir = tempfile::tempdir().unwrap();
    let mut hog = Hog::start(dir.path().join("hog"), Shape { cpu: 1, disk: 1 }).unwrap();
    assert_eq!(hog.running(), 2);
    // The priority is applied before exec, so it holds as soon as the process exists.
    for pid in hog.pids() {
        // SAFETY: getpriority reads another process's nice.
        let nice = unsafe {
            *errno_location() = 0;
            libc::getpriority(libc::PRIO_PROCESS, pid)
        };
        // macOS runs the hog under `taskpolicy -b`, whose background state can read back as a
        // nice above the one set (its limit is 20, not 19) when the test itself runs niced.
        if cfg!(target_os = "macos") {
            assert!(nice >= hog.nice, "pid {pid} should run at or below the hog's nice {}", hog.nice);
        } else {
            assert_eq!(nice, hog.nice, "pid {pid} should run at the hog's nice");
        }
        #[cfg(target_os = "linux")]
        {
            // SAFETY: ioprio_get reads another process's IO priority.
            let io = unsafe { libc::syscall(libc::SYS_ioprio_get, 1, pid) };
            assert_eq!(io >> 13, 2, "pid {pid} should be best-effort");
            assert_eq!(io & 0x1fff, SEAT_IO_LEVEL as i64, "pid {pid} should be at the lowest level");
        }
    }
    let pids = hog.pids();
    drop(hog);
    for pid in pids {
        // SAFETY: signal 0 only checks the process exists; a reaped child has gone.
        assert_ne!(unsafe { libc::kill(pid as i32, 0) }, 0, "pid {pid} should have ended");
    }
    assert!(!dir.path().join("hog").exists());
}
