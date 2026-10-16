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
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde_json::json;

use super::identity::{self, Placement, Statement};
use super::policy::{Policy, Verdict};
use super::protocol::{self, CallerView, Reply, Request, RunRequest};
use super::sandbox::{self, BoundDirectory, Sandbox};
use super::store::{GatewayStore, Grant, Profile, now_unix_ms, person_name};
use crate::tile_registry::{Registry, Tile};

pub const DEFAULT_CONFIG: &str = "/etc/st-sekrets/gateway.toml";
/// How long the gateway keeps its own log: each daemon has recorded its entries long before.
const LOG_KEEP_MS: i64 = 7 * 24 * 60 * 60 * 1000;
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
    /// The API base an authorized request may reach with a profile's token.
    #[serde(default = "default_github_api")]
    pub github_api: String,
    /// The name callers see, such as the host's.
    #[serde(default)]
    pub name: Option<String>,
    /// Unix user ID to person, such as `1000 = "person/ada"`.
    pub people: BTreeMap<String, String>,
    /// Dedicated Unix UID to a root-controlled, run-only system service.
    #[serde(default)]
    pub programs: BTreeMap<String, ProgramConfig>,
    /// Root's tile registry. A tile's Unix user is one agent, which the kernel identifies
    /// without any daemon's word: its process runs in the tile's root-owned cgroup.
    #[serde(default = "default_tile_registry")]
    pub tile_registry: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramConfig {
    pub principal: String,
    pub person: String,
    /// Exact system unit, not a user-manager unit or a caller-supplied label.
    pub unit: String,
    /// Exact installed command argv; profile and grant must also allow it.
    pub command: Vec<String>,
    /// A runner's private bind mount. Absent for ordinary host-path working directories.
    #[serde(default)]
    pub checkout: Option<ProgramCheckout>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramCheckout {
    pub backing_root: PathBuf,
    pub mount_root: PathBuf,
}

pub(super) fn program_principal(value: &str) -> bool {
    let parts: Vec<_> = value.split('/').collect();
    value.len() <= 256
        && parts.len() == 3
        && parts[0] == "program"
        && parts[1..].iter().all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        })
}

