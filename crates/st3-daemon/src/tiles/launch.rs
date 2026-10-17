//! Launching a tile: root starts a systemd unit in the tile's slice, and the unit's process
//! builds the tile's namespaces, drops to the tile's Unix user and runs the command.
//!
//! `st tiles run` (root) starts `st-tile-NAME-tools.service` in `st_tiles-NAME.slice` with the
//! tile's resource ceilings. systemd owns the cgroup: a tile cannot write to it, stopping the
//! unit ends every process in it, and so does the unit's main process ending for any reason.
//!
//! `st tiles enter` is that main process. Still root, it checks it runs in the tile's unit,
//! makes new mount, PID, IPC, network, UTS and cgroup namespaces, and forks the tile's first
//! process, PID 1 of the new PID namespace. PID 1 builds the tile's file system from an empty
//! tmpfs: the registry's read-only paths, the tile's own home and workspace, a private /tmp, a
//! fresh /proc, a few devices and the tile's endpoint directory, nothing else of the host. It
//! closes every descriptor but standard input, output and error, drops to the tile's user with
//! no capabilities, sets no-new-privileges and installs the seccomp filter, then forks the
//! command and reaps until it exits. When PID 1 exits, the kernel ends every process left in
//! the namespace; when `enter` dies, PID 1 is killed with it.

use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};

use crate::tile_registry::{self as registry, Registry, Tile};
use super::uids;

/// Where a tile's own directories appear inside it.
pub const TILE_HOME: &str = "/tile/home";
pub const TILE_WORK: &str = "/tile/work";
/// The tile's endpoint directory: the only host directory a tile can reach a socket in.
pub const TILE_ENDPOINT: &str = "/run/st";
/// Where a tile reaches the sekrets gateway, when the registry names one.
pub const TILE_SEKRETS_SOCKET: &str = crate::sekrets::gateway::DEFAULT_SOCKET;
/// The tile's user name inside the tile.
const TILE_USER: &str = "tile";

/// What `st tiles run` asks systemd for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunPlan {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

/// Device nodes a tile gets, bound from the host's.
const DEVICES: &[&str] = &["null", "zero", "full", "random", "urandom", "tty"];

/// The `systemd-run` invocation that starts a tile's tools unit.
pub fn run_plan(
    registry_path: &Path,
    name: &str,
    tile: &Tile,
    st: &Path,
    terminal: bool,
    command: &[OsString],
) -> RunPlan {
    let mut args: Vec<OsString> = vec![
        format!("--unit={}", registry::tools_unit(name)).into(),
        format!("--slice={}", registry::slice_unit(name)).into(),
        format!("--description=st tile {name}").into(),
        "--service-type=exec".into(),
        "--collect".into(),
        "--wait".into(),
        "--quiet".into(),
        "--no-ask-password".into(),
        "--expand-environment=no".into(),
        if terminal { "--pty" } else { "--pipe" }.into(),
        "--property=KillMode=control-group".into(),
        "--property=SendSIGKILL=yes".into(),
        "--property=OOMPolicy=kill".into(),
        format!("--property=TasksMax={}", tile.tasks_max).into(),
        format!("--property=MemoryMax={}", tile.memory_max).into(),
        format!("--property=CPUWeight={}", tile.cpu_weight).into(),
    ];
    if let Some(high) = &tile.memory_high {
        args.push(format!("--property=MemoryHigh={high}").into());
    }
    args.push("--".into());
    args.push(st.into());
    args.push("tiles".into());
    args.push("enter".into());
    args.push("--registry".into());
    args.push(registry_path.into());
    args.push(name.into());
    args.push("--".into());
    args.extend(command.iter().cloned());
    RunPlan {
        program: "systemd-run".into(),
        args,
    }
}

/// `st tiles run`: start the tile's tools unit with `command` and wait for it.
pub fn run(registry_path: &Path, name: &str, terminal: bool, command: &[OsString]) -> Result<i32> {
    require_root("st tiles run")?;
    let mut registry = Registry::load(registry_path)?;
    let tile = registry.tile(name)?.clone();
    root_dir(&registry.state_root, 0o755)?;
    uids::allocate(&mut registry, name)?;
    let st = std::env::current_exe().context("find this st executable")?;
    registry::root_controlled(&st).with_context(|| {
        format!(
            "{} runs as root in the tile's unit, so root must control it; install st where only \
             root can change it",
            st.display()
        )
    })?;
    if command.is_empty() {
        bail!("name a command to run in the tile, after `--`");
    }
    let plan = run_plan(registry_path, name, &tile, &st, terminal, command);
    let status = Command::new(&plan.program)
        .args(&plan.args)
        .env_clear()
        .env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin")
        .status()
        .context("start systemd-run")?;
    Ok(status.code().unwrap_or(1))
}

