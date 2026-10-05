//! The gateway: `st sekrets serve`, run as the sekrets user. It owns the store, decides each
//! call, and runs allowed commands in the sandbox with the profile's credentials.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde_json::json;

use super::identity::{self, Placement, Statement};
use super::policy::{Policy, Verdict};
use super::protocol::{self, CallerView, Reply, Request, RunRequest};
use super::sandbox::{self, BoundDirectory, Sandbox};
use super::store::{GatewayStore, Grant, Profile, now_unix_ms, person_name};

pub const DEFAULT_CONFIG: &str = "/etc/st-sekrets/gateway.toml";
pub const DEFAULT_SOCKET: &str = "/run/st-sekrets/gateway.sock";
pub const DEFAULT_STORE: &str = "/var/lib/st-sekrets";

/// Root's configuration for the gateway. Root owns this file; the gateway refuses one anyone
/// else can change.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    #[serde(default = "default_socket")]
    pub socket: PathBuf,
    #[serde(default = "default_store")]
    pub store: PathBuf,
    #[serde(default = "default_bwrap")]
    pub bwrap: PathBuf,
    /// Where commands are found. Every directory and tool on it must be root's.
    #[serde(default = "default_path")]
    pub path: Vec<PathBuf>,
    /// Where callers' checkouts may live. Each is hidden in the sandbox except what a caller
    /// passes.
    #[serde(default = "default_checkout_roots")]
    pub checkout_roots: Vec<PathBuf>,
    /// The name callers see, such as the host's.
    #[serde(default)]
    pub name: Option<String>,
    /// Unix user ID to person, such as `1000 = "person/ada"`.
    pub people: BTreeMap<String, String>,
}

fn default_socket() -> PathBuf {
    DEFAULT_SOCKET.into()
}
fn default_store() -> PathBuf {
    DEFAULT_STORE.into()
}
fn default_bwrap() -> PathBuf {
    "/usr/bin/bwrap".into()
}
fn default_path() -> Vec<PathBuf> {
    vec!["/usr/local/bin".into(), "/usr/bin".into(), "/bin".into()]
}
fn default_checkout_roots() -> Vec<PathBuf> {
    vec!["/home".into()]
}

impl GatewayConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let config: Self =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        for (uid, person) in &config.people {
            uid.parse::<u32>()
                .with_context(|| format!("people: `{uid}` is not a Unix user ID"))?;
            if person_name(person).is_none() {
                bail!("people: `{person}` is not a person such as person/ada");
            }
        }
        for root in &config.checkout_roots {
            if !root.is_absolute() || root == Path::new("/") {
                bail!(
                    "checkout_roots: {} must be an absolute directory, not /",
                    root.display()
                );
            }
        }
        Ok(config)
    }

    fn person_for(&self, uid: u32) -> Option<&str> {
        self.people.get(&uid.to_string()).map(String::as_str)
    }
}

pub struct Gateway {
    config: GatewayConfig,
    /// Tools and directories owned by this user, besides root, count as trusted.
    tool_owner: u32,
    /// The gateway's own Unix user, which may never call it.
    own_uid: Option<u32>,
    kernel: Box<dyn Kernel>,
    git: Option<PathBuf>,
    store: Mutex<GatewayStore>,
    logged: Condvar,
}

/// Start the gateway and serve until the process ends.
/// What the gateway asks the kernel about a caller. Tests stand in for it; the gateway itself
/// always reads `/proc`.
pub trait Kernel: Send + Sync {
    fn cgroup(&self, pid: i32) -> Option<String>;
    fn start(&self, pid: i32) -> Option<u64>;
}

struct Proc;

impl Kernel for Proc {
    fn cgroup(&self, pid: i32) -> Option<String> {
        identity::process_cgroup(pid)
    }
    fn start(&self, pid: i32) -> Option<u64> {
        identity::process_start(pid)
    }
}

