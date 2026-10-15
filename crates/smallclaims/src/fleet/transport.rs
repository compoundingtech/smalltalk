//! The transports inside the replication worker: Tailscale, Fabric, and loopback.
//!
//! The replication protocol authenticates but does not encrypt, so the worker binds only
//! loopback and tailnet addresses and dials only loopback, tailnet, and Fabric routes.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde_json::Value;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const MACOS_TAILSCALE: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";

/// Whether an address is in Tailscale's ranges: `100.64.0.0/10` or `fd7a:115c:a1e0::/48`.
pub fn is_tailnet_address(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let octets = address.octets();
            octets[0] == 100 && octets[1] & 0xc0 == 64
        }
        IpAddr::V6(address) => {
            let segments = address.segments();
            segments[0] == 0xfd7a && segments[1] == 0x115c && segments[2] == 0xa1e0
        }
    }
}

/// Whether an address may carry replication traffic: loopback or tailnet, never a LAN.
pub fn is_permitted_route_address(address: &IpAddr) -> bool {
    address.is_loopback() || is_tailnet_address(address)
}

/// The addresses of this machine's network interfaces.
pub fn local_addresses() -> BTreeSet<IpAddr> {
    let mut addresses = BTreeSet::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list` with a linked list that freeifaddrs releases below; each
    // entry's address pointer is either null or valid for its family.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return addresses;
        }
        let mut entry = list;
        while !entry.is_null() {
            let address = (*entry).ifa_addr;
            if !address.is_null() {
                match i32::from((*address).sa_family) {
                    libc::AF_INET => {
                        let address = &*(address as *const libc::sockaddr_in);
                        addresses.insert(IpAddr::from(
                            u32::from_be(address.sin_addr.s_addr).to_be_bytes(),
                        ));
                    }
                    libc::AF_INET6 => {
                        let address = &*(address as *const libc::sockaddr_in6);
                        addresses.insert(IpAddr::from(address.sin6_addr.s6_addr));
                    }
                    _ => {}
                }
            }
            entry = (*entry).ifa_next;
        }
        libc::freeifaddrs(list);
    }
    addresses
}

/// Parse `tailscale ip` output: one address per line.
pub fn parse_tailscale_ips(output: &str) -> Vec<IpAddr> {
    output
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

/// The tailnet addresses this machine can bind: reported by Tailscale, inside the tailnet
/// ranges, and present on a local interface. Userspace-networking Tailscale reports addresses
/// that no interface has, so it yields none.
pub fn bindable_tailnet_addresses(reported: &[IpAddr], local: &BTreeSet<IpAddr>) -> Vec<IpAddr> {
    reported
        .iter()
        .filter(|address| is_tailnet_address(address) && local.contains(address))
        .copied()
        .collect()
}

/// Find a transport tool: the `fleet.toml` override when set (tests always set it), otherwise
/// the login `PATH`, and for Tailscale on macOS the app bundle.
pub fn resolve_tool(override_path: Option<&Path>, name: &str) -> Option<PathBuf> {
    if let Some(path) = override_path {
        return Some(path.to_path_buf());
    }
    let from_path = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    });
    from_path.or_else(|| {
        (cfg!(target_os = "macos") && name == "tailscale" && Path::new(MACOS_TAILSCALE).is_file())
            .then(|| PathBuf::from(MACOS_TAILSCALE))
    })
}

#[derive(Debug)]
struct CommandFailure(String);

impl std::fmt::Display for CommandFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CommandFailure {}

/// Fabric answered the dial but this member has not granted the requested service, or does
/// not trust this machine at all. `code` is Fabric's own error code.
#[derive(Debug)]
pub struct FabricGrantRefusal {
    pub node: String,
    pub protocol: String,
    pub code: String,
}

impl std::fmt::Display for FabricGrantRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refused by that member's Fabric grants (node {}, service {}); retrying in about 30 minutes; replication can continue through other members",
            self.node, self.protocol
        )
    }
}

impl std::error::Error for FabricGrantRefusal {}

/// How Fabric classes a failure: trust or grants said no (`refusal`), it did not work this
/// time and another attempt or route may (`fault`), or Fabric rejected the arguments
/// (`usage`). Fabric assigns the class to each code and only adds codes, never changes a
/// class, so st keeps no code table and a code added later is still classified.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FabricErrorClass {
    Refusal,
    Fault,
    Usage,
}

/// A failure Fabric reported as `{"error": {"code", "class", "message", "peer", "service"}}`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FabricError {
    pub code: String,
    pub class: FabricErrorClass,
    pub message: String,
    pub peer: Option<String>,
    pub service: Option<String>,
}

impl std::fmt::Display for FabricError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "fabric {}: {}", self.code, self.message)
    }
}

impl std::error::Error for FabricError {}

impl FabricError {
    /// Read the error object Fabric prints on stdout under `--json`. `None` when stdout is
    /// not that object, which is what a Fabric build without `--json` leaves behind.
    fn parse(stdout: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(stdout.trim()).ok()?;
        let error = value.get("error")?;
        let text = |key: &str| {
            error
                .get(key)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        let class = match error.get("class")?.as_str()? {
            "refusal" => FabricErrorClass::Refusal,
            "usage" => FabricErrorClass::Usage,
            // A class added later is not a refusal, so it must not stop a retry.
            _ => FabricErrorClass::Fault,
        };
        Some(Self {
            code: text("code")?,
            class,
            message: text("message").unwrap_or_default(),
            peer: text("peer"),
            service: text("service"),
        })
    }
}

/// What a Fabric command left behind, before anyone decides what it means.
struct Finished {
    success: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

async fn run_raw(program: &Path, arguments: &[&str]) -> Result<Finished> {
    // Exec fails with "text file busy" while another process still holds the program open for
    // writing: Fabric replaced by `fabric update`, or a file a sibling fork has not yet let go
    // of. It clears within moments, so wait it out a few times instead of failing the call.
    let mut busy = 0;
    let output = loop {
        let attempt = tokio::time::timeout(
            COMMAND_TIMEOUT,
            tokio::process::Command::new(program)
                .args(arguments)
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .with_context(|| format!("{} {} timed out", program.display(), arguments.join(" ")))?;
        match attempt {
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy && busy < 5 => {
                busy += 1;
                tokio::time::sleep(Duration::from_millis(20 * busy)).await;
            }
            other => break other.with_context(|| format!("run {}", program.display()))?,
        }
    };
    Ok(Finished {
        success: output.status.success(),
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

fn failed(program: &Path, arguments: &[&str], finished: Finished) -> anyhow::Error {
    anyhow::Error::new(CommandFailure(finished.stderr)).context(format!(
        "{} {} failed",
        program.display(),
        arguments.join(" ")
    ))
}

/// Run a Fabric command with `--json`. `Ok(None)` means this Fabric build predates `--json`:
/// it rejected the option with a usage error (exit 2) and printed nothing on stdout, so the
/// caller takes the text path in `legacy`. Every other failure is reported as a
/// [`FabricError`] when Fabric printed one, and as an opaque command failure when it did not.
async fn run_json(program: &Path, arguments: &[&str]) -> Result<Option<Value>> {
    let mut with_flag = arguments.to_vec();
    with_flag.push("--json");
    let finished = run_raw(program, &with_flag).await?;
    if finished.success {
        let value = serde_json::from_str(finished.stdout.trim()).with_context(|| {
            format!(
                "{} {} printed no JSON",
                program.display(),
                with_flag.join(" ")
            )
        })?;
        return Ok(Some(value));
    }
    if let Some(error) = FabricError::parse(&finished.stdout) {
        return Err(anyhow::Error::new(error));
    }
    if finished.code == Some(2) && finished.stdout.trim().is_empty() {
        return Ok(None);
    }
    Err(failed(program, &with_flag, finished))
}

async fn run(program: &Path, arguments: &[&str]) -> Result<String> {
    let finished = run_raw(program, arguments).await?;
    if !finished.success {
        return Err(failed(program, arguments, finished));
    }
    Ok(finished.stdout)
}

/// Fabric builds older than 0.2.41 have no `--json` and no error codes: they print the
/// address, path, NodeID or tab-separated peer rows alone and say a refusal only in words on
/// stderr. This is the single place st still reads that text, and it turns a refusal into the
/// same [`FabricError`] a current build prints, so nothing downstream matches text.
///
/// Remove this module, and `Ok(None)` from [`run_json`], once no fleet member runs Fabric
/// older than 0.2.41. A member updates Fabric with `fabric update`, which is how a fleet
/// reaches that point.
mod legacy {
    use super::*;

    /// The old words and the code fabric's spec says to read them as.
    const REFUSALS: [(&str, &str); 2] = [
        (
            "peer not permitted for service",
            "peer_not_permitted_for_service",
        ),
        ("node is not in fabric allow-list", "peer_not_in_allow_list"),
    ];

    /// `fabric peers` rows: a NodeID, the local name (possibly empty), and the peer's grants.
    pub(super) fn parse_peers(printed: &str) -> Vec<(String, String)> {
        printed
            .lines()
            .filter_map(|line| {
                let mut columns = line.split('\t');
                let node = columns.next()?.trim();
                let name = columns.next().unwrap_or_default().trim();
                (!node.is_empty()).then(|| (node.to_owned(), name.to_owned()))
            })
            .collect()
    }

    /// The code the old words stand for, for a refusal said only in prose.
    pub(super) fn refusal_code(text: &str) -> Option<&'static str> {
        REFUSALS
            .iter()
            .find(|(words, _)| text.contains(words))
            .map(|(_, code)| *code)
    }

    pub(super) async fn run_dial(program: &Path, arguments: &[&str]) -> Result<String> {
        run(program, arguments).await.map_err(|error| {
            let Some(failure) = error.downcast_ref::<CommandFailure>() else {
                return error;
            };
            match refusal_code(&failure.0) {
                Some(code) => anyhow::Error::new(FabricError {
                    code: code.into(),
                    class: FabricErrorClass::Refusal,
                    message: failure.0.clone(),
                    peer: None,
                    service: None,
                }),
                None => error,
            }
        })
    }
}

/// The addresses `tailscale ip` reports.
pub async fn tailscale_addresses(tailscale: &Path) -> Result<Vec<IpAddr>> {
    Ok(parse_tailscale_ips(&run(tailscale, &["ip"]).await?))
}

/// The default Fabric protocol for a fleet, so a throwaway fleet never collides with another.
pub fn default_fabric_protocol(fleet_id: &str) -> String {
    format!("st3/fleet/{fleet_id}")
}

/// The `fabric` command, which talks to the local Fabric daemon.
#[derive(Clone, Debug)]
pub struct Fabric {
    program: PathBuf,
}

impl Fabric {
    pub fn new(program: PathBuf) -> Self {
        Self { program }
    }

    /// Local endpoint state, queried without contacting any peer. Address changes or a
    /// restarted Fabric endpoint wake replication independently of failed-peer timers.
    pub async fn addresses(&self) -> Result<BTreeSet<String>> {
        let value: Value = serde_json::from_str(&run(&self.program, &["addr"]).await?)?;
        let addresses = value["addrs"]
            .as_array()
            .context("fabric addr needs addrs")?;
        Ok(addresses.iter().map(Value::to_string).collect())
    }

    /// Fabric owns reconnecting this passive local event stream. Older builds lack the
    /// command; the worker then retains its address/suspend watcher and retry timers.
    pub fn peer_events(&self) -> Result<tokio::process::Child> {
        Ok(tokio::process::Command::new(&self.program)
            .args(["peer-events", "--watch"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?)
    }

    /// This machine's Fabric NodeID.
    pub async fn id(&self) -> Result<String> {
        let id = match run_json(&self.program, &["id"]).await? {
            Some(value) => value["id"].as_str().unwrap_or_default().to_owned(),
            None => run(&self.program, &["id"]).await?.trim().to_owned(),
        };
        anyhow::ensure!(!id.is_empty(), "fabric id printed nothing");
        Ok(id)
    }

    /// Persist a loopback listener exposure so Fabric restores it after a restart.
    /// Repeating it is harmless; fleet leave and uninstall remove it explicitly.
    pub async fn expose(&self, protocol: &str, address: &str) -> Result<()> {
        run(&self.program, &["expose", protocol, "--tcp", address])
            .await
            .map(|_| ())
    }

    pub async fn unexpose(&self, protocol: &str) -> Result<()> {
        run(&self.program, &["unexpose", protocol])
            .await
            .map(|_| ())
    }

    /// Expose `argv` to trusted peers under `protocol`: Fabric runs it once for each incoming
    /// tunnel, with the tunnel on its stdin and stdout. Fabric keeps the exposure in its own
    /// configuration, so it outlives this process and any st daemon.
    pub async fn expose_exec(&self, protocol: &str, argv: &[&str]) -> Result<()> {
        let mut arguments = vec!["expose", protocol, "--exec", "--"];
        arguments.extend_from_slice(argv);
        run(&self.program, &arguments).await.map(|_| ())
    }

    /// Expose the Unix socket at `socket` to trusted peers under `protocol`: Fabric opens one
    /// connection to it for each incoming tunnel and forwards bytes both ways. Fabric keeps the
    /// exposure in its own configuration, so it outlives this process, and repeating it replaces
    /// any earlier exposure of `protocol`.
    pub async fn expose_socket(&self, protocol: &str, socket: &Path) -> Result<()> {
        let socket = socket.to_str().context("the socket path is not UTF-8")?;
        run(&self.program, &["expose", protocol, "--socket", socket])
            .await
            .map(|_| ())
    }

    /// The trusted peers in Fabric's `peers.toml`, as NodeID and local name.
    pub async fn peers(&self) -> Result<Vec<(String, String)>> {
        let Some(value) = run_json(&self.program, &["peers"]).await? else {
            return Ok(legacy::parse_peers(&run(&self.program, &["peers"]).await?));
        };
        let peers = value["peers"]
            .as_array()
            .context("fabric peers printed no peers")?;
        Ok(peers
            .iter()
            .filter_map(|peer| {
                let node = peer["node"].as_str().filter(|node| !node.is_empty())?;
                Some((
                    node.to_owned(),
                    peer["name"].as_str().unwrap_or_default().to_owned(),
                ))
            })
            .collect())
    }

    /// Whether the peer's Fabric refuses this machine for `protocol`, asked with a fresh
    /// `fabric probe --json`. `dial` never connects, so a refusal reaches st only as a tunnel
    /// that closes; the probe is what names it. `None` when the probe was answered by anything
    /// else (supported, a fault, a timeout, an unknown peer) or could not be run.
    pub async fn refusal(&self, node: &str, protocol: &str) -> Option<FabricGrantRefusal> {
        let finished = run_raw(&self.program, &["probe", node, protocol, "--json"])
            .await
            .ok()?;
        let answer: Value = serde_json::from_str(finished.stdout.trim()).ok()?;
        // Fabric 0.2.41 puts `code` and `class` beside `outcome`; older builds only the
        // words in `error`.
        let code = match answer.get("class").and_then(Value::as_str) {
            Some("refusal") => answer.get("code")?.as_str()?.to_owned(),
            Some(_) => return None,
            None => legacy::refusal_code(answer.get("error")?.as_str()?)?.to_owned(),
        };
        Some(FabricGrantRefusal {
            node: node.into(),
            protocol: protocol.into(),
            code,
        })
    }

    /// Ask Fabric for a local Unix socket that tunnels to a peer's exposed protocol and return
    /// its path. Fabric reuses the socket while its listener lives.
    pub async fn dial_socket(&self, node: &str, protocol: &str) -> Result<PathBuf> {
        let arguments = ["dial", node, protocol];
        let printed = match run_json(&self.program, &arguments).await? {
            Some(value) => value["socket"]
                .as_str()
                .context("fabric dial printed no socket")?
                .to_owned(),
            None => legacy::run_dial(&self.program, &arguments)
                .await?
                .trim()
                .to_owned(),
        };
        let path = PathBuf::from(&printed);
        anyhow::ensure!(path.is_absolute(), "fabric dial printed `{printed}`");
        Ok(path)
    }

    /// Ask Fabric for a loopback TCP tunnel to a peer's exposed protocol and return its
    /// address. Fabric reuses the tunnel while it lives, so repeating this is cheap. A
    /// refusal, by grant or by trust, is a [`FabricGrantRefusal`]; any other Fabric failure
    /// is a [`FabricError`].
    pub async fn dial(&self, node: &str, protocol: &str) -> Result<SocketAddr> {
        let arguments = ["dial", node, protocol, "--tcp", "127.0.0.1:0"];
        let printed = match run_json(&self.program, &arguments).await {
            Ok(Some(value)) => Ok(value["address"]
                .as_str()
                .context("fabric dial printed no address")?
                .to_owned()),
            Ok(None) => legacy::run_dial(&self.program, &arguments)
                .await
                .map(|printed| printed.trim().to_owned()),
            Err(error) => Err(error),
        }
        .map_err(|error| match error.downcast_ref::<FabricError>() {
            Some(fabric) if fabric.class == FabricErrorClass::Refusal => FabricGrantRefusal {
                node: node.into(),
                protocol: protocol.into(),
                code: fabric.code.clone(),
            }
            .into(),
            _ => error,
        })?;
        let address: SocketAddr = printed
            .parse()
            .with_context(|| format!("fabric dial printed `{printed}`"))?;
        anyhow::ensure!(
            address.ip().is_loopback(),
            "fabric dial returned a non-loopback address"
        );
        Ok(address)
    }
}

/// Where one peer can be reached from this machine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Route {
    /// A loopback or tailnet URL.
    Http(String),
    /// A Fabric peer and protocol; the worker dials a local tunnel for it.
    Fabric { node: String, protocol: String },
}

impl Route {
    /// Classify the actual dial route, including config overrides. A Fabric tunnel's
    /// ephemeral loopback URL must never be classified as a loopback carrier.
    pub fn carrier(&self) -> crate::replication::ReplicationCarrier {
        use crate::replication::ReplicationCarrier;
        match self {
            Self::Fabric { .. } => ReplicationCarrier::Fabric,
            Self::Http(url) => {
                let host = reqwest::Url::parse(url).ok().and_then(|url| {
                    url.host_str()
                        .map(|host| host.trim_matches(['[', ']']).to_owned())
                });
                match host.as_deref() {
                    Some(host) if host.eq_ignore_ascii_case("localhost") => {
                        ReplicationCarrier::Loopback
                    }
                    Some(host) => match host.parse::<IpAddr>() {
                        Ok(address) if address.is_loopback() => ReplicationCarrier::Loopback,
                        Ok(address) if is_tailnet_address(&address) => {
                            ReplicationCarrier::Tailscale
                        }
                        _ => ReplicationCarrier::Unknown,
                    },
                    None => ReplicationCarrier::Unknown,
                }
            }
        }
    }
}

/// A lasting peer route. HTTP stays on loopback or the encrypted tailnet; Fabric names
/// the remote peer/protocol, never an ephemeral local tunnel address.
pub fn parse_route(route: &str) -> Option<Route> {
    if let Some(rest) = route.strip_prefix("fabric://") {
        let (node, protocol) = rest.split_once('/')?;
        if node.is_empty()
            || protocol.is_empty()
            || rest
                .chars()
                .any(|character| character.is_whitespace() || matches!(character, '?' | '#' | '@'))
        {
            return None;
        }
        return Some(Route::Fabric {
            node: node.into(),
            protocol: protocol.into(),
        });
    }
    let url = reqwest::Url::parse(route).ok()?;
    let host = url.host_str()?.trim_matches(['[', ']']);
    (url.scheme() == "http"
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && (host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| is_permitted_route_address(&address))))
    .then(|| Route::Http(route.trim_end_matches('/').into()))
}

/// What this machine can use to dial.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LocalTransports {
    pub tailscale: bool,
    pub fabric: bool,
}

/// A member's routes from its advertised endpoints, in the design's order after any config
/// override: Tailscale, then Fabric, then loopback. Endpoints this machine cannot use, and any
/// address that is neither loopback nor tailnet, are skipped.
pub fn routes_from_endpoints(endpoints: &[Value], local: LocalTransports) -> Vec<Route> {
    let text = |endpoint: &Value, field: &str| endpoint[field].as_str().map(str::to_owned);
    let address = |endpoint: &Value| {
        text(endpoint, "address")
            .and_then(|address| address.parse::<SocketAddr>().ok())
            .filter(|address| is_permitted_route_address(&address.ip()))
            .map(|address| Route::Http(format!("http://{address}")))
    };
    let mut routes = Vec::new();
    if local.tailscale {
        routes.extend(
            endpoints
                .iter()
                .filter(|endpoint| endpoint["transport"] == "tailscale")
                .filter_map(address)
                .filter(|route| match route {
                    Route::Http(url) => url
                        .trim_start_matches("http://")
                        .parse::<SocketAddr>()
                        .is_ok_and(|address| is_tailnet_address(&address.ip())),
                    Route::Fabric { .. } => false,
                }),
        );
    }
    if local.fabric {
        routes.extend(
            endpoints
                .iter()
                .filter(|endpoint| endpoint["transport"] == "fabric")
                .filter_map(|endpoint| {
                    Some(Route::Fabric {
                        node: text(endpoint, "node")?,
                        protocol: text(endpoint, "protocol")?,
                    })
                }),
        );
    }
    routes.extend(
        endpoints
            .iter()
            .filter(|endpoint| endpoint["transport"] == "loopback")
            .filter_map(address)
            .filter(|route| match route {
                Route::Http(url) => url
                    .trim_start_matches("http://")
                    .parse::<SocketAddr>()
                    .is_ok_and(|address| address.ip().is_loopback()),
                Route::Fabric { .. } => false,
            }),
    );
    routes
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn carrier_comes_from_the_dial_route_including_overrides() {
        use crate::replication::ReplicationCarrier;
        for (route, carrier) in [
            ("http://127.0.0.1:4000", ReplicationCarrier::Loopback),
            ("http://[::1]:4000", ReplicationCarrier::Loopback),
            ("http://localhost:4000", ReplicationCarrier::Loopback),
            ("http://100.101.102.103:4000", ReplicationCarrier::Tailscale),
            (
                "http://[fd7a:115c:a1e0::1]:4000",
                ReplicationCarrier::Tailscale,
            ),
            ("fabric://birch/st3-peer-v1", ReplicationCarrier::Fabric),
        ] {
            assert_eq!(parse_route(route).unwrap().carrier(), carrier, "{route}");
        }
        assert_eq!(
            Route::Http("http://example.invalid:4000".into()).carrier(),
            ReplicationCarrier::Unknown
        );
        assert_eq!(
            Route::Http("malformed".into()).carrier(),
            ReplicationCarrier::Unknown
        );
    }

    #[test]
    fn tailnet_ranges_are_exact() {
        for inside in ["100.64.0.1", "100.127.255.254", "fd7a:115c:a1e0::1"] {
            assert!(is_tailnet_address(&inside.parse().unwrap()), "{inside}");
        }
        for outside in [
            "100.63.255.255",
            "100.128.0.1",
            "192.168.1.10",
            "10.0.0.1",
            "fd7a:115c:a1e1::1",
            "127.0.0.1",
        ] {
            assert!(!is_tailnet_address(&outside.parse().unwrap()), "{outside}");
        }
    }

    #[test]
    fn the_tailnet_listener_binds_only_tailscale_addresses_that_an_interface_has() {
        let reported = parse_tailscale_ips("100.101.102.103\nfd7a:115c:a1e0::5\n192.168.1.4\n");
        assert_eq!(reported.len(), 3);
        let local = BTreeSet::from([
            "100.101.102.103".parse().unwrap(),
            "192.168.1.4".parse().unwrap(),
        ]);
        assert_eq!(
            bindable_tailnet_addresses(&reported, &local),
            vec!["100.101.102.103".parse::<IpAddr>().unwrap()]
        );
        assert!(local_addresses().iter().any(IpAddr::is_loopback));
    }

    #[test]
    fn routes_are_tailscale_then_fabric_then_loopback_and_never_a_lan() {
        let endpoints = [
            json!({"transport": "loopback", "address": "127.0.0.1:4000"}),
            json!({"transport": "fabric", "node": "abc", "protocol": "st3/fleet/x"}),
            json!({"transport": "tailscale", "address": "100.101.102.103:31313"}),
            json!({"transport": "tailscale", "address": "192.168.1.4:31313"}),
            json!({"transport": "loopback", "address": "192.168.1.4:4000"}),
        ];
        let all = LocalTransports {
            tailscale: true,
            fabric: true,
        };
        assert_eq!(
            routes_from_endpoints(&endpoints, all),
            vec![
                Route::Http("http://100.101.102.103:31313".into()),
                Route::Fabric {
                    node: "abc".into(),
                    protocol: "st3/fleet/x".into()
                },
                Route::Http("http://127.0.0.1:4000".into()),
            ]
        );
        assert_eq!(
            routes_from_endpoints(&endpoints, LocalTransports::default()),
            vec![Route::Http("http://127.0.0.1:4000".into())]
        );
    }

    fn shim(root: &Path, body: &str) -> PathBuf {
        let path = root.join("fabric");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn the_fabric_route_dials_through_the_cli_and_refuses_a_non_loopback_tunnel() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("calls");
        let fabric = Fabric::new(shim(
            root.path(),
            &format!(
                "echo \"$@\" >> {log}\ncase \"$1\" in id) echo '{{\"id\":\"node-a\"}}';; dial) echo '{{\"address\":\"127.0.0.1:45678\"}}';; *) ;; esac",
                log = log.display()
            ),
        ));
        assert_eq!(fabric.id().await.unwrap(), "node-a");
        assert_eq!(
            fabric.dial("node-b", "st3/fleet/x").await.unwrap(),
            "127.0.0.1:45678".parse().unwrap()
        );
        fabric
            .expose("st3/fleet/x", "127.0.0.1:31313")
            .await
            .unwrap();
        fabric.unexpose("st3/fleet/x").await.unwrap();
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            calls,
            "id --json\ndial node-b st3/fleet/x --tcp 127.0.0.1:0 --json\n\
             expose st3/fleet/x --tcp 127.0.0.1:31313\nunexpose st3/fleet/x\n"
        );

        let remote = Fabric::new(shim(
            root.path(),
            "echo '{\"address\":\"192.168.1.4:45678\"}'",
        ));
        assert!(remote.dial("node-b", "p").await.is_err());
    }

    /// A current Fabric: the error object on stdout, nothing parsed from stderr.
    fn fails_with(root: &Path, code: &str, class: &str, status: u8) -> Fabric {
        Fabric::new(shim(
            root,
            &format!(
                "echo '{{\"error\":{{\"code\":\"{code}\",\"class\":\"{class}\",\
                 \"message\":\"a message\",\"peer\":null,\"service\":null}}}}'\n\
                 echo 'peer not permitted for service st3-peer-v1' >&2; exit {status}"
            ),
        ))
    }

    #[tokio::test]
    async fn every_fabric_refusal_code_is_a_grant_refusal() {
        // The last code is invented: Fabric adds codes, and its class is what st reads.
        for code in [
            "peer_not_permitted_for_service",
            "peer_not_in_allow_list",
            "invented_future_refusal",
        ] {
            let root = tempfile::tempdir().unwrap();
            let error = fails_with(root.path(), code, "refusal", 77)
                .dial("invented-leaf", "st3-peer-v1")
                .await
                .unwrap_err();
            let refusal = error
                .downcast_ref::<FabricGrantRefusal>()
                .unwrap_or_else(|| panic!("{code}: {error:#}"));
            assert_eq!(refusal.code, code);
            assert_eq!(refusal.node, "invented-leaf");
            assert!(error.to_string().contains("that member's Fabric grants"));
        }
    }

    #[tokio::test]
    async fn every_fabric_fault_code_is_a_fabric_error_and_not_a_refusal() {
        // The shim's stderr carries the old refusal words on purpose: with `--json` they are
        // free text for people, and the class decides.
        for (code, class, expected) in [
            ("unknown_peer", "fault", FabricErrorClass::Fault),
            ("peer_offline", "fault", FabricErrorClass::Fault),
            ("unreachable", "fault", FabricErrorClass::Fault),
            ("timeout", "fault", FabricErrorClass::Fault),
            ("daemon_not_running", "fault", FabricErrorClass::Fault),
            ("service_not_exposed", "fault", FabricErrorClass::Fault),
            ("service_unavailable", "fault", FabricErrorClass::Fault),
            ("session_limit_reached", "fault", FabricErrorClass::Fault),
            ("session_expired", "fault", FabricErrorClass::Fault),
            ("spawn_failed", "fault", FabricErrorClass::Fault),
            ("internal", "fault", FabricErrorClass::Fault),
            ("invalid_argument", "usage", FabricErrorClass::Usage),
            (
                "invented_future_code",
                "invented-class",
                FabricErrorClass::Fault,
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let status = if class == "usage" { 2 } else { 69 };
            let error = fails_with(root.path(), code, class, status)
                .dial("invented-leaf", "st3-peer-v1")
                .await
                .unwrap_err();
            assert!(!error.is::<FabricGrantRefusal>(), "{code}");
            let fault = error
                .downcast_ref::<FabricError>()
                .unwrap_or_else(|| panic!("{code}: {error:#}"));
            assert_eq!((fault.code.as_str(), fault.class), (code, expected));
            assert_eq!(fault.message, "a message");
        }
    }

    #[test]
    fn the_error_object_carries_the_peer_and_service_when_fabric_names_them() {
        let printed = r#"{"error":{"code":"peer_offline","class":"fault","message":"m","peer":"invented-leaf","service":"st3-peer-v1"}}"#;
        let error = FabricError::parse(&format!("{printed}\n")).unwrap();
        assert_eq!(error.peer.as_deref(), Some("invented-leaf"));
        assert_eq!(error.service.as_deref(), Some("st3-peer-v1"));
        for not_an_error in [
            "",
            "127.0.0.1:1",
            r#"{"address":"127.0.0.1:1"}"#,
            r#"{"error":"x"}"#,
        ] {
            assert_eq!(FabricError::parse(not_an_error), None, "{not_an_error}");
        }
    }

    #[tokio::test]
    async fn a_failure_with_no_error_object_is_an_opaque_command_failure() {
        let root = tempfile::tempdir().unwrap();
        let crashed = Fabric::new(shim(root.path(), "echo 'boom' >&2; exit 1"));
        let error = crashed.dial("invented-leaf", "p").await.unwrap_err();
        assert!(error.downcast_ref::<FabricError>().is_none(), "{error:#}");
        assert!(!error.is::<FabricGrantRefusal>());
        assert!(error.downcast_ref::<CommandFailure>().is_some());
    }

    /// A Fabric older than 0.2.41 rejects `--json` with a usage error and prints the
    /// address alone; it says a refusal only in words.
    fn old_build(root: &Path, dial: &str) -> Fabric {
        Fabric::new(shim(
            root,
            &format!(
                "case \" $* \" in *' --json '*) echo \"error: unexpected argument '--json'\" >&2; exit 2;; esac\n{dial}"
            ),
        ))
    }

    #[tokio::test]
    async fn an_older_fabric_without_json_still_names_itself_and_lists_its_peers() {
        let root = tempfile::tempdir().unwrap();
        let fabric = old_build(
            root.path(),
            "case \"$1\" in id) echo node-a;; \
             peers) printf 'node-b\\tBox\\techo,st3/pty/x\\nnode-c\\t\\techo\\n';; esac",
        );
        assert_eq!(fabric.id().await.unwrap(), "node-a");
        assert_eq!(
            fabric.peers().await.unwrap(),
            vec![
                ("node-b".to_owned(), "Box".to_owned()),
                ("node-c".to_owned(), String::new()),
            ]
        );
    }

    #[tokio::test]
    async fn id_and_peers_report_fabric_failures_by_code() {
        let root = tempfile::tempdir().unwrap();
        let down = fails_with(root.path(), "daemon_not_running", "fault", 69);
        for error in [
            down.id().await.unwrap_err(),
            down.peers().await.unwrap_err(),
        ] {
            let fault = error.downcast_ref::<FabricError>().unwrap();
            assert_eq!(fault.code, "daemon_not_running");
        }
        let root = tempfile::tempdir().unwrap();
        let empty = Fabric::new(shim(root.path(), "echo '{\"id\":\"\"}'"));
        assert!(empty.id().await.is_err());
    }

    #[tokio::test]
    async fn an_older_fabric_without_json_still_dials_and_still_tells_a_refusal_from_a_fault() {
        let root = tempfile::tempdir().unwrap();
        let works = old_build(root.path(), "echo 127.0.0.1:45678");
        assert_eq!(
            works.dial("invented-leaf", "p").await.unwrap(),
            "127.0.0.1:45678".parse().unwrap()
        );
        for (words, code) in [
            (
                "peer not permitted for service st3-peer-v1",
                "peer_not_permitted_for_service",
            ),
            ("node is not in fabric allow-list", "peer_not_in_allow_list"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let refused = old_build(root.path(), &format!("echo '{words}' >&2; exit 1"));
            let error = refused
                .dial("invented-leaf", "st3-peer-v1")
                .await
                .unwrap_err();
            let refusal = error.downcast_ref::<FabricGrantRefusal>().unwrap();
            assert_eq!(refusal.code, code);
        }
        let root = tempfile::tempdir().unwrap();
        let away = old_build(root.path(), "echo 'peer offline' >&2; exit 1");
        assert!(
            !away
                .dial("invented-leaf", "st3-peer-v1")
                .await
                .unwrap_err()
                .is::<FabricGrantRefusal>()
        );
    }

    /// A shim answering `probe` with `stdout` and `status`, and failing any other command.
    fn probes_with(root: &Path, stdout: &str, status: u8) -> Fabric {
        Fabric::new(shim(
            root,
            &format!("case \"$1\" in probe) echo '{stdout}'; exit {status};; *) exit 1;; esac"),
        ))
    }

    fn answer(outcome: &str, code: &str, class: &str, error: &str) -> String {
        let quoted = |text: &str| {
            if text.is_empty() {
                "null".to_owned()
            } else {
                format!("\"{text}\"")
            }
        };
        format!(
            "{{\"peer\":\"invented-leaf\",\"peer_id\":\"abc\",\"protocol\":\"st3-peer-v1\",\
             \"outcome\":\"{outcome}\",\"round_trip_micros\":null,\"transport\":null,\
             \"error\":{},\"code\":{},\"class\":{}}}",
            quoted(error),
            quoted(code),
            quoted(class)
        )
    }

    #[tokio::test]
    async fn a_probe_names_a_refusal_by_its_code_and_nothing_else_is_one() {
        for (stdout, status, expected) in [
            (answer("unreachable", "peer_not_permitted_for_service", "refusal", "x"), 2, Some("peer_not_permitted_for_service")),
            (answer("unreachable", "peer_not_in_allow_list", "refusal", "x"), 2, Some("peer_not_in_allow_list")),
            (answer("unreachable", "invented_future_refusal", "refusal", "x"), 2, Some("invented_future_refusal")),
            (answer("supported", "", "", ""), 0, None),
            (answer("timeout", "timeout", "fault", "x"), 3, None),
            (answer("unsupported", "service_not_exposed", "fault", "x"), 1, None),
            // The prose is a refusal's words, but the code says fault, so the class decides.
            (answer("unreachable", "unreachable", "fault", "peer not permitted for service"), 2, None),
            (r#"{"error":{"code":"unknown_peer","class":"fault","message":"m","peer":"ghost","service":null}}"#.to_owned(), 64, None),
            (String::new(), 64, None),
            ("not json".to_owned(), 0, None),
        ] {
            let root = tempfile::tempdir().unwrap();
            let found = probes_with(root.path(), &stdout, status)
                .refusal("invented-leaf", "st3-peer-v1")
                .await;
            assert_eq!(
                found.as_ref().map(|refusal| refusal.code.as_str()),
                expected,
                "{stdout}"
            );
            if let Some(refusal) = found {
                assert_eq!((refusal.node.as_str(), refusal.protocol.as_str()), ("invented-leaf", "st3-peer-v1"));
            }
        }
    }

    #[tokio::test]
    async fn a_probe_from_an_older_fabric_is_read_by_its_words() {
        for (error, expected) in [
            (
                "peer not permitted for service \\\"st3-peer-v1\\\"",
                Some("peer_not_permitted_for_service"),
            ),
            (
                "node is not in fabric allow-list",
                Some("peer_not_in_allow_list"),
            ),
            ("connection reset", None),
        ] {
            let root = tempfile::tempdir().unwrap();
            // No `code` or `class` keys at all, as before Fabric 0.2.41.
            let stdout =
                format!(r#"{{"peer":"invented-leaf","outcome":"unreachable","error":"{error}"}}"#);
            let found = probes_with(root.path(), &stdout, 2)
                .refusal("invented-leaf", "st3-peer-v1")
                .await;
            assert_eq!(
                found.as_ref().map(|refusal| refusal.code.as_str()),
                expected,
                "{error}"
            );
        }
        // A Fabric with no `probe` at all is not a refusal either.
        let root = tempfile::tempdir().unwrap();
        let none = Fabric::new(shim(root.path(), "exit 2"));
        assert!(none.refusal("invented-leaf", "p").await.is_none());
    }

    #[tokio::test]
    async fn a_pty_route_exposes_a_socket_and_dials_a_unix_socket() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("calls");
        let fabric = Fabric::new(shim(
            root.path(),
            &format!(
                "echo \"$@\" >> {log}\ncase \"$1\" in \
                 peers) echo '{{\"peers\":[{{\"node\":\"node-b\",\"name\":\"Box\",\"allow\":[\"echo\",\"st3/pty/x\"]}},{{\"node\":\"node-c\",\"name\":null,\"allow\":[\"echo\"]}}]}}';; \
                 dial) echo '{{\"socket\":\"/run/fabric/dials/node-b.sock\"}}';; *) ;; esac",
                log = log.display()
            ),
        ));
        fabric
            .expose_socket("st3/pty/x", Path::new("/run/st/terminals.sock"))
            .await
            .unwrap();
        fabric
            .expose_exec("st3/pty/x", &["/bin/st", "terminals", "serve-fabric"])
            .await
            .unwrap();
        assert_eq!(
            fabric.peers().await.unwrap(),
            vec![
                ("node-b".to_owned(), "Box".to_owned()),
                ("node-c".to_owned(), String::new()),
            ]
        );
        assert_eq!(
            fabric.dial_socket("node-b", "st3/pty/x").await.unwrap(),
            PathBuf::from("/run/fabric/dials/node-b.sock")
        );
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            calls,
            "expose st3/pty/x --socket /run/st/terminals.sock\n\
             expose st3/pty/x --exec -- /bin/st terminals serve-fabric\npeers --json\n\
             dial node-b st3/pty/x --json\n"
        );

        let relative = Fabric::new(shim(
            root.path(),
            "echo '{\"socket\":\"dials/node-b.sock\"}'",
        ));
        assert!(relative.dial_socket("node-b", "p").await.is_err());
    }

    #[test]
    fn an_override_is_used_without_searching_path() {
        let override_path = Path::new("/nonexistent/shim/fabric");
        assert_eq!(
            resolve_tool(Some(override_path), "fabric"),
            Some(override_path.to_path_buf())
        );
    }
}