/// `st tiles prepare`: make the tile's directories without starting it, so its supervisor can
/// serve the endpoint before the first launch.
pub fn prepare(registry_path: &Path, name: &str) -> Result<()> {
    require_root("st tiles prepare")?;
    let mut registry = Registry::load(registry_path)?;
    let tile = registry.tile(name)?.clone();
    root_dir(&registry.state_root, 0o755)?;
    let uid = uids::allocate(&mut registry, name)?;
    prepare_directories(&registry, name, &tile, uid).map(|_| ())
}

/// `st tiles remove`: end the tile, remove its state and runtime directories, and give its user
/// ID back. Root's registry entry stays until root removes it; a later launch allocates afresh,
/// with nothing of the old tile left to inherit.
pub fn remove(registry_path: &Path, name: &str) -> Result<()> {
    require_root("st tiles remove")?;
    registry::check_name(name)?;
    let mut registry = Registry::load(registry_path)?;
    // Stopping a unit that is not loaded fails; what matters is that none is left.
    let _ = Command::new("systemctl")
        .args([
            "stop",
            &registry::tools_unit(name),
            &registry::slice_unit(name),
        ])
        .status()
        .context("run systemctl stop")?;
    let slice =
        Path::new("/sys/fs/cgroup").join(registry::slice_cgroup(name).trim_start_matches('/'));
    if slice.exists() {
        bail!(
            "{} is still running after systemctl stop; its user ID stays allocated",
            registry::slice_unit(name)
        );
    }
    if fs::symlink_metadata(&registry.state_root).is_err() {
        return Ok(());
    }
    // What is removed as root must be root's to remove.
    registry::root_controlled(&registry.state_root)?;
    if fs::symlink_metadata(&registry.runtime_root).is_ok() {
        registry::root_controlled(&registry.runtime_root)?;
    }
    uids::release(&mut registry, name, |registry| {
        for path in [registry.tile_runtime(name), registry.tile_state(name)] {
            match fs::remove_dir_all(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("remove {}", path.display()));
                }
            }
        }
        Ok(())
    })
}

/// `st tiles stop`: end every process of the tile's tools unit.
pub fn stop(name: &str) -> Result<()> {
    require_root("st tiles stop")?;
    registry::check_name(name)?;
    let status = Command::new("systemctl")
        .args([
            "stop",
            &registry::tools_unit(name),
            &registry::slice_unit(name),
        ])
        .status()
        .context("run systemctl stop")?;
    if !status.success() {
        bail!("systemctl stop exited with {status}");
    }
    Ok(())
}

fn require_root(what: &str) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        bail!("{what} runs as root: it creates the tile's namespaces and drops to its user");
    }
    Ok(())
}

/// `st tiles enter`: build the tile and run `command` in it. Runs as the main process of the
/// tile's tools unit and never returns on success paths it controls; its exit status is the
/// command's.
pub fn enter(registry_path: &Path, name: &str, command: &[OsString]) -> Result<i32> {
    require_root("st tiles enter")?;
    let registry = Registry::load(registry_path)?;
    let tile = registry.tile(name)?.clone();
    // `st tiles run` allocated it; `enter` never allocates, so it runs as no ID root's launcher
    // did not record.
    let uid = registry
        .uid(name)
        .with_context(|| format!("tile {name} has no user ID yet; start it with `st tiles run`"))?;
    uids::check_unclaimed(name, uid)?;
    if command.is_empty() {
        bail!("name a command to run in the tile");
    }
    let expected = registry::tools_cgroup(name);
    // SAFETY: getpid has no preconditions.
    let cgroup = crate::sekrets::identity::process_cgroup(unsafe { libc::getpid() })
        .context("read this process's cgroup")?;
    if cgroup != expected {
        bail!(
            "st tiles enter runs only as the main process of {}, in {expected}; this process runs \
             in {cgroup}. Start the tile with `st tiles run`",
            registry::tools_unit(name)
        );
    }
    single_threaded()?;
    let dirs = prepare_directories(&registry, name, &tile, uid)?;
    let setup = Setup::new(&registry, name, &tile, uid, &dirs, command)?;

    let namespaces = libc::CLONE_NEWNS
        | libc::CLONE_NEWPID
        | libc::CLONE_NEWIPC
        | libc::CLONE_NEWNET
        | libc::CLONE_NEWUTS
        | libc::CLONE_NEWCGROUP;
    // SAFETY: this process is single-threaded, as checked above.
    check(unsafe { libc::unshare(namespaces) }).context("create the tile's namespaces")?;
    // SAFETY: single-threaded; the child only runs `Setup::init`, which uses prepared data.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(io::Error::last_os_error()).context("fork the tile's first process");
    }
    if child == 0 {
        let code = match setup.init() {
            Ok(code) => code,
            Err(error) => {
                eprintln!("st tiles: {name}: {error:#}");
                125
            }
        };
        // SAFETY: _exit ends this child without running the parent's destructors twice.
        unsafe { libc::_exit(code) };
    }
    wait_exit(child)
}