/// Start the gateway and serve until the process ends.
pub fn serve(config_path: &Path) -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!(
            "the sekrets gateway runs only on Linux: it identifies callers by Linux credentials and cgroups"
        );
    }
    let uid = unsafe { libc::getuid() };
    if uid == 0 {
        bail!("run the gateway as the sekrets user, not root");
    }
    sandbox::trusted_path(config_path, uid).context("the gateway configuration must be root's")?;
    let config = GatewayConfig::load(config_path)?;
    check_store(&config.store, uid)?;
    sandbox::trusted_path(&config.bwrap, uid).context("bubblewrap must be root's")?;
    for directory in &config.path {
        sandbox::trusted_path(directory, uid)
            .with_context(|| format!("{} must be root's", directory.display()))?;
    }
    let socket = config.socket.clone();
    if socket.exists() {
        fs::remove_file(&socket).with_context(|| format!("remove {}", socket.display()))?;
    }
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("listen on {}", socket.display()))?;
    // Anyone may connect; the kernel says who they are.
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o666))?;
    let gateway = Arc::new(Gateway::new(config, uid, Some(uid), Box::new(Proc))?);
    eprintln!(
        "st sekrets: gateway listening on {} for {} people",
        socket.display(),
        gateway.config.people.len()
    );
    gateway.serve_listener(listener);
    Ok(())
}

/// The store must be the gateway's own and closed to everyone else.
fn check_store(store: &Path, uid: u32) -> Result<()> {
    let metadata =
        fs::metadata(store).with_context(|| format!("inspect the store {}", store.display()))?;
    if metadata.uid() != uid {
        bail!("{} must belong to the sekrets user", store.display());
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        bail!(
            "{} is open to other users (mode {:o}); make it 0700, since a seat must never read it",
            store.display(),
            metadata.permissions().mode() & 0o777
        );
    }
    Ok(())
}

/// The calling process and its Unix user, as the kernel reports them. Linux only: the gateway's
/// caller checks rest on Linux credentials and cgroups.
#[cfg(target_os = "linux")]
fn peer_credentials(stream: &UnixStream) -> io::Result<(i32, u32)> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((credentials.pid, credentials.uid))
}

#[cfg(not(target_os = "linux"))]
fn peer_credentials(_stream: &UnixStream) -> io::Result<(i32, u32)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the sekrets gateway runs only on Linux",
    ))
}

fn nonce() -> String {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).expect("the system has randomness");
    hex::encode(bytes)
}

/// The caller as the kernel and, for a seat, its daemon describe it.
struct Caller {
    uid: u32,
    pid: i32,
    person: String,
    view: CallerView,
}

impl Caller {
    fn principal(&self) -> Option<&str> {
        self.view.principal()
    }

    fn require_person(&self) -> Result<&str, String> {
        match &self.view {
            CallerView::Person { person } => Ok(person),
            CallerView::Agent { agent, .. } => Err(format!(
                "{agent} is an agent; only a person changes profiles, grants and keys, from a login session"
            )),
            CallerView::Unidentified { reason, .. } => Err(reason.clone()),
        }
    }
}

/// A profile a caller may use, and the grant that lets them when it is not theirs.
struct Usable {
    profile: Profile,
    grant: Option<Grant>,
}

impl Usable {
    fn judge(&self, argv: &[String]) -> Verdict {
        match self.profile.policy.judge(argv) {
            Verdict::Allowed => {}
            Verdict::Refused(reason) => {
                return Verdict::Refused(format!("profile {}: {reason}", self.profile.id));
            }
        }
        if let Some(grant) = &self.grant
            && let Verdict::Refused(reason) = grant.policy.judge(argv)
        {
            return Verdict::Refused(format!(
                "grant {} of {}: {reason}",
                grant.id, self.profile.id
            ));
        }
        Verdict::Allowed
    }
}

impl Gateway {
    pub fn new(
        config: GatewayConfig,
        tool_owner: u32,
        own_uid: Option<u32>,
        kernel: Box<dyn Kernel>,
    ) -> Result<Self> {
        let git = sandbox::resolve_tool("git", &config.path, tool_owner).ok();
        let store = GatewayStore::open(&config.store)?;
        Ok(Self {
            config,
            tool_owner,
            own_uid,
            kernel,
            git,
            store: Mutex::new(store),
            logged: Condvar::new(),
        })
    }

