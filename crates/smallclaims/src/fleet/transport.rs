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

/// Fabric answered the dial but this member has not granted the requested service.
#[derive(Debug)]
pub struct FabricGrantRefusal {
    pub node: String,
    pub protocol: String,
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

async fn run(program: &Path, arguments: &[&str]) -> Result<String> {
    let output = tokio::time::timeout(
        COMMAND_TIMEOUT,
        tokio::process::Command::new(program)
            .args(arguments)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .with_context(|| format!("{} {} timed out", program.display(), arguments.join(" ")))?
    .with_context(|| format!("run {}", program.display()))?;
    if !output.status.success() {
        return Err(anyhow::Error::new(CommandFailure(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
        .context(format!(
            "{} {} failed",
            program.display(),
            arguments.join(" ")
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The addresses `tailscale ip` reports.
pub async fn tailscale_addresses(tailscale: &Path) -> Result<Vec<IpAddr>> {
    Ok(parse_tailscale_ips(&run(tailscale, &["ip"]).await?))
}

/// `fabric peers` rows: a NodeID, the local name (possibly empty), and the peer's grants.
fn parse_fabric_peers(printed: &str) -> Vec<(String, String)> {
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
        let id = run(&self.program, &["id"]).await?.trim().to_owned();
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

    /// The trusted peers in Fabric's `peers.toml`, as NodeID and local name.
    pub async fn peers(&self) -> Result<Vec<(String, String)>> {
        Ok(parse_fabric_peers(&run(&self.program, &["peers"]).await?))
    }

    /// Ask Fabric for a local Unix socket that tunnels to a peer's exposed protocol and return
    /// its path. Fabric reuses the socket while its listener lives.
    pub async fn dial_socket(&self, node: &str, protocol: &str) -> Result<PathBuf> {
        let printed = run(&self.program, &["dial", node, protocol]).await?;
        let path = PathBuf::from(printed.trim());
        anyhow::ensure!(
            path.is_absolute(),
            "fabric dial printed `{}`",
            printed.trim()
        );
        Ok(path)
    }

    /// Ask Fabric for a loopback TCP tunnel to a peer's exposed protocol and return its
    /// address. Fabric reuses the tunnel while it lives, so repeating this is cheap.
    pub async fn dial(&self, node: &str, protocol: &str) -> Result<SocketAddr> {
        let printed = run(
            &self.program,
            &["dial", node, protocol, "--tcp", "127.0.0.1:0"],
        )
        .await
        .map_err(|error| {
            if error
                .downcast_ref::<CommandFailure>()
                .is_some_and(|failure| failure.0.contains("peer not permitted for service"))
            {
                FabricGrantRefusal {
                    node: node.into(),
                    protocol: protocol.into(),
                }
                .into()
            } else {
                error
            }
        })?;
        let address: SocketAddr = printed
            .trim()
            .parse()
            .with_context(|| format!("fabric dial printed `{}`", printed.trim()))?;
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
                "echo \"$@\" >> {log}\ncase \"$1\" in id) echo node-a;; dial) echo 127.0.0.1:45678;; *) ;; esac",
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
            "id\ndial node-b st3/fleet/x --tcp 127.0.0.1:0\n\
             expose st3/fleet/x --tcp 127.0.0.1:31313\nunexpose st3/fleet/x\n"
        );

        let remote = Fabric::new(shim(root.path(), "echo 192.168.1.4:45678"));
        assert!(remote.dial("node-b", "p").await.is_err());
    }

    #[tokio::test]
    async fn fabric_grant_refusals_are_distinct_from_unavailable_peers() {
        let root = tempfile::tempdir().unwrap();
        let refused = Fabric::new(shim(
            root.path(),
            "echo 'peer not permitted for service st3-peer-v1' >&2; exit 1",
        ));
        let error = refused
            .dial("invented-leaf", "st3-peer-v1")
            .await
            .unwrap_err();
        assert!(error.is::<FabricGrantRefusal>(), "{error:#}");
        assert!(error.to_string().contains("that member's Fabric grants"));
        let away = Fabric::new(shim(root.path(), "echo 'peer offline' >&2; exit 1"));
        assert!(
            !away
                .dial("invented-leaf", "st3-peer-v1")
                .await
                .unwrap_err()
                .is::<FabricGrantRefusal>()
        );
    }

    #[tokio::test]
    async fn a_pty_route_exposes_a_command_and_dials_a_unix_socket() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("calls");
        let fabric = Fabric::new(shim(
            root.path(),
            &format!(
                "echo \"$@\" >> {log}\ncase \"$1\" in \
                 peers) printf 'node-b\\tBox\\techo,st3/pty/x\\nnode-c\\t\\techo\\n';; \
                 dial) echo /run/fabric/dials/node-b.sock;; *) ;; esac",
                log = log.display()
            ),
        ));
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
            "expose st3/pty/x --exec -- /bin/st terminals serve-fabric\npeers\n\
             dial node-b st3/pty/x\n"
        );

        let relative = Fabric::new(shim(root.path(), "echo dials/node-b.sock"));
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