fn single_threaded() -> Result<()> {
    let threads = fs::read_dir("/proc/self/task")
        .context("count this process's threads")?
        .count();
    if threads != 1 {
        bail!("st tiles enter must start single-threaded, but has {threads} threads");
    }
    Ok(())
}

/// The host directories a tile is built from.
struct Directories {
    home: PathBuf,
    work: PathBuf,
    endpoint: PathBuf,
    root: PathBuf,
}

/// Make the tile's directories: its state, owned by the tile and private to it, and its
/// runtime directories, owned by root except the endpoint directory, which its supervisor owns.
fn prepare_directories(
    registry: &Registry,
    name: &str,
    tile: &Tile,
    uid: u32,
) -> Result<Directories> {
    root_dir(&registry.state_root, 0o755)?;
    let state = registry.tile_state(name);
    root_dir(&state, 0o700)?;
    let home = tile_dir(&state.join("home"), uid)?;
    let work = tile_dir(&state.join("work"), uid)?;
    root_dir(&registry.runtime_root, 0o755)?;
    let runtime = registry.tile_runtime(name);
    root_dir(&runtime, 0o755)?;
    let endpoint = registry.endpoint_dir(name);
    owned_dir(&endpoint, tile.supervisor_uid.unwrap_or(0), 0, 0o755)?;
    let root = runtime.join("root");
    root_dir(&root, 0o700)?;
    Ok(Directories {
        home,
        work,
        endpoint,
        root,
    })
}

fn root_dir(path: &Path, mode: u32) -> Result<()> {
    owned_dir(path, 0, 0, mode)
}

/// A tile's own directory; its group ID is its user ID.
fn tile_dir(path: &Path, uid: u32) -> Result<PathBuf> {
    owned_dir(path, uid, uid, 0o700)?;
    Ok(path.to_owned())
}

/// Make `path` a real directory owned by `uid:gid` with `mode`, or refuse one that is not a
/// directory or belongs to someone else, such as a user that once had this tile's number. The
/// directory is opened without following a symbolic link and changed through that descriptor,
/// so what is changed is what was checked, wherever the directory lies.
fn owned_dir(path: &Path, uid: u32, gid: u32, mode: u32) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let created = match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
    };
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("{} is not a directory; remove it", path.display()))?;
    if created {
        std::os::unix::fs::fchown(&directory, Some(uid), Some(gid))
            .with_context(|| format!("own {}", path.display()))?;
    } else {
        let owner = directory.metadata()?.uid();
        if owner != uid {
            bail!(
                "{} belongs to Unix user {owner}, not {uid}; a tile's directories are never \
                 inherited from another user",
                path.display()
            );
        }
    }
    directory
        .set_permissions(fs::Permissions::from_mode(mode))
        .with_context(|| format!("set the mode of {}", path.display()))?;
    Ok(())
}

/// Everything the tile's first process needs, prepared before the fork so the child only makes
/// system calls with it.
struct Setup {
    name: String,
    uid: u32,
    gid: u32,
    root: PathBuf,
    home: PathBuf,
    work: PathBuf,
    endpoint: PathBuf,
    read_only: Vec<(PathBuf, bool)>,
    sekrets: Option<PathBuf>,
    passwd: String,
    group: String,
    program: PathBuf,
    search: Vec<PathBuf>,
    argv: Vec<CString>,
    environment: Vec<CString>,
}

