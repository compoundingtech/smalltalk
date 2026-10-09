use std::fmt;
use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use smallclaims::fleet::transport::Fabric;
use smallclaims::fleet::view::MemberView;

/// How long each side waits for a route: Fabric's dial, the route line, and the owner's answer.
pub const ROUTE_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest route line or answer either side reads.
pub const ROUTE_LINE_LIMIT: usize = 4_096;

/// The Fabric protocol that serves a fleet's PTY sessions. Each fleet has its own, so an isolated
/// fleet on the same machine never replaces another fleet's exposure.
pub fn protocol(fleet_id: &str) -> String {
    format!("st3/pty/{fleet_id}")
}

/// Attach to PTY session `name`, which must be `subject`'s terminal at runtime incarnation
/// `incarnation` (`PTY_DAEMON_PID:CREATED_AT`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RouteRequest {
    pub op: String,
    pub name: String,
    pub subject: String,
    pub incarnation: String,
}

impl RouteRequest {
    pub fn new(name: &str, subject: &str, incarnation: &str) -> Self {
        Self {
            op: "route".into(),
            name: name.into(),
            subject: subject.into(),
            incarnation: incarnation.into(),
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
pub struct RouteAnswer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Why a route did not open.
#[derive(Debug)]
pub enum RouteError {
    /// Fabric, the tunnel, or the owner's answer failed, so another try may work.
    Unreachable(String),
    /// The owner answered and refused: its session is not the one st selected.
    Refused(String),
}

impl fmt::Display for RouteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(reason) | Self::Refused(reason) => formatter.write_str(reason),
        }
    }
}

impl std::error::Error for RouteError {}

/// Where a terminal's owner serves its PTY sessions over Fabric.
#[derive(Clone, Debug)]
pub struct FabricTarget {
    pub fabric: Fabric,
    /// The owner's Fabric NodeID.
    pub peer: String,
    pub protocol: String,
}

/// The Fabric NodeID of fleet host `owner`: the node its member record advertises, or else the
/// trusted Fabric peer whose local name is the host's name, ignoring case.
pub async fn owner_peer(fabric: &Fabric, members: &[MemberView], owner: &str) -> Option<String> {
    let advertised = members
        .iter()
        .filter(|member| member.name == owner && member.state == "current")
        .flat_map(|member| &member.endpoints)
        .filter(|endpoint| endpoint["transport"] == "fabric")
        .find_map(|endpoint| endpoint["node"].as_str().map(str::to_owned));
    if advertised.is_some() {
        return advertised;
    }
    let peers = fabric.peers().await.ok()?;
    let mut named = peers
        .into_iter()
        .filter(|(_, name)| name.eq_ignore_ascii_case(owner));
    // Two peers with the host's name leave the choice to a person.
    match (named.next(), named.next()) {
        (Some((node, _)), None) => Some(node),
        _ => None,
    }
}

/// Open a route to `request`'s PTY session on `target`. The stream speaks the PTY session
/// protocol once this returns.
pub async fn open_route(
    target: &FabricTarget,
    request: &RouteRequest,
) -> Result<StdUnixStream, RouteError> {
    let socket = tokio::time::timeout(
        ROUTE_TIMEOUT,
        target.fabric.dial_socket(&target.peer, &target.protocol),
    )
    .await
    .map_err(|_| {
        RouteError::Unreachable(format!(
            "fabric dial did not answer within {} s",
            ROUTE_TIMEOUT.as_secs()
        ))
    })?
    .map_err(|error| RouteError::Unreachable(format!("{error:#}")))?;
    let line = format!(
        "{}\n",
        serde_json::to_string(request).expect("a route request serializes")
    );
    let protocol = target.protocol.clone();
    tokio::task::spawn_blocking(move || handshake(&socket, &line, &protocol))
        .await
        .map_err(|error| RouteError::Unreachable(format!("the route handshake failed: {error}")))?
}

fn handshake(socket: &Path, line: &str, protocol: &str) -> Result<StdUnixStream, RouteError> {
    let unreachable = |what: &str, error: std::io::Error| {
        RouteError::Unreachable(format!(
            "{what} the Fabric tunnel {}: {error}",
            socket.display()
        ))
    };
    let mut stream =
        StdUnixStream::connect(socket).map_err(|error| unreachable("connect to", error))?;
    stream
        .set_read_timeout(Some(ROUTE_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(ROUTE_TIMEOUT)))
        .map_err(|error| unreachable("configure", error))?;
    stream
        .write_all(line.as_bytes())
        .map_err(|error| unreachable("write to", error))?;
    // The owner writes nothing after its answer until the attach client speaks, but one byte at
    // a time still leaves every byte after the answer to the attach client.
    let mut answer = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => {
                return Err(RouteError::Unreachable(format!(
                    "the owner closed the tunnel without answering; its Fabric may not serve `{protocol}` to this machine"
                )));
            }
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) if answer.len() < ROUTE_LINE_LIMIT => answer.push(byte[0]),
            Ok(_) => {
                return Err(RouteError::Unreachable(
                    "the owner's answer is too long".into(),
                ));
            }
            Err(error) => return Err(unreachable("read from", error)),
        }
    }
    let answer: RouteAnswer = serde_json::from_slice(&answer).map_err(|_| {
        RouteError::Unreachable(format!(
            "the owner answered `{}`",
            String::from_utf8_lossy(&answer)
        ))
    })?;
    if answer.ok != Some(true) {
        return Err(RouteError::Refused(
            answer
                .error
                .unwrap_or_else(|| "the owner refused the route".into()),
        ));
    }
    stream
        .set_read_timeout(None)
        .and_then(|()| stream.set_write_timeout(None))
        .map_err(|error| unreachable("configure", error))?;
    Ok(stream)
}