    /// Serve each connection on its own thread until the listener fails.
    pub fn serve_listener(self: &Arc<Self>, listener: UnixListener) {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let gateway = Arc::clone(self);
            std::thread::spawn(move || {
                if let Err(error) = gateway.connection(&stream) {
                    let _ = protocol::send(
                        &stream,
                        &Reply::Error {
                            message: format!("{error:#}"),
                        },
                        &[],
                    );
                }
            });
        }
    }

    fn store(&self) -> std::sync::MutexGuard<'_, GatewayStore> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn log(
        &self,
        caller: &Caller,
        owner: Option<&str>,
        event: &str,
        profile: Option<&str>,
        detail: serde_json::Value,
    ) -> Result<i64> {
        let seq = self.store().log(
            Some(&caller.person),
            owner,
            event,
            caller.principal(),
            profile,
            &detail,
        )?;
        self.logged.notify_all();
        Ok(seq)
    }

    fn connection(&self, stream: &UnixStream) -> Result<()> {
        let (pid, uid) = peer_credentials(stream)?;
        if Some(uid) == self.own_uid {
            bail!("the sekrets user cannot call its own gateway");
        }
        let Some(person) = self.config.person_for(uid) else {
            bail!("Unix user {uid} is not a person this gateway serves");
        };
        let nonce = nonce();
        let mut caller = Caller {
            uid,
            pid,
            person: person.to_owned(),
            view: self.classify(uid, pid, person, None, &nonce),
        };
        while let Some((request, fds)) = protocol::recv::<Request>(stream)? {
            let reply = match request {
                Request::Hello { attestation } => {
                    caller.view = self.classify(uid, pid, person, attestation.as_ref(), &nonce);
                    Reply::Hello {
                        caller: caller.view.clone(),
                        nonce: nonce.clone(),
                        gateway: self.config.name.clone().unwrap_or_else(|| "sekrets".into()),
                    }
                }
                Request::Run(run) => return self.run(stream, &caller, run, fds),
                Request::Signal { .. } => Reply::Error {
                    message: "nothing is running".into(),
                },
                Request::Log {
                    after,
                    limit,
                    wait_ms,
                } => self.read_log(&caller.person, after, limit, wait_ms)?,
                other => match self.manage(&caller, other) {
                    Ok(value) => Reply::Ok { value },
                    Err(reason) => Reply::Refused { reason, call: None },
                },
            };
            protocol::send(stream, &reply, &[])?;
        }
        Ok(())
    }

    fn classify(
        &self,
        uid: u32,
        pid: i32,
        person: &str,
        attestation: Option<&protocol::Attestation>,
        nonce: &str,
    ) -> CallerView {
        let unidentified = |reason: String| CallerView::Unidentified {
            person: person.to_owned(),
            reason,
        };
        let Some(cgroup) = self.kernel.cgroup(pid) else {
            return unidentified(format!("cannot read the cgroup of process {pid}"));
        };
        match identity::placement(uid, &cgroup) {
            Placement::Session => CallerView::Person {
                person: person.to_owned(),
            },
            Placement::Elsewhere => unidentified(format!(
                "process {pid} runs in {cgroup}, neither a login session nor {person}'s service manager"
            )),
            Placement::UserManager => {
                let Some(attestation) = attestation else {
                    return unidentified(format!(
                        "process {pid} runs in {person}'s service manager, not a login session, and \
                         brought no attestation from its daemon; a seat calls through `st sekrets`, \
                         and a person calls from a login session"
                    ));
                };
                match self.check_attestation(uid, pid, person, &cgroup, attestation, nonce) {
                    Ok(agent) => CallerView::Agent {
                        agent,
                        person: person.to_owned(),
                    },
                    Err(reason) => unidentified(format!("attestation refused: {reason}")),
                }
            }
        }
    }

    fn check_attestation(
        &self,
        uid: u32,
        pid: i32,
        person: &str,
        cgroup: &str,
        attestation: &protocol::Attestation,
        nonce: &str,
    ) -> Result<String, String> {
        let registered = self
            .store()
            .daemon(uid)
            .map_err(|error| error.to_string())?;
        let Some((_, node, key)) = registered else {
            return Err(format!(
                "{person} has registered no daemon key; run `st sekrets enable` from a login session"
            ));
        };
        if !smallclaims::fleet::verify_signature(
            &key,
            &identity::signing_message(&attestation.statement),
            &attestation.signature,
        ) {
            return Err(format!("the signature is not {node}'s registered key"));
        }
        let statement: Statement = serde_json::from_str(&attestation.statement)
            .map_err(|error| format!("unreadable statement: {error}"))?;
        if statement.node != node {
            return Err(format!("statement names {}, not {node}", statement.node));
        }
        if statement.person != person {
            return Err(format!(
                "{} works for {}, but this Unix user is {person}",
                statement.agent, statement.person
            ));
        }
        identity::check_statement(
            &statement,
            pid,
            self.kernel.start(pid),
            cgroup,
            nonce,
            now_unix_ms(),
        )?;
        Ok(statement.agent)
    }

    /// Profiles `caller` may use: their own when they are the person, and any granted to them.
    fn usable(&self, caller: &Caller) -> Result<Vec<Usable>> {
        let Some(principal) = caller.principal() else {
            return Ok(Vec::new());
        };
        let store = self.store();
        let now = now_unix_ms();
        let mut usable = Vec::new();
        for profile in store.profiles()? {
            if matches!(caller.view, CallerView::Person { .. }) && profile.owner == principal {
                usable.push(Usable {
                    profile,
                    grant: None,
                });
            }
        }
        for grant in store.grants()? {
            if grant.until_unix_ms.is_some_and(|until| until <= now)
                || !identity::principal_matches(&grant.grantee, principal)
                || usable
                    .iter()
                    .any(|u| u.profile.id == grant.profile && u.grant.is_none())
            {
                continue;
            }
            if let Some(profile) = store.profile(&grant.profile)? {
                usable.push(Usable {
                    profile,
                    grant: Some(grant),
                });
            }
        }
        Ok(usable)
    }

    /// Choose the profile for a run, or say why none fits.
    fn choose(&self, caller: &Caller, run: &RunRequest) -> Result<Result<Usable, String>> {
        let Some(principal) = caller.principal() else {
            let CallerView::Unidentified { reason, .. } = &caller.view else {
                unreachable!()
            };
            return Ok(Err(format!("the caller is not identified: {reason}")));
        };
        let usable = self.usable(caller)?;
        if run.login {
            let Some(id) = &run.profile else {
                return Ok(Err("a login names its profile".into()));
            };
            return Ok(usable
                .into_iter()
                .find(|u| &u.profile.id == id && u.grant.is_none())
                .ok_or_else(|| {
                    format!("{principal} does not own profile {id}; only its owner logs it in")
                }));
        }
        if let Some(id) = &run.profile {
            // The owner's own use first, then any grant that allows the command.
            let mut matching = usable
                .into_iter()
                .filter(|u| &u.profile.id == id)
                .peekable();
            if matching.peek().is_none() {
                return Ok(Err(format!(
                    "profile {id} is not {principal}'s and no grant gives it to {principal}"
                )));
            }
            let mut reasons = Vec::new();
            for candidate in matching {
                match candidate.judge(&run.argv) {
                    Verdict::Allowed => return Ok(Ok(candidate)),
                    Verdict::Refused(reason) => reasons.push(reason),
                }
            }
            return Ok(Err(reasons.join("; ")));
        }
        if usable.is_empty() {
            return Ok(Err(format!(
                "{principal} owns no profile and has been granted none"
            )));
        }
        if let Some(default) = usable
            .iter()
            .position(|u| u.grant.is_none() && u.profile.default)
        {
            let mut usable = usable;
            let candidate = usable.swap_remove(default);
            return Ok(match candidate.judge(&run.argv) {
                Verdict::Allowed => Ok(candidate),
                Verdict::Refused(reason) => Err(reason),
            });
        }
        let mut allowed = Vec::new();
        let mut reasons = Vec::new();
        for candidate in usable {
            match candidate.judge(&run.argv) {
                Verdict::Allowed => allowed.push(candidate),
                Verdict::Refused(reason) => reasons.push(reason),
            }
        }
        match allowed.len() {
            0 => Ok(Err(reasons.join("; "))),
            1 => Ok(Ok(allowed.pop().expect("one"))),
            _ => {
                let mut ids = allowed
                    .iter()
                    .map(|u| u.profile.id.as_str())
                    .collect::<Vec<_>>();
                ids.dedup();
                Ok(Err(format!(
                    "more than one profile allows this; choose one with --profile ({})",
                    ids.join(", ")
                )))
            }
        }
    }

    fn refuse(
        &self,
        stream: &UnixStream,
        caller: &Caller,
        run: &RunRequest,
        owner: Option<&str>,
        profile: Option<&str>,
        reason: String,
    ) -> Result<()> {
        let call = self.log(
            caller,
            owner,
            "refused",
            profile,
            json!({ "argv": run.argv, "reason": reason, "login": run.login }),
        )?;
        protocol::send(
            stream,
            &Reply::Refused {
                reason,
                call: Some(call),
            },
            &[],
        )?;
        Ok(())
    }

    fn run(
        &self,
        stream: &UnixStream,
        caller: &Caller,
        run: RunRequest,
        fds: Vec<OwnedFd>,
    ) -> Result<()> {
        let usable = match self.choose(caller, &run)? {
            Ok(usable) => usable,
            Err(reason) => {
                let profile = run.profile.clone();
                return self.refuse(stream, caller, &run, None, profile.as_deref(), reason);
            }
        };
        let profile = usable.profile.id.clone();
        let owner = usable.profile.owner.clone();
        let lock = self.store().lock_for(&caller.person, &owner)?;
        if let Some(lock) = lock {
            let reason = format!(
                "sekrets is locked for {} by {}{}",
                if lock.scope == "*" {
                    "everyone"
                } else {
                    &lock.scope
                },
                lock.locked_by,
                lock.reason
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            );
            return self.refuse(stream, caller, &run, Some(&owner), Some(&profile), reason);
        }
        let prepared = match self.prepare(&usable.profile, &run, fds) {
            Ok(prepared) => prepared,
            Err(error) => {
                return self.refuse(
                    stream,
                    caller,
                    &run,
                    Some(&owner),
                    Some(&profile),
                    format!("{error:#}"),
                );
            }
        };
        let cwd = prepared.cwd.display().to_string();
        let call = self.log(
            caller,
            Some(&owner),
            "call",
            Some(&profile),
            json!({
                "argv": run.argv,
                "cwd": cwd,
                "grant": usable.grant.as_ref().map(|g| g.id.clone()),
                "login": run.login,
                "tty": run.tty.is_some(),
            }),
        )?;
        let outcome = self.execute(stream, &run, prepared, &profile, call);
        let exit = match &outcome {
            Ok(status) => json!({ "call": call, "code": status.code(), "signal": status.signal() }),
            Err(error) => json!({ "call": call, "error": format!("{error:#}") }),
        };
        self.log(caller, Some(&owner), "exited", Some(&profile), exit)?;
        let status = outcome?;
        protocol::send(
            stream,
            &Reply::Exited {
                code: status.code(),
                signal: status.signal(),
            },
            &[],
        )?;
        Ok(())
    }

    /// Whether the caller's working directory can be the command's: under a checkout root, and
    /// a directory the sekrets user may enter.
    fn servable(&self, fd: &OwnedFd) -> Result<()> {
        let path = sandbox::descriptor_path(fd)?;
        sandbox::check_checkout_path(&path, &self.config.checkout_roots)?;
        reachable(&path)
    }

    fn prepare(&self, profile: &Profile, run: &RunRequest, fds: Vec<OwnedFd>) -> Result<Prepared> {
        let Some(name) = run.argv.first() else {
            bail!("no command");
        };
        let tool = sandbox::resolve_tool(name, &self.config.path, self.tool_owner)?;
        let streams = if run.tty.is_some() { 0 } else { 3 };
        if fds.len() != streams + run.directories || run.directories == 0 {
            bail!(
                "the request passed {} descriptors, not {}",
                fds.len(),
                streams + run.directories
            );
        }
        let mut fds = fds.into_iter();
        let stdio = (&mut fds).take(streams).collect::<Vec<_>>();
        let home = self.store().profile_home(&profile.id);
        let mut fds = fds.collect::<Vec<_>>();
        // A command runs in the caller's directory when the sekrets user may enter it there;
        // otherwise (a 0700 home, or somewhere outside the checkout roots) in the profile's home,
        // with no checkout, and the caller is told.
        let mut note = None;
        if let Err(reason) = self.servable(&fds[0]) {
            note = Some(format!(
                "running in the profile's home, without your directory: {reason:#}"
            ));
            fds.clear();
        }
        let mut directories = Vec::new();
        for fd in fds {
            let path = sandbox::descriptor_path(&fd)?;
            let metadata = fs::metadata(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
            if !metadata.is_dir() {
                bail!("{} is not a directory", path.display());
            }
            sandbox::check_checkout_path(&path, &self.config.checkout_roots)?;
            reachable(&path)?;
            directories.push(BoundDirectory { fd, path });
        }
        let cwd = directories
            .first()
            .map(|directory| directory.path.clone())
            .unwrap_or_else(|| home.clone());
        // Parents first, so a nested directory is bound over its parent's view.
        directories.sort_by_key(|directory| directory.path.components().count());
        let scratch = tempfile::Builder::new()
            .prefix("call-")
            .tempdir_in(private_dir(self.config.store.join("scratch"))?)?;
        let view = if directories.is_empty() {
            sandbox::CheckoutView::default()
        } else {
            sandbox::prepare_checkout(&directories, &cwd, self.git.as_deref(), scratch.path())?
        };
        let env = self.store().env(&profile.id)?;
        Ok(Prepared {
            tool,
            home,
            note,
            stdio,
            directories,
            view,
            cwd,
            env,
            _scratch: scratch,
        })
    }

    fn execute(
        &self,
        stream: &UnixStream,
        run: &RunRequest,
        prepared: Prepared,
        profile: &str,
        call: i64,
    ) -> Result<std::process::ExitStatus> {
        let prepared_note = prepared.note.clone();
        let mut hidden = vec![
            self.config.store.clone(),
            self.config
                .socket
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
        ];
        hidden.extend(self.config.checkout_roots.iter().cloned());
        let sandbox = Sandbox {
            bwrap: &self.config.bwrap,
            tool: &prepared.tool,
            args: &run.argv[1..],
            home: &prepared.home,
            hidden: &hidden,
            path: &self.config.path,
            directories: &prepared.directories,
            overlays: &prepared.view.overlays,
            empty_dirs: &prepared.view.empty_dirs,
            cwd: &prepared.cwd,
            env: &prepared.env,
            term: run
                .term
                .as_deref()
                .filter(|term| run.tty.is_some() && plain_term(term))
                .or(run.tty.map(|_| "xterm-256color")),
            user: "sekrets",
            new_session: run.tty.is_none(),
        };
        let mut command = sandbox.command();
        let keep: Vec<RawFd> = prepared
            .directories
            .iter()
            .map(|d| d.fd.as_raw_fd())
            .collect();
        let mut master = None;
        if let Some(size) = run.tty {
            let (pty_master, slave) = open_pty(size)?;
            command.stdin(std::process::Stdio::from(slave.try_clone()?));
            command.stdout(std::process::Stdio::from(slave.try_clone()?));
            command.stderr(std::process::Stdio::from(slave));
            master = Some(pty_master);
            unsafe {
                command.pre_exec(move || {
                    if libc::setsid() < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    clear_cloexec(&keep)
                });
            }
        } else {
            let mut stdio = prepared.stdio.into_iter();
            let (Some(stdin), Some(stdout), Some(stderr)) =
                (stdio.next(), stdio.next(), stdio.next())
            else {
                bail!("a run without a terminal passes stdin, stdout and stderr");
            };
            command.stdin(std::process::Stdio::from(stdin));
            command.stdout(std::process::Stdio::from(stdout));
            command.stderr(std::process::Stdio::from(stderr));
            unsafe {
                command.pre_exec(move || clear_cloexec(&keep));
            }
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("start {}", self.config.bwrap.display()))?;
        drop(command);
        let master_fds: Vec<RawFd> = master.iter().map(|m| m.as_raw_fd()).collect();
        protocol::send(
            stream,
            &Reply::Started {
                profile: profile.to_owned(),
                call,
                note: prepared_note,
            },
            &master_fds,
        )?;
        drop(master);
        // The caller may forward a signal; when the caller goes away, so does the command.
        let mut caller_gone = false;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            if caller_gone {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            let mut poll = libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut poll, 1, 100) } <= 0 {
                continue;
            }
            match protocol::recv::<Request>(stream) {
                Ok(Some((Request::Signal { .. }, _))) => {
                    let _ = child.kill();
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    caller_gone = true;
                }
            }
        }
    }

    fn read_log(&self, person: &str, after: i64, limit: i64, wait_ms: u64) -> Result<Reply> {
        let deadline = std::time::Instant::now() + Duration::from_millis(wait_ms.min(300_000));
        let mut store = self.store();
        loop {
            let entries = store.log_for(person, after, limit)?;
            let now = std::time::Instant::now();
            if !entries.is_empty() || now >= deadline {
                return Ok(Reply::Ok {
                    value: serde_json::to_value(entries)?,
                });
            }
            store = self
                .logged
                .wait_timeout(store, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn manage(&self, caller: &Caller, request: Request) -> Result<serde_json::Value, String> {
        let text = |error: anyhow::Error| format!("{error:#}");
        match request {
            Request::ProfileList => {
                let usable = self.usable(caller).map_err(text)?;
                let mut seen = Vec::new();
                let mut listed = Vec::new();
                for u in usable {
                    if seen.contains(&u.profile.id) {
                        continue;
                    }
                    seen.push(u.profile.id.clone());
                    listed.push(json!({
                        "profile": u.profile,
                        "grant": u.grant,
                    }));
                }
                Ok(json!(listed))
            }
            Request::ProfileShow { profile } => {
                let usable = self.usable(caller).map_err(text)?;
                let found = usable
                    .into_iter()
                    .filter(|u| u.profile.id == profile)
                    .map(|u| json!({ "profile": u.profile, "grant": u.grant }))
                    .collect::<Vec<_>>();
                if found.is_empty() {
                    return Err(format!("no profile {profile} that you may use"));
                }
                Ok(json!(found))
            }
            Request::GrantList => {
                let principal = caller
                    .principal()
                    .ok_or_else(|| caller.require_person().err().unwrap_or_default())?;
                let store = self.store();
                let owned = store
                    .profiles()
                    .map_err(text)?
                    .into_iter()
                    .filter(|p| p.owner == principal)
                    .map(|p| p.id)
                    .collect::<Vec<_>>();
                let grants = store
                    .grants()
                    .map_err(text)?
                    .into_iter()
                    .filter(|g| {
                        owned.contains(&g.profile)
                            || identity::principal_matches(&g.grantee, principal)
                    })
                    .collect::<Vec<_>>();
                Ok(json!(grants))
            }
            Request::Lock { person, reason } => {
                let principal = caller
                    .principal()
                    .ok_or_else(|| caller.require_person().err().unwrap_or_default())?
                    .to_owned();
                let scope = match person {
                    None => {
                        caller.require_person()?;
                        "*".to_owned()
                    }
                    Some(person) if person_name(&person).is_some() => person,
                    Some(other) => return Err(format!("`{other}` is not a person")),
                };
                self.store()
                    .lock(&scope, &principal, reason.as_deref())
                    .map_err(text)?;
                self.log(
                    caller,
                    None,
                    "locked",
                    None,
                    json!({ "scope": scope, "reason": reason }),
                )
                .map_err(text)?;
                Ok(json!({ "locked": scope }))
            }
            Request::Unlock { person } => {
                let me = caller.require_person()?.to_owned();
                let scope = person.unwrap_or_else(|| "*".into());
                if scope != "*" && scope != me {
                    return Err(format!("only {scope} unlocks {scope}"));
                }
                let removed = self.store().unlock(&scope).map_err(text)?;
                self.log(caller, None, "unlocked", None, json!({ "scope": scope }))
                    .map_err(text)?;
                Ok(json!({ "unlocked": scope, "was_locked": removed }))
            }
            Request::Register { node, key } => {
                let person = caller.require_person()?.to_owned();
                if !node.starts_with("host/") {
                    return Err(format!("`{node}` is not a node such as host/example"));
                }
                self.store()
                    .register_daemon(caller.uid, &person, &node, &key)
                    .map_err(text)?;
                self.log(
                    caller,
                    None,
                    "daemon-registered",
                    None,
                    json!({ "node": node, "uid": caller.uid, "pid": caller.pid }),
                )
                .map_err(text)?;
                Ok(json!({ "node": node, "person": person }))
            }
            other => self.manage_owned(caller, other),
        }
    }

    /// Changes only a profile's owner makes, from a login session.
    fn manage_owned(&self, caller: &Caller, request: Request) -> Result<serde_json::Value, String> {
        let text = |error: anyhow::Error| format!("{error:#}");
        let person = caller.require_person()?.to_owned();
        let owned = |id: &str| -> Result<Profile, String> {
            match self.store().profile(id).map_err(text)? {
                Some(profile) if profile.owner == person => Ok(profile),
                Some(profile) => Err(format!("{id} belongs to {}", profile.owner)),
                None => Err(format!("no profile {id}")),
            }
        };
        match request {
            Request::ProfileCreate {
                profile,
                description,
                policy,
                default,
            } => {
                check_policy(&policy)?;
                let created = self
                    .store()
                    .create_profile(&profile, &person, description.as_deref(), &policy, default)
                    .map_err(text)?;
                self.log(
                    caller,
                    Some(&person),
                    "profile-created",
                    Some(&profile),
                    json!({ "policy": policy, "default": default }),
                )
                .map_err(text)?;
                Ok(json!(created))
            }
            Request::ProfileRemove { profile } => {
                owned(&profile)?;
                self.store().remove_profile(&profile).map_err(text)?;
                self.log(
                    caller,
                    Some(&person),
                    "profile-removed",
                    Some(&profile),
                    json!({}),
                )
                .map_err(text)?;
                Ok(json!({ "removed": profile }))
            }
            Request::PolicySet { profile, policy } => {
                owned(&profile)?;
                check_policy(&policy)?;
                self.store().set_policy(&profile, &policy).map_err(text)?;
                self.log(
                    caller,
                    Some(&person),
                    "policy-set",
                    Some(&profile),
                    json!({ "policy": policy }),
                )
                .map_err(text)?;
                Ok(json!({ "profile": profile, "policy": policy }))
            }
            Request::Put {
                profile,
                name,
                value,
            } => {
                owned(&profile)?;
                if value.is_empty() {
                    return Err("the value is empty".into());
                }
                self.store().put(&profile, &name, &value).map_err(text)?;
                self.log(
                    caller,
                    Some(&person),
                    "put",
                    Some(&profile),
                    json!({ "name": name }),
                )
                .map_err(text)?;
                Ok(json!({ "profile": profile, "name": name }))
            }
            Request::Unset { profile, name } => {
                owned(&profile)?;
                let removed = self.store().unset(&profile, &name).map_err(text)?;
                self.log(
                    caller,
                    Some(&person),
                    "unset",
                    Some(&profile),
                    json!({ "name": name }),
                )
                .map_err(text)?;
                Ok(json!({ "profile": profile, "name": name, "removed": removed }))
            }
            Request::GrantAdd {
                profile,
                to,
                policy,
                until_unix_ms,
            } => {
                owned(&profile)?;
                check_policy(&policy)?;
                if !(to.starts_with("agent/") || to.starts_with("person/")) {
                    return Err(format!("grant to an agent or a person, not `{to}`"));
                }
                if to == person {
                    return Err(format!("{person} owns {profile} already"));
                }
                if to.starts_with("agent/")
                    && let Some(rule) = policy.broad_allow()
                {
                    return Err(format!(
                        "a grant to an agent starts from an allow list of subcommands; `{rule}` allows a whole command"
                    ));
                }
                if until_unix_ms.is_some_and(|until| until <= now_unix_ms()) {
                    return Err("that expiry has passed".into());
                }
                let grant = self
                    .store()
                    .add_grant(&profile, &to, &policy, until_unix_ms, &person)
                    .map_err(text)?;
                self.log(
                    caller,
                    Some(&person),
                    "grant-added",
                    Some(&profile),
                    json!({ "grant": grant.id, "to": to, "policy": policy, "until_unix_ms": until_unix_ms }),
                )
                .map_err(text)?;
                Ok(json!(grant))
            }
            Request::GrantRemove { grant } => {
                let found = self
                    .store()
                    .grant(&grant)
                    .map_err(text)?
                    .ok_or_else(|| format!("no grant {grant}"))?;
                owned(&found.profile)?;
                self.store().revoke_grant(&grant, &person).map_err(text)?;
                self.log(
                    caller,
                    Some(&person),
                    "grant-removed",
                    Some(&found.profile),
                    json!({ "grant": grant, "to": found.grantee }),
                )
                .map_err(text)?;
                Ok(json!({ "removed": grant }))
            }
            _ => Err("that request is not a management request".into()),
        }
    }
}

/// A terminal type is a short plain name; it never names a file.
fn plain_term(term: &str) -> bool {
    !term.is_empty()
        && term.len() <= 64
        && term
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+'))
        && !term.starts_with('.')
}

/// Whether the sekrets user can walk to `path` and enter it. bubblewrap binds a passed directory
/// by the path its descriptor resolves to, so every directory above it must let the sekrets user
/// search it; setup grants search, never read, on each person's home.
fn reachable(path: &Path) -> Result<()> {
    let text = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    if unsafe { libc::access(text.as_ptr(), libc::X_OK) } != 0 {
        bail!(
            "the sekrets user may not reach {}; let it search the directories above it \
             (setup does `setfacl -m u:sekrets:x` on your home), or run from a checkout it can reach",
            path.display()
        );
    }
    Ok(())
}

fn check_policy(policy: &Policy) -> Result<(), String> {
    if policy.allow.is_empty() {
        return Err("a policy needs at least one allow rule or preset".into());
    }
    Ok(())
}

struct Prepared {
    tool: PathBuf,
    home: PathBuf,
    note: Option<String>,
    stdio: Vec<OwnedFd>,
    directories: Vec<BoundDirectory>,
    view: sandbox::CheckoutView,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    _scratch: tempfile::TempDir,
}

fn private_dir(path: PathBuf) -> Result<PathBuf> {
    super::store::create_private_dir(&path)?;
    Ok(path)
}

fn clear_cloexec(fds: &[RawFd]) -> io::Result<()> {
    for fd in fds {
        let flags = unsafe { libc::fcntl(*fd, libc::F_GETFD) };
        if flags < 0 || unsafe { libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn open_pty(size: protocol::Winsize) -> Result<(OwnedFd, OwnedFd)> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let mut winsize = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            // Linux takes a const pointer, macOS a mutable one; a raw pointer suits both.
            std::ptr::addr_of_mut!(winsize),
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error()).context("open a terminal for the command");
    }
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok((master, slave))
}