fn system_unit(value: &str) -> bool {
    value.len() > ".service".len()
        && value.len() <= 255
        && value.ends_with(".service")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-@".contains(&byte))
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
fn default_github_api() -> String {
    "https://api.github.com".into()
}
fn default_tile_registry() -> PathBuf {
    crate::tile_registry::DEFAULT_REGISTRY.into()
}
fn default_checkout_roots() -> Vec<PathBuf> {
    vec!["/home".into()]
}

impl GatewayConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let config: Self =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        for (uid, person) in &self.people {
            uid.parse::<u32>()
                .with_context(|| format!("people: `{uid}` is not a Unix user ID"))?;
            if person_name(person).is_none() {
                bail!("people: `{person}` is not a person such as person/ada");
            }
        }
        let mut principals = std::collections::BTreeSet::new();
        for (uid, program) in &self.programs {
            let number = uid.parse::<u32>().context("program UID must be numeric")?;
            if number == 0
                || uid != &number.to_string()
                || self
                    .people
                    .keys()
                    .any(|key| key.parse::<u32>().ok() == Some(number))
            {
                bail!("program UID must be non-root, canonical and separate from people");
            }
            if let Some(checkout) = &program.checkout {
                for path in [&checkout.backing_root, &checkout.mount_root] {
                    if !path.is_absolute()
                        || path == Path::new("/")
                        || path.components().any(|part| {
                            !matches!(
                                part,
                                std::path::Component::RootDir | std::path::Component::Normal(_)
                            )
                        })
                    {
                        bail!("program checkout roots must be plain absolute directories, never /");
                    }
                }
                if checkout.backing_root == checkout.mount_root
                    || checkout.backing_root.starts_with(&self.store)
                    || self.store.starts_with(&checkout.backing_root)
                    || checkout.mount_root.starts_with(&self.store)
                    || self.store.starts_with(&checkout.mount_root)
                {
                    bail!(
                        "program checkout must not overlap the secret store or use the same physical/virtual root"
                    );
                }
            }
            if !program_principal(&program.principal)
                || !principals.insert(&program.principal)
                || !self.people.values().any(|person| person == &program.person)
                || !system_unit(&program.unit)
                || program.command.is_empty()
                || program.command.len() > 16
                || program
                    .command
                    .iter()
                    .any(|word| word.is_empty() || word.len() > 1024 || word.contains('\0'))
                || !program.command[0]
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
            {
                bail!(
                    "program needs a unique program/HOST/NAME, an enrolled person and exact system service unit"
                );
            }
        }
        for root in &self.checkout_roots {
            if !root.is_absolute() || root == Path::new("/") {
                bail!(
                    "checkout_roots: {} must be an absolute directory, not /",
                    root.display()
                );
            }
        }
        Ok(())
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
    http: OnceLock<Arc<super::authorized::Http>>,
    /// Each profile's API token, read from the profile, until a 401 says it changed.
    tokens: Mutex<BTreeMap<String, String>>,
    /// Reads root's tile registry for each connection, so a change applies to the next one.
    tiles: Box<dyn Fn() -> Result<Option<Registry>> + Send + Sync>,
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
    if !config.programs.is_empty() {
        sandbox::trusted_path(config_path, 0)
            .context("program enrollment must be controlled by root, not the profile user")?;
    }
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
    pid_start: Option<u64>,
    person: String,
    view: CallerView,
    /// The tile this caller's Unix user is, when it is one.
    tile: Option<String>,
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
            CallerView::Host { node, .. } => Err(format!(
                "{node}'s st is not a person; only a person changes profiles, grants and keys, from a login session"
            )),
            CallerView::Program { program, .. } => Err(format!(
                "{program} is run-only; only a person changes profiles, grants and keys"
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
        config.validate()?;
        let git = sandbox::resolve_tool("git", &config.path, tool_owner).ok();
        let store = GatewayStore::open(&config.store)?;
        let registry = config.tile_registry.clone();
        let tiles = Box::new(move || match fs::symlink_metadata(&registry) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            _ => Registry::load(&registry).map(Some),
        });
        Ok(Self {
            config,
            tool_owner,
            own_uid,
            kernel,
            git,
            store: Mutex::new(store),
            logged: Condvar::new(),
            http: OnceLock::new(),
            tokens: Mutex::new(BTreeMap::new()),
            tiles,
        })
    }

    /// Read the tile registry with `load` instead of from root's file. Tests stand in for it.
    pub fn with_tiles(
        mut self,
        load: impl Fn() -> Result<Option<Registry>> + Send + Sync + 'static,
    ) -> Self {
        self.tiles = Box::new(load);
        self
    }

    /// Serve each connection on its own thread until the listener fails.
    pub fn serve_listener(self: &Arc<Self>, listener: UnixListener) {
        let trimmer = Arc::clone(self);
        std::thread::spawn(move || {
            loop {
                if let Err(error) = trimmer.store().trim_log(LOG_KEEP_MS) {
                    eprintln!("st sekrets: trim the log: {error:#}");
                }
                std::thread::sleep(Duration::from_secs(60 * 60));
            }
        });
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
        self.connection_from_peer(stream, pid, uid)
    }

    // Production always obtains these values from SO_PEERCRED above. Keeping the protocol
    // loop separate lets controls change kernel lifetime/placement between real frames.
    fn connection_from_peer(&self, stream: &UnixStream, pid: i32, uid: u32) -> Result<()> {
        if Some(uid) == self.own_uid {
            bail!("the sekrets user cannot call its own gateway");
        }
        let program = self.config.programs.get(&uid.to_string());
        let person = program
            .map(|program| program.person.as_str())
            .or_else(|| self.config.person_for(uid));
        // A person's or program's call never depends on the registry; a tile's call needs it.
        let registry = match (self.tiles)() {
            Ok(registry) => registry,
            Err(_) if person.is_some() => None,
            Err(error) => return Err(error.context("read root's tile registry")),
        };
        let tile = registry
            .as_ref()
            .and_then(|registry| registry.tile_for_uid(uid))
            .map(|(name, tile)| (name.to_owned(), tile.clone()));
        let tile_name = tile.as_ref().map(|(name, _)| name.clone());
        let (person, tile_view) = match (person, tile) {
            (Some(_), Some((name, _))) => bail!(
                "Unix user {uid} is both a person in the gateway's configuration and tile {name} \
                 in root's registry; a tile is never a person"
            ),
            (Some(person), None) => (person.to_owned(), None),
            (None, Some((name, tile))) => {
                let view = self.classify_tile(
                    registry.as_ref().expect("a tile came from the registry"),
                    uid,
                    pid,
                    &name,
                    &tile,
                );
                (tile.person, Some(view))
            }
            (None, None) => bail!("Unix user {uid} is not a person or tile this gateway serves"),
        };
        let person = person.as_str();
        let nonce = nonce();
        let classify = |attestation: Option<&protocol::Attestation>| match &tile_view {
            // The kernel and root's registry name a tile; no daemon's word changes that.
            Some(view) => view.clone(),
            None => self.classify(uid, pid, person, attestation, &nonce),
        };
        let mut caller = Caller {
            uid,
            pid,
            pid_start: self.kernel.start(pid),
            person: person.to_owned(),
            view: classify(None),
            tile: tile_name,
        };
        while let Some((request, fds)) = protocol::recv::<Request>(stream)? {
            // A program may not keep an earlier classification after leaving its unit,
            // or transfer a connection whose original process has exited/reused its PID.
            if self.config.programs.contains_key(&uid.to_string()) {
                caller.view =
                    if caller.pid_start.is_some() && self.kernel.start(pid) == caller.pid_start {
                        self.classify(uid, pid, person, None, &nonce)
                    } else {
                        CallerView::Unidentified {
                            person: person.to_owned(),
                            reason: "program process lifetime changed".into(),
                        }
                    };
            }
            let reply = match request {
                Request::Hello { attestation } => {
                    if !self.config.programs.contains_key(&uid.to_string()) {
                        caller.view = classify(attestation.as_ref());
                    }
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
                } => {
                    if self.config.programs.contains_key(&uid.to_string()) {
                        Reply::Refused {
                            reason: "program callers are run-only; the person reads the audit log"
                                .into(),
                            call: None,
                        }
                    } else {
                        self.read_log(&caller.person, after, limit, wait_ms)?
                    }
                }
                Request::Authorized(call) => self.authorized(&caller, call)?,
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
        if let Some(program) = self.config.programs.get(&uid.to_string()) {
            let unit = format!("/system.slice/{}", program.unit);
            return if cgroup == unit
                || cgroup
                    .strip_prefix(&unit)
                    .is_some_and(|rest| rest.starts_with('/'))
            {
                CallerView::Program {
                    program: program.principal.clone(),
                    person: program.person.clone(),
                }
            } else {
                unidentified(format!(
                    "program process is not in enrolled system unit {}",
                    program.unit
                ))
            };
        }
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
                    Ok(node) if node.starts_with("host/") => CallerView::Host {
                        node,
                        person: person.to_owned(),
                    },
                    Ok(agent) => CallerView::Agent {
                        agent,
                        person: person.to_owned(),
                    },
                    Err(reason) => unidentified(format!("attestation refused: {reason}")),
                }
            }
        }
    }

    /// A process of a tile's Unix user is the tile's agent when it runs in the tile's slice,
    /// which only root can place it in. Anywhere else it is no one.
    fn classify_tile(
        &self,
        registry: &Registry,
        uid: u32,
        pid: i32,
        name: &str,
        tile: &Tile,
    ) -> CallerView {
        let unidentified = |reason: String| CallerView::Unidentified {
            person: tile.person.clone(),
            reason,
        };
        let Some(cgroup) = self.kernel.cgroup(pid) else {
            return unidentified(format!("cannot read the cgroup of process {pid}"));
        };
        match registry.identify(uid, &cgroup) {
            Some((found, _)) if found == name => CallerView::Agent {
                agent: tile.agent.clone(),
                person: tile.person.clone(),
            },
            _ => unidentified(format!(
                "process {pid} is tile {name}'s Unix user but runs in {cgroup}, outside the tile"
            )),
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
        if statement.agent.starts_with("host/") && statement.agent != node {
            return Err(format!("{} speaks only for {node}", statement.agent));
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
        if let Some(program) = self.config.programs.get(&caller.uid.to_string())
            && (run.login || run.argv != program.command)
        {
            return Ok(Err(
                "program may only run its root-enrolled command, never login".into(),
            ));
        }
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
        mut run: RunRequest,
        mut fds: Vec<OwnedFd>,
    ) -> Result<()> {
        // The files the command reads come last; the policy judges the arguments as the command
        // will see them, naming those descriptors.
        let streams = if run.tty.is_some() { 0 } else { 3 };
        if fds.len() != streams + run.directories + run.files {
            let reason = format!(
                "the request passed {} descriptors, not {}",
                fds.len(),
                streams + run.directories + run.files
            );
            return self.refuse(
                stream,
                caller,
                &run,
                None,
                run.profile.clone().as_deref(),
                reason,
            );
        }
        let files = fds.split_off(streams + run.directories);
        if let Err(reason) = super::files::substitute(&mut run.argv, &files) {
            return self.refuse(
                stream,
                caller,
                &run,
                None,
                run.profile.clone().as_deref(),
                reason,
            );
        }
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
        let mut prepared = match self.prepare(caller, &usable.profile, &run, fds) {
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
        prepared.files = files;
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

    fn prepare(
        &self,
        caller: &Caller,
        profile: &Profile,
        run: &RunRequest,
        fds: Vec<OwnedFd>,
    ) -> Result<Prepared> {
        let Some(name) = run.argv.first() else {
            bail!("no command");
        };
        let program = self.config.programs.get(&caller.uid.to_string());
        let tool = sandbox::resolve_tool(
            name,
            &self.config.path,
            if program.is_some() {
                0
            } else {
                self.tool_owner
            },
        )?;
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
        if let Some(program) = program {
            // Absolute fixed argv inputs (notably an interpreter's signer script) are
            // installation inputs, never scripts supplied by the runner checkout.
            for word in program.command.iter().skip(1) {
                let path = Path::new(word);
                if path.is_absolute() {
                    sandbox::trusted_path(path, 0)?;
                }
            }
        }
        if let Some(tile) = caller.tile.as_deref() {
            // A tile's directories live in its own mount namespace; the sandbox binds a checkout
            // by the path the gateway's namespace gives a descriptor, which does not name them.
            // Until that is qualified (tiles design step 4), a tile's checkout is never bound.
            note = Some(format!(
                "running in the profile's home, without your directory: tile {tile}'s directories \
                 are not served yet"
            ));
            fds.clear();
        } else if program.is_some_and(|program| program.checkout.is_none()) {
            // Signing has no meaningful HOME fallback: it must see the authorized checkout.
            self.servable(&fds[0])?;
        } else if program.is_none()
            && let Err(reason) = self.servable(&fds[0])
        {
            note = Some(format!(
                "running in the profile's home, without your directory: {reason:#}"
            ));
            fds.clear();
        }
        let mut directories = Vec::new();
        let mount_roots = self
            .config
            .programs
            .values()
            .filter_map(|program| program.checkout.as_ref())
            .flat_map(|checkout| [checkout.backing_root.clone(), checkout.mount_root.clone()])
            .collect();
        for fd in fds {
            if let Some(checkout) = program.and_then(|program| program.checkout.as_ref()) {
                directories.push(program_checkout_directory(&fd, caller.uid, checkout)?);
                continue;
            }
            let path = sandbox::descriptor_path(&fd)?;
            sandbox::check_checkout_path(&path, &self.config.checkout_roots)?;
            reachable(&path)?;
            let metadata = fs::metadata(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
            if !metadata.is_dir() {
                bail!("{} is not a directory", path.display());
            }
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
            files: Vec::new(),
            stdio,
            directories,
            view,
            cwd,
            env,
            mount_roots,
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
        hidden.extend(prepared.mount_roots.iter().cloned());
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
        // The bound directories until bubblewrap mounts them, and the passed files for the command.
        let keep: Vec<RawFd> = prepared
            .directories
            .iter()
            .map(|d| d.fd.as_raw_fd())
            .chain(prepared.files.iter().map(|f| f.as_raw_fd()))
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

    /// One authorized API request: the caller's grant of the profile must allow `http METHOD
    /// github`, the URL must lie under the API base, and the caller brings no credential.
    fn authorized(
        &self,
        caller: &Caller,
        call: super::authorized::AuthorizedCall,
    ) -> Result<Reply> {
        use super::authorized::{GITHUB_TARGET, MAX_REQUEST_BODY, check_url, forwarded_headers};
        let refuse = |reason: String| Ok(Reply::Refused { reason, call: None });
        if self.config.programs.contains_key(&caller.uid.to_string()) {
            return refuse(
                "program callers are run-only; authorized HTTP is not a command grant".into(),
            );
        }
        let Some(principal) = caller.principal() else {
            return refuse(caller.require_person().err().unwrap_or_default());
        };
        let argv = vec![
            "http".to_owned(),
            call.method.to_ascii_uppercase(),
            GITHUB_TARGET.to_owned(),
        ];
        let usable = self
            .usable(caller)?
            .into_iter()
            .filter(|u| u.profile.id == call.profile)
            .collect::<Vec<_>>();
        if usable.is_empty() {
            return refuse(format!(
                "profile {} is not {principal}'s and no grant gives it to {principal}",
                call.profile
            ));
        }
        let mut reasons = Vec::new();
        let Some(usable) = usable.into_iter().find(|u| match u.judge(&argv) {
            Verdict::Allowed => true,
            Verdict::Refused(reason) => {
                reasons.push(reason);
                false
            }
        }) else {
            return refuse(reasons.join("; "));
        };
        let url = match check_url(&call.url, &self.config.github_api) {
            Ok(url) => url,
            Err(reason) => return refuse(reason),
        };
        let headers = match forwarded_headers(&call.headers) {
            Ok(headers) => headers,
            Err(reason) => return refuse(reason),
        };
        let body =
            match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &call.body) {
                Ok(body) if body.len() <= MAX_REQUEST_BODY => body,
                Ok(_) => {
                    return refuse(format!("the request body is over {MAX_REQUEST_BODY} bytes"));
                }
                Err(error) => return refuse(format!("the request body is not base64: {error}")),
            };
        let token = match self.api_token(&usable.profile) {
            Ok(token) => token,
            Err(error) => {
                return Ok(Reply::Error {
                    message: format!("{error:#}"),
                });
            }
        };
        let http = match self.http.get() {
            Some(http) => Arc::clone(http),
            None => {
                let http = super::authorized::Http::new()?;
                Arc::clone(self.http.get_or_init(|| http))
            }
        };
        let path = url.path().to_owned();
        let outcome = http.send(&call.method, url, &headers, &token, body);
        let detail = match &outcome {
            Ok(response) => {
                json!({ "method": call.method, "path": path, "status": response.status })
            }
            Err(error) => json!({ "method": call.method, "path": path, "error": error }),
        };
        self.log(
            caller,
            Some(&usable.profile.owner),
            "requested",
            Some(&usable.profile.id),
            detail,
        )?;
        match outcome {
            Ok(response) => {
                if response.status == 401 {
                    // The token was revoked or replaced: read it from the profile next time.
                    self.tokens
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&usable.profile.id);
                }
                Ok(Reply::Response {
                    status: response.status,
                    headers: response.headers,
                    body: base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        response.body,
                    ),
                })
            }
            Err(message) => Ok(Reply::Error { message }),
        }
    }

    /// A profile's API token: a value put into it (`GH_TOKEN`, `GITHUB_TOKEN`), else what its gh
    /// login holds, read by running gh as the sekrets user with the profile's home.
    fn api_token(&self, profile: &Profile) -> Result<String> {
        if let Some(token) = self
            .tokens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&profile.id)
        {
            return Ok(token.clone());
        }
        let env = self.store().env(&profile.id)?;
        let token = match env
            .iter()
            .find(|(name, _)| name == "GH_TOKEN" || name == "GITHUB_TOKEN")
        {
            Some((_, value)) => value.clone(),
            None => {
                let gh = sandbox::resolve_tool("gh", &self.config.path, self.tool_owner)?;
                let home = self.store().profile_home(&profile.id);
                let output = std::process::Command::new(gh)
                    .env_clear()
                    .env("HOME", &home)
                    .env("XDG_CONFIG_HOME", home.join(".config"))
                    .env(
                        "PATH",
                        std::env::join_paths(&self.config.path).unwrap_or_default(),
                    )
                    .args(["auth", "token", "--hostname", "github.com"])
                    .stdin(std::process::Stdio::null())
                    .output()
                    .context("read the profile's gh login")?;
                if !output.status.success() {
                    bail!(
                        "profile {} has no GitHub login: put GH_TOKEN into it or log it in with gh",
                        profile.id
                    );
                }
                String::from_utf8_lossy(&output.stdout).trim().to_owned()
            }
        };
        if token.is_empty() {
            bail!("profile {} has an empty GitHub token", profile.id);
        }
        self.tokens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(profile.id.clone(), token.clone());
        Ok(token)
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
        if self.config.programs.contains_key(&caller.uid.to_string()) {
            return Err(
                "program callers are run-only; management is reserved for a login-session person"
                    .into(),
            );
        }
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
                if !(to.starts_with("agent/")
                    || to.starts_with("person/")
                    || to.starts_with("host/"))
                    && !program_principal(&to)
                {
                    return Err(format!(
                        "grant to an agent, a person, a node's st or exact program/HOST/NAME, not `{to}`"
                    ));
                }
                if to == person {
                    return Err(format!("{person} owns {profile} already"));
                }
                if to.starts_with("program/")
                    && policy
                        .allow
                        .iter()
                        .any(|rule| rule.prefix.iter().any(|part| part == "*"))
                {
                    return Err(
                        "a program grant uses literal command prefixes, never wildcards".into(),
                    );
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

/// Resolve below an already opened directory, never following a caller's symlink. Every
/// component stays descriptor-relative even if the runner renames a parent during the walk.
fn open_checkout_beneath(root: &Path, relative: &Path, uid: u32) -> Result<OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt as _;
    #[cfg(target_os = "linux")]
    let access = libc::O_PATH;
    #[cfg(not(target_os = "linux"))]
    let access = libc::O_RDONLY;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        bail!("checkout must be strictly below its configured root");
    }
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(access | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)
        .context("open configured runner checkout root")?;
    if directory.metadata()?.uid() != uid {
        bail!("runner checkout root has the wrong owner");
    }
    let mut directory: OwnedFd = directory.into();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            unreachable!()
        };
        let name = std::ffi::CString::new(name.as_encoded_bytes())?;
        let next = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                access | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if next < 0 {
            return Err(io::Error::last_os_error())
                .context("open checkout component without following symlinks");
        }
        directory = unsafe { OwnedFd::from_raw_fd(next) };
    }
    Ok(directory)
}

fn program_checkout_directory(
    fd: &OwnedFd,
    uid: u32,
    checkout: &ProgramCheckout,
) -> Result<BoundDirectory> {
    // The backing root's parent is operator-controlled. Only the root itself belongs to the
    // runner; it cannot replace its parent or redirect this gateway-side anchor.
    let parent = checkout
        .backing_root
        .parent()
        .context("checkout root has no parent")?;
    if sandbox::trusted_path(parent, 0)? != parent {
        bail!("runner checkout parent must not be a symlink");
    }
    // This is an empty operator-created mount anchor, not the caller's private /w mount.
    if sandbox::trusted_path(&checkout.mount_root, 0)? != checkout.mount_root
        || !fs::metadata(&checkout.mount_root)?.is_dir()
    {
        bail!("program mount root must be an existing root-controlled directory");
    }
    let path = sandbox::descriptor_path(fd)?;
    let relative = path
        .strip_prefix(&checkout.mount_root)
        .or_else(|_| path.strip_prefix(&checkout.backing_root))
        .context("passed checkout is outside this runner's configured roots")?;
    let opened = open_checkout_beneath(&checkout.backing_root, relative, uid)?;
    verified_program_directory(fd, opened, checkout.mount_root.join(relative))
}

fn verified_program_directory(
    received: &OwnedFd,
    opened: OwnedFd,
    path: PathBuf,
) -> Result<BoundDirectory> {
    let expected = fs::metadata(format!("/proc/self/fd/{}", opened.as_raw_fd()))?;
    let received = fs::metadata(format!("/proc/self/fd/{}", received.as_raw_fd()))?;
    if !expected.is_dir()
        || !received.is_dir()
        || expected.dev() != received.dev()
        || expected.ino() != received.ino()
    {
        bail!("passed checkout does not match the configured runner backing directory");
    }
    // Bubblewrap resolves --ro-bind-fd through realpath before mounting it. A descriptor
    // received from the runner's private /w mount names a path absent in this namespace.
    // Keep the descriptor opened beneath the configured backing root instead, after proving
    // that it identifies exactly the same directory as the caller's descriptor. Bubblewrap
    // also compares the mounted inode to this retained descriptor, refusing replacement races.
    Ok(BoundDirectory { fd: opened, path })
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
    files: Vec<OwnedFd>,
    stdio: Vec<OwnedFd>,
    directories: Vec<BoundDirectory>,
    view: sandbox::CheckoutView,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    mount_roots: Vec<PathBuf>,
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

#[cfg(test)]
mod program_tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn program_checkout_directory_retains_verified_inode_after_path_replacement() {
        // Exercise the descriptor walk and inode match without assuming a root-owned
        // temporary parent: Nix sandboxes can own /tmp as the build uid. The VM controls
        // exercise program_checkout_directory's root-controlled configuration and mount.
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let uid = fs::metadata(root).unwrap().uid();
        fs::create_dir(root.join("repo")).unwrap();
        fs::write(root.join("repo/marker"), "verified").unwrap();
        let caller = open_checkout_beneath(root, Path::new("repo"), uid).unwrap();
        let opened = open_checkout_beneath(root, Path::new("repo"), uid).unwrap();
        let bound = verified_program_directory(&caller, opened, "/w/repo".into()).unwrap();
        assert_eq!(bound.path, Path::new("/w/repo"));
        assert_ne!(bound.fd.as_raw_fd(), caller.as_raw_fd());
        fs::rename(root.join("repo"), root.join("verified-original")).unwrap();
        fs::create_dir(root.join("repo")).unwrap();
        fs::write(root.join("repo/marker"), "replacement").unwrap();
        drop(caller);
        assert_eq!(
            fs::read_to_string(format!("/proc/self/fd/{}/marker", bound.fd.as_raw_fd())).unwrap(),
            "verified"
        );
        assert_ne!(
            fs::metadata(format!("/proc/self/fd/{}", bound.fd.as_raw_fd()))
                .unwrap()
                .ino(),
            fs::metadata(root.join("repo")).unwrap().ino()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn program_checkout_directory_refuses_wrong_inode_or_non_directory() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let uid = fs::metadata(root).unwrap().uid();
        for name in ["authorized", "other"] {
            fs::create_dir(root.join(name)).unwrap();
        }
        fs::write(root.join("file"), "not a directory").unwrap();
        for received in [
            open_checkout_beneath(root, Path::new("other"), uid).unwrap(),
            fs::File::open(root.join("file")).unwrap().into(),
        ] {
            let opened = open_checkout_beneath(root, Path::new("authorized"), uid).unwrap();
            let result = verified_program_directory(&received, opened, "/usr/repo".into());
            assert_eq!(
                result.err().unwrap().to_string(),
                "passed checkout does not match the configured runner backing directory"
            );
        }
    }

    struct TestKernel(String);
    impl Kernel for TestKernel {
        fn cgroup(&self, _: i32) -> Option<String> {
            Some(self.0.clone())
        }
        fn start(&self, _: i32) -> Option<u64> {
            Some(123)
        }
    }

    struct ChangingKernel(Arc<Mutex<(Option<u64>, String)>>);
    impl Kernel for ChangingKernel {
        fn cgroup(&self, _: i32) -> Option<String> {
            Some(self.0.lock().unwrap().1.clone())
        }
        fn start(&self, _: i32) -> Option<u64> {
            self.0.lock().unwrap().0
        }
    }

    fn config(root: &Path) -> GatewayConfig {
        toml::from_str(&format!(
            r#"
store = {root:?}
[people]
"1000" = "person/ada"
[programs."1001"]
principal = "program/ci1/evidence"
person = "person/ada"
unit = "ci-evidence.service"
command = ["evidence-sign"]
"#
        ))
        .unwrap()
    }

    fn program_tile_registry() -> Registry {
        Registry::parse(
            "uids = { first = 1001, last = 1001 }\n\
             [tiles.atlas]\nagent=\"agent/example/atlas\"\nperson=\"person/ada\"\n",
        )
        .unwrap()
        .with_uids("atlas = 1001\n")
        .unwrap()
    }

    #[test]
    fn program_and_tile_uid_collision_is_refused_with_readable_registry() {
        for cgroup in [
            "/system.slice/ci-evidence.service".to_owned(),
            crate::tile_registry::tools_cgroup("atlas"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let registry = program_tile_registry();
            let gateway = Gateway::new(
                config(root.path()),
                1000,
                None,
                Box::new(TestKernel(cgroup)),
            )
            .unwrap()
            .with_tiles(move || Ok(Some(registry.clone())));
            let (server, _client) = UnixStream::pair().unwrap();
            let error = gateway.connection_from_peer(&server, 7, 1001).unwrap_err();
            assert!(format!("{error:#}").contains("both a person"), "{error:#}");
        }
    }

    #[test]
    fn unreadable_registry_keeps_program_unit_identity_without_tile_privilege() {
        for (cgroup, expected_program) in [
            ("/system.slice/ci-evidence.service".to_owned(), true),
            (crate::tile_registry::tools_cgroup("atlas"), false),
        ] {
            let root = tempfile::tempdir().unwrap();
            let gateway = Gateway::new(
                config(root.path()),
                1000,
                None,
                Box::new(TestKernel(cgroup)),
            )
            .unwrap()
            .with_tiles(|| anyhow::bail!("fixture registry unreadable"));
            let (server, client) = UnixStream::pair().unwrap();
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let worker = std::thread::spawn(move || gateway.connection_from_peer(&server, 7, 1001));
            protocol::send(&client, &Request::Hello { attestation: None }, &[]).unwrap();
            let (reply, _) = protocol::recv::<Reply>(&client).unwrap().unwrap();
            let Reply::Hello { caller, .. } = reply else {
                panic!("expected Hello");
            };
            if expected_program {
                assert_eq!(
                    caller,
                    CallerView::Program {
                        program: "program/ci1/evidence".into(),
                        person: "person/ada".into(),
                    }
                );
            } else {
                assert!(
                    matches!(caller, CallerView::Unidentified { .. }),
                    "{caller:?}"
                );
            }
            drop(client);
            worker.join().unwrap().unwrap();
        }
    }

    #[test]
    fn tile_only_uid_is_refused_when_registry_is_unreadable() {
        let root = tempfile::tempdir().unwrap();
        let mut config = config(root.path());
        config.programs.clear();
        let gateway = Gateway::new(
            config,
            1000,
            None,
            Box::new(TestKernel(crate::tile_registry::tools_cgroup("atlas"))),
        )
        .unwrap()
        .with_tiles(|| anyhow::bail!("fixture registry unreadable"));
        let (server, _client) = UnixStream::pair().unwrap();
        let error = gateway.connection_from_peer(&server, 7, 1001).unwrap_err();
        let reason = format!("{error:#}");
        assert!(
            reason.contains("read root's tile registry")
                && reason.contains("fixture registry unreadable"),
            "{reason}"
        );
    }

    #[test]
    fn program_enrollment_rejects_ambiguous_uid_principal_unit_and_command() {
        let root = tempfile::tempdir().unwrap();
        let valid = config(root.path());
        valid.validate().unwrap();
        for (field, value) in [
            ("principal", "person/ada"),
            ("principal", "program/ci1/*"),
            ("person", "person/other"),
            ("unit", "user@1001.service/app.slice/seat.scope"),
            ("unit", "ci-evidence.service/child"),
            ("unit", ".service"),
            ("command", "/bin/sh"),
            ("command", "*"),
        ] {
            let mut candidate = valid.clone();
            let program = candidate.programs.get_mut("1001").unwrap();
            match field {
                "principal" => program.principal = value.into(),
                "person" => program.person = value.into(),
                "unit" => program.unit = value.into(),
                _ => program.command = vec![value.into()],
            }
            assert!(candidate.validate().is_err(), "{field}={value}");
        }
        for uid in ["0", "01001", "1000"] {
            let mut candidate = valid.clone();
            let program = candidate.programs.remove("1001").unwrap();
            candidate.programs.insert(uid.into(), program);
            assert!(candidate.validate().is_err(), "uid={uid}");
        }
        let mut duplicate = valid.clone();
        duplicate
            .programs
            .insert("1002".into(), duplicate.programs["1001"].clone());
        assert!(duplicate.validate().is_err());
    }

    #[test]
    fn program_identification_uses_uid_and_exact_system_unit_never_person_or_agent() {
        for (cgroup, allowed) in [
            ("/system.slice/ci-evidence.service", true),
            ("/system.slice/ci-evidence.service/child", true),
            ("/system.slice/ci-evidence.service-other", false),
            ("/system.slice/other.service", false),
            ("/user.slice/user-1001.slice/session-1.scope", false),
            (
                "/user.slice/user-1001.slice/user@1001.service/ci-evidence.service",
                false,
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let gateway = Gateway::new(
                config(root.path()),
                1000,
                None,
                Box::new(TestKernel(cgroup.into())),
            )
            .unwrap();
            let view = gateway.classify(1001, 7, "person/ada", None, "nonce");
            assert_eq!(
                matches!(view, CallerView::Program { .. }),
                allowed,
                "{cgroup}"
            );
            assert!(!matches!(
                view,
                CallerView::Person { .. } | CallerView::Agent { .. }
            ));
            assert!(matches!(
                gateway.classify(1002, 7, "person/ada", None, "nonce"),
                CallerView::Unidentified { .. }
            ));
        }
    }

    #[test]
    fn program_hello_to_run_refuses_exited_reused_or_moved_peer() {
        for (start, cgroup, expected) in [
            (
                None,
                "/system.slice/ci-evidence.service",
                "program process lifetime changed",
            ),
            (
                Some(124),
                "/system.slice/ci-evidence.service",
                "program process lifetime changed",
            ),
            (
                Some(123),
                "/system.slice/other.service",
                "not in enrolled system unit",
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let state = Arc::new(Mutex::new((
                Some(123),
                "/system.slice/ci-evidence.service".into(),
            )));
            let gateway = Gateway::new(
                config(root.path()),
                1000,
                None,
                Box::new(ChangingKernel(state.clone())),
            )
            .unwrap();
            let policy = Policy::build(&[], &["evidence-sign".into()], &[]).unwrap();
            gateway
                .store()
                .create_profile("ada/evidence", "person/ada", None, &policy, false)
                .unwrap();
            gateway
                .store()
                .add_grant(
                    "ada/evidence",
                    "program/ci1/evidence",
                    &policy,
                    None,
                    "person/ada",
                )
                .unwrap();
            let (server, client) = UnixStream::pair().unwrap();
            // Only the kernel identity input is simulated. The same production loop receives
            // the real Hello/Run frames and SCM_RIGHTS descriptors; no command is spawned.
            let worker = std::thread::spawn(move || gateway.connection_from_peer(&server, 7, 1001));
            protocol::send(&client, &Request::Hello { attestation: None }, &[]).unwrap();
            let (hello, _) = protocol::recv::<Reply>(&client).unwrap().unwrap();
            assert!(matches!(
                hello,
                Reply::Hello {
                    caller: CallerView::Program { .. },
                    ..
                }
            ));
            *state.lock().unwrap() = (start, cgroup.into());
            let stdio = fs::File::open("/dev/null").unwrap();
            let cwd = fs::File::open(root.path()).unwrap();
            let run = RunRequest {
                profile: Some("ada/evidence".into()),
                argv: vec!["evidence-sign".into()],
                directories: 1,
                ..Default::default()
            };
            protocol::send(
                &client,
                &Request::Run(run),
                &[
                    stdio.as_raw_fd(),
                    stdio.as_raw_fd(),
                    stdio.as_raw_fd(),
                    cwd.as_raw_fd(),
                ],
            )
            .unwrap();
            let (reply, _) = protocol::recv::<Reply>(&client).unwrap().unwrap();
            let Reply::Refused { reason, .. } = reply else {
                panic!("expected refusal, got {reply:?}")
            };
            assert!(reason.contains(expected), "{reason}");
            drop(client);
            worker.join().unwrap().unwrap();
        }
    }

    #[test]
    fn program_needs_a_narrow_grant_and_cannot_manage_login_or_choose_another_tool() {
        let root = tempfile::tempdir().unwrap();
        let gateway = Gateway::new(
            config(root.path()),
            1000,
            None,
            Box::new(TestKernel("/system.slice/ci-evidence.service".into())),
        )
        .unwrap();
        let caller = Caller {
            uid: 1001,
            pid: 7,
            pid_start: Some(123),
            person: "person/ada".into(),
            view: gateway.classify(1001, 7, "person/ada", None, "nonce"),
            tile: None,
        };
        let allow = Policy::build(&[], &["evidence-sign".into()], &[]).unwrap();
        gateway
            .store()
            .create_profile("ada/evidence", "person/ada", None, &allow, false)
            .unwrap();
        let request = RunRequest {
            profile: Some("ada/evidence".into()),
            argv: vec!["evidence-sign".into()],
            ..Default::default()
        };
        assert!(
            gateway.choose(&caller, &request).unwrap().is_err(),
            "ownership is not a grant"
        );
        let grant = gateway
            .store()
            .add_grant(
                "ada/evidence",
                "program/ci1/evidence",
                &allow,
                None,
                "person/ada",
            )
            .unwrap();
        assert!(gateway.choose(&caller, &request).unwrap().is_ok());
        let wrong = RunRequest {
            argv: vec!["sh".into()],
            ..request.clone()
        };
        assert!(gateway.choose(&caller, &wrong).unwrap().is_err());
        let extra = RunRequest {
            argv: vec![
                "evidence-sign".into(),
                "--config".into(),
                "/tmp/key.json".into(),
            ],
            ..request.clone()
        };
        assert!(gateway.choose(&caller, &extra).unwrap().is_err());
        let login = RunRequest {
            login: true,
            ..request.clone()
        };
        assert!(gateway.choose(&caller, &login).unwrap().is_err());
        for request in [
            Request::ProfileList,
            Request::GrantList,
            Request::Put {
                profile: "ada/evidence".into(),
                name: "KEY".into(),
                value: "replacement".into(),
            },
            Request::Register {
                node: "host/ci1".into(),
                key: "not-a-key".into(),
            },
            Request::Lock {
                person: None,
                reason: None,
            },
        ] {
            assert!(gateway.manage(&caller, request).is_err());
        }
        let person = Caller {
            uid: 1000,
            pid: 8,
            pid_start: Some(124),
            person: "person/ada".into(),
            view: CallerView::Person {
                person: "person/ada".into(),
            },
            tile: None,
        };
        assert!(
            gateway
                .manage(
                    &person,
                    Request::GrantAdd {
                        profile: "ada/evidence".into(),
                        to: "program/ci1/*".into(),
                        policy: allow.clone(),
                        until_unix_ms: None
                    }
                )
                .is_err()
        );
        assert!(
            gateway
                .manage(
                    &person,
                    Request::GrantAdd {
                        profile: "ada/evidence".into(),
                        to: "program/ci1/evidence".into(),
                        policy: Policy::build(&["everything".into()], &[], &[]).unwrap(),
                        until_unix_ms: None
                    }
                )
                .is_err()
        );
        gateway
            .store()
            .revoke_grant(&grant.id, "person/ada")
            .unwrap();
        assert!(gateway.choose(&caller, &request).unwrap().is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn program_checkout_walk_is_descriptor_relative_and_refuses_symlinks_and_other_owners() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("runner");
        fs::create_dir(&root).unwrap();
        fs::create_dir_all(root.join("smalltalk/repo")).unwrap();
        let uid = fs::metadata(&root).unwrap().uid();
        let fd = open_checkout_beneath(&root, Path::new("smalltalk/repo"), uid).unwrap();
        let actual = fs::metadata(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap();
        assert_eq!(
            actual.ino(),
            fs::metadata(root.join("smalltalk/repo")).unwrap().ino()
        );
        for path in [
            "",
            "..",
            "../runner/smalltalk",
            "/",
            "missing",
            "smalltalk/repo/../repo",
        ] {
            assert!(
                open_checkout_beneath(&root, Path::new(path), uid).is_err(),
                "{path}"
            );
        }
        assert!(
            open_checkout_beneath(&root, Path::new("smalltalk/repo"), uid.wrapping_add(1)).is_err()
        );
        let outside = fixture.path().join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, root.join("escape")).unwrap();
        symlink(root.join("smalltalk/repo"), root.join("inside-link")).unwrap();
        for path in ["escape", "inside-link"] {
            assert!(open_checkout_beneath(&root, Path::new(path), uid).is_err());
        }
        // A root symlink is refused too; checking only descendant components is insufficient.
        symlink(&root, fixture.path().join("root-link")).unwrap();
        assert!(
            open_checkout_beneath(
                &fixture.path().join("root-link"),
                Path::new("smalltalk/repo"),
                uid
            )
            .is_err()
        );
        fs::rename(root.join("smalltalk"), root.join("renamed")).unwrap();
        assert_eq!(
            fs::metadata(format!("/proc/self/fd/{}", fd.as_raw_fd()))
                .unwrap()
                .ino(),
            actual.ino()
        );
    }
}