impl Setup {
    fn new(
        registry: &Registry,
        name: &str,
        tile: &Tile,
        uid: u32,
        dirs: &Directories,
        command: &[OsString],
    ) -> Result<Self> {
        let mut read_only = Vec::new();
        for path in &registry.read_only {
            registry::root_controlled(path).with_context(|| {
                format!("read_only: {} is bound into every tile", path.display())
            })?;
            read_only.push((path.clone(), fs::metadata(path)?.is_dir()));
        }
        // A tile that should reach sekrets and cannot is refused, never started without it.
        if let Some(socket) = &registry.sekrets_socket {
            use std::os::unix::fs::FileTypeExt as _;
            let is_socket =
                fs::symlink_metadata(socket).is_ok_and(|metadata| metadata.file_type().is_socket());
            if !is_socket {
                bail!(
                    "the registry names the sekrets gateway at {}, but there is no socket there; \
                     start st-sekrets.service, or remove sekrets_socket from the registry",
                    socket.display()
                );
            }
        }
        let c = |value: &OsStr| {
            CString::new(value.as_bytes()).context("an argument contains a NUL byte")
        };
        let argv = command
            .iter()
            .map(|arg| c(arg))
            .collect::<Result<Vec<_>>>()?;
        let program = PathBuf::from(&command[0]);
        let path = std::env::join_paths(&registry.path).context("join the tile's PATH")?;
        let mut environment = vec![
            format!("HOME={TILE_HOME}"),
            format!("USER={TILE_USER}"),
            format!("LOGNAME={TILE_USER}"),
            "TMPDIR=/tmp".into(),
            format!("PATH={}", path.to_string_lossy()),
            format!("ST_TILE={name}"),
            format!("ST_AGENT={}", tile.agent),
            format!("ST_TILE_ENDPOINT={TILE_ENDPOINT}"),
        ];
        // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-16-env-fractal-dual-read — DELETE at contraction — https://github.com/compoundingtech/smalltalk/issues/3032
        environment.extend([
            format!("FRACTAL_TILE={name}"),
            format!("FRACTAL_AGENT={}", tile.agent),
            format!("FRACTAL_TILE_ENDPOINT={TILE_ENDPOINT}"),
        ]);
        // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-16-env-fractal-dual-read
        if let Some(term) = std::env::var_os("TERM") {
            environment.push(format!("TERM={}", term.to_string_lossy()));
        }
        let environment = environment
            .into_iter()
            .map(|entry| CString::new(entry).context("an environment value contains NUL"))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            name: name.to_owned(),
            uid,
            gid: uid,
            root: dirs.root.clone(),
            home: dirs.home.clone(),
            work: dirs.work.clone(),
            endpoint: dirs.endpoint.clone(),
            read_only,
            sekrets: registry.sekrets_socket.clone(),
            passwd: format!(
                "root:x:0:0::/:/nonexistent\n{TILE_USER}:x:{uid}:{uid}::{TILE_HOME}:/nonexistent\n"
            ),
            group: format!("root:x:0:\n{TILE_USER}:x:{uid}:\n"),
            program,
            search: registry.path.clone(),
            argv,
            environment,
        })
    }

    /// PID 1 of the tile: build its file system, drop to its user, run the command, reap.
    fn init(self) -> Result<i32> {
        // The guarantee that nothing outlives the launch is the unit's cgroup: when `enter` ends,
        // systemd kills every process in it. The parent-death signal only ends the tile sooner.
        // It cannot tell whether `enter` already died before this call, since getppid is 0 for a
        // parent outside the PID namespace.
        // SAFETY: prctl with these arguments has no memory effects.
        check(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) })?;
        // SAFETY: getppid has no preconditions.
        if unsafe { libc::getppid() } != 0 {
            bail!("the tile's first process is not in a new PID namespace");
        }
        self.build_root()?;
        self.enter_root()?;
        loopback_up()?;
        let hostname = format!("tile-{}", self.name);
        // SAFETY: the pointer and length describe the hostname's bytes.
        check(unsafe { libc::sethostname(hostname.as_ptr().cast(), hostname.len()) })?;
        close_descriptors()?;
        drop_privileges(self.uid, self.gid)?;
        super::seccomp::install().context("install the tile's system call filter")?;
        // SAFETY: the tile is single-threaded here; the child only execs.
        let command = unsafe { libc::fork() };
        if command < 0 {
            return Err(io::Error::last_os_error()).context("fork the tile's command");
        }
        if command == 0 {
            self.exec();
        }
        reap(command)
    }

    fn exec(&self) -> ! {
        let mut argv: Vec<*const libc::c_char> = self.argv.iter().map(|a| a.as_ptr()).collect();
        argv.push(std::ptr::null());
        let mut environment: Vec<*const libc::c_char> =
            self.environment.iter().map(|e| e.as_ptr()).collect();
        environment.push(std::ptr::null());
        // A bare name is looked up on the tile's own PATH, never this process's.
        let candidates = if self.program.components().count() > 1 {
            vec![self.program.clone()]
        } else {
            self.search
                .iter()
                .map(|dir| dir.join(&self.program))
                .collect()
        };
        let mut error = io::Error::from_raw_os_error(libc::ENOENT);
        // SAFETY: chdir and execve get NUL-terminated strings, and argv and environment are
        // NUL-terminated arrays of pointers into strings that live until execve replaces us.
        // The command starts with every signal at its default and none blocked, not with what
        // st's runtime set (Rust ignores SIGPIPE).
        unsafe {
            libc::chdir(c"/tile/work".as_ptr());
            for signal in 1..libc::SIGRTMAX() {
                libc::signal(signal, libc::SIG_DFL);
            }
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
        }
        for candidate in candidates {
            let Ok(path) = path_c(&candidate) else {
                continue;
            };
            unsafe { libc::execve(path.as_ptr(), argv.as_ptr(), environment.as_ptr()) };
            error = io::Error::last_os_error();
        }
        eprintln!(
            "st tiles: {}: cannot run {}: {error}",
            self.name,
            self.program.display()
        );
        // SAFETY: _exit ends the forked child.
        unsafe { libc::_exit(127) }
    }

    /// Mount the tile's file system on a fresh tmpfs at `root`.
    fn build_root(&self) -> Result<()> {
        mount(None, "/", None, libc::MS_REC | libc::MS_PRIVATE, None)
            .context("make the tile's mounts private")?;
        let root = &self.root;
        mount(
            Some("tile"),
            root,
            Some("tmpfs"),
            libc::MS_NOSUID | libc::MS_NODEV,
            Some("mode=0755,size=16m"),
        )
        .context("mount the tile's root")?;
        for (path, is_dir) in &self.read_only {
            let target = inside(root, path);
            if *is_dir {
                fs::create_dir_all(&target)?;
            } else {
                fs::create_dir_all(target.parent().expect("an absolute path has a parent"))?;
                fs::write(&target, b"")?;
            }
            bind(path, &target, true)?;
        }
        for (source, target) in [(&self.home, TILE_HOME), (&self.work, TILE_WORK)] {
            let target = inside(root, Path::new(target));
            fs::create_dir_all(&target)?;
            bind(source, &target, false)?;
        }
        let endpoint = inside(root, Path::new(TILE_ENDPOINT));
        fs::create_dir_all(&endpoint)?;
        bind(&self.endpoint, &endpoint, true)?;
        if let Some(socket) = &self.sekrets {
            let target = inside(root, Path::new(TILE_SEKRETS_SOCKET));
            fs::create_dir_all(target.parent().expect("an absolute path has a parent"))?;
            fs::write(&target, b"")?;
            bind(socket, &target, true)?;
        }
        for (target, options) in [("tmp", "mode=1777"), ("dev", "mode=0755,size=64k")] {
            let target = root.join(target);
            fs::create_dir_all(&target)?;
            mount(
                Some("tile"),
                &target,
                Some("tmpfs"),
                libc::MS_NOSUID | libc::MS_NODEV,
                Some(options),
            )?;
        }
        let dev = root.join("dev");
        for device in DEVICES {
            let target = dev.join(device);
            fs::write(&target, b"")?;
            bind(&Path::new("/dev").join(device), &target, false)?;
        }
        for (link, to) in [
            ("fd", "/proc/self/fd"),
            ("stdin", "/proc/self/fd/0"),
            ("stdout", "/proc/self/fd/1"),
            ("stderr", "/proc/self/fd/2"),
            ("ptmx", "pts/ptmx"),
        ] {
            std::os::unix::fs::symlink(to, dev.join(link))?;
        }
        fs::create_dir(dev.join("pts"))?;
        mount(
            Some("devpts"),
            dev.join("pts"),
            Some("devpts"),
            libc::MS_NOSUID | libc::MS_NOEXEC,
            Some("newinstance,ptmxmode=0666,mode=0620"),
        )?;
        fs::create_dir(dev.join("shm"))?;
        mount(
            Some("tile"),
            dev.join("shm"),
            Some("tmpfs"),
            libc::MS_NOSUID | libc::MS_NODEV,
            Some("mode=1777"),
        )?;
        let proc = root.join("proc");
        fs::create_dir(&proc)?;
        mount(
            Some("proc"),
            &proc,
            Some("proc"),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            None,
        )
        .context("mount the tile's /proc")?;
        let etc = root.join("etc");
        fs::create_dir_all(&etc)?;
        fs::write(etc.join("passwd"), &self.passwd)?;
        fs::write(etc.join("group"), &self.group)?;
        fs::write(etc.join("hostname"), format!("tile-{}\n", self.name))?;
        Ok(())
    }

    /// Make `root` the tile's `/`, detach the host's, and make the root itself read-only.
    fn enter_root(&self) -> Result<()> {
        let root = path_c(&self.root)?;
        // SAFETY: root is a NUL-terminated path; pivot_root(".", ".") stacks the old root under
        // the new one, and the detach below removes it.
        unsafe {
            check(libc::chdir(root.as_ptr())).context("enter the tile's root")?;
            check(libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".".as_ptr()) as i32)
                .context("pivot to the tile's root")?;
            check(libc::umount2(c".".as_ptr(), libc::MNT_DETACH))
                .context("detach the host's root")?;
            check(libc::chdir(c"/".as_ptr()))?;
        }
        mount(
            None,
            "/",
            None,
            libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
            None,
        )
        .context("make the tile's root read-only")?;
        Ok(())
    }
}

