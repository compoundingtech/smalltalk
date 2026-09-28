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
    anyhow::ensure!(
        output.status.success(),
        "{} {} failed: {}",
        program.display(),
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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

    /// This machine's Fabric NodeID.
    pub async fn id(&self) -> Result<String> {
        let id = run(&self.program, &["id"]).await?.trim().to_owned();
        anyhow::ensure!(!id.is_empty(), "fabric id printed nothing");
        Ok(id)
    }

    /// Expose a loopback TCP listener to trusted peers under `protocol`, without persisting
    /// the exposure in Fabric's configuration. Repeating it is harmless.
    pub async fn expose(&self, protocol: &str, address: &str) -> Result<()> {
        run(
            &self.program,
            &["expose", protocol, "--tcp", address, "--ephemeral"],
        )
        .await
        .map(|_| ())
    }

    pub async fn unexpose(&self, protocol: &str) -> Result<()> {
        run(&self.program, &["unexpose", protocol])
            .await
            .map(|_| ())
    }

    /// Ask Fabric for a loopback TCP tunnel to a peer's exposed protocol and return its
    /// address. Fabric reuses the tunnel while it lives, so repeating this is cheap.
    pub async fn dial(&self, node: &str, protocol: &str) -> Result<SocketAddr> {
        let printed = run(
            &self.program,
            &["dial", node, protocol, "--tcp", "127.0.0.1:0"],
        )
        .await?;
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
             expose st3/fleet/x --tcp 127.0.0.1:31313 --ephemeral\nunexpose st3/fleet/x\n"
        );

        let remote = Fabric::new(shim(root.path(), "echo 192.168.1.4:45678"));
        assert!(remote.dial("node-b", "p").await.is_err());
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