/// `path` under `root`.
fn inside(root: &Path, path: &Path) -> PathBuf {
    root.join(path.strip_prefix("/").unwrap_or(path))
}

fn path_c(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).context("a path contains a NUL byte")
}

fn mount(
    source: Option<&str>,
    target: impl AsRef<Path>,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> Result<()> {
    let target = target.as_ref();
    let source = source.map(CString::new).transpose()?;
    let fstype = fstype.map(CString::new).transpose()?;
    let data = data.map(CString::new).transpose()?;
    let target_c = path_c(target)?;
    // SAFETY: every pointer is null or a NUL-terminated string that outlives the call.
    let result = unsafe {
        libc::mount(
            source.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            target_c.as_ptr(),
            fstype.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            flags,
            data.as_ref()
                .map_or(std::ptr::null(), |s| s.as_ptr().cast()),
        )
    };
    check(result).with_context(|| format!("mount at {}", target.display()))
}

/// Bind `source` at `target` without its submounts, read-only when asked, and never with
/// set-user-ID files or device nodes, except that device binds keep their devices.
fn bind(source: &Path, target: &Path, read_only: bool) -> Result<()> {
    let source_c = source
        .to_str()
        .with_context(|| format!("{} is not UTF-8", source.display()))?;
    mount(Some(source_c), target, None, libc::MS_BIND, None)
        .with_context(|| format!("bind {}", source.display()))?;
    let mut flags = libc::MS_REMOUNT | libc::MS_BIND | libc::MS_NOSUID;
    if read_only {
        flags |= libc::MS_RDONLY | libc::MS_NODEV;
    } else if !source.starts_with("/dev") {
        flags |= libc::MS_NODEV;
    }
    mount(None, target, None, flags, None)
        .with_context(|| format!("restrict the bind of {}", source.display()))
}

/// Bring up the new network namespace's loopback, so a tile's own servers can listen on it.
/// Nothing else is configured: a tile has no route to the host or to any other tile.
fn loopback_up() -> Result<()> {
    // SAFETY: a plain socket call; the descriptor is closed below.
    let socket = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if socket < 0 {
        return Err(io::Error::last_os_error()).context("open a socket for the loopback");
    }
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    for (slot, byte) in request.ifr_name.iter_mut().zip(b"lo\0") {
        *slot = *byte as libc::c_char;
    }
    request.ifr_ifru.ifru_flags = (libc::IFF_UP | libc::IFF_LOOPBACK | libc::IFF_RUNNING) as i16;
    // SAFETY: request is a valid ifreq naming lo.
    let result = unsafe { libc::ioctl(socket, libc::SIOCSIFFLAGS, &request) };
    let error = io::Error::last_os_error();
    // SAFETY: socket is ours.
    unsafe { libc::close(socket) };
    if result != 0 {
        return Err(error).context("bring up the tile's loopback");
    }
    Ok(())
}

/// Close every descriptor but standard input, output and error.
fn close_descriptors() -> Result<()> {
    // SAFETY: close_range closes only descriptors; nothing below 3 is touched.
    check(unsafe { libc::close_range(3, u32::MAX, 0) }).context("close inherited descriptors")
}

/// Become the tile's user with no capabilities in any set, and no way to gain one.
fn drop_privileges(uid: u32, gid: u32) -> Result<()> {
    let last = fs::read_to_string("/proc/sys/kernel/cap_last_cap")
        .context("read the last capability")?
        .trim()
        .parse::<libc::c_ulong>()?;
    // SAFETY: each call takes only integers.
    unsafe {
        for capability in 0..=last {
            check(libc::prctl(libc::PR_CAPBSET_DROP, capability, 0, 0, 0))
                .context("drop the capability bounding set")?;
        }
        check(libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong,
            0,
            0,
            0,
        ))?;
        check(libc::setgroups(0, std::ptr::null())).context("drop supplementary groups")?;
        check(libc::setresgid(gid, gid, gid)).context("become the tile's group")?;
        check(libc::setresuid(uid, uid, uid)).context("become the tile's user")?;
        check(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))?;
        // A process with the tile's user may still call setuid back only with a capability.
        if libc::setresuid(0, 0, 0) == 0 {
            bail!("the tile could become root again");
        }
    }
    Ok(())
}

/// Reap every process in the tile until the command exits, then report its status. When this
/// returns and PID 1 exits, the kernel ends whatever else is still running.
fn reap(command: libc::pid_t) -> Result<i32> {
    loop {
        let mut status = 0;
        // SAFETY: status is a valid out pointer.
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if pid < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(error).context("wait for the tile's command");
        }
        if pid == command {
            return Ok(exit_code(status));
        }
    }
}

fn wait_exit(pid: libc::pid_t) -> Result<i32> {
    loop {
        let mut status = 0;
        // SAFETY: status is a valid out pointer.
        let result = unsafe { libc::waitpid(pid, &mut status, 0) };
        if result == pid {
            return Ok(exit_code(status));
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error).context("wait for the tile's first process");
        }
    }
}

fn exit_code(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}

fn check(result: libc::c_int) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_starts_the_tools_unit_in_the_tile_slice_with_its_ceilings() {
        let tile = Tile {
            agent: "agent/fleet/fixture-example/atlas".into(),
            person: "person/ada".into(),
            supervisor_uid: Some(1000),
            tasks_max: 64,
            memory_max: "1G".into(),
            memory_high: Some("768M".into()),
            cpu_weight: 50,
        };
        let plan = run_plan(
            Path::new("/etc/st-tiles/tiles.toml"),
            "atlas",
            &tile,
            Path::new("/usr/local/libexec/st-tiles"),
            false,
            &["sh".into(), "-c".into(), "true".into()],
        );
        let args = plan
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        for expected in [
            "--unit=st-tile-atlas-tools.service",
            "--slice=st_tiles-atlas.slice",
            "--property=KillMode=control-group",
            "--property=TasksMax=64",
            "--property=MemoryMax=1G",
            "--property=MemoryHigh=768M",
            "--property=CPUWeight=50",
            "--pipe",
            "--wait",
            "--collect",
        ] {
            assert!(
                args.iter().any(|arg| arg == expected),
                "{expected} in {args:?}"
            );
        }
        let tail = args[args.iter().position(|arg| arg == "--").unwrap() + 1..].to_vec();
        assert_eq!(
            tail,
            [
                "/usr/local/libexec/st-tiles",
                "tiles",
                "enter",
                "--registry",
                "/etc/st-tiles/tiles.toml",
                "atlas",
                "--",
                "sh",
                "-c",
                "true"
            ]
        );
    }

    #[test]
    fn exit_codes_follow_the_shell() {
        assert_eq!(exit_code(3 << 8), 3);
        assert_eq!(exit_code(libc::SIGKILL), 128 + libc::SIGKILL);
    }
}
