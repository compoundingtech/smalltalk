//! Founding a fleet and joining one: everything that writes `STATE/fleet/` on this machine.
//!
//! These run while the local daemon is stopped (or before it ever ran). The daemon then applies
//! the result at start with [`super::activate`].

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use super::code::{CodeEndpoint, JoinCode, fingerprint};
use super::file::{FleetFile, FleetMode};
use super::handshake::{JoinResponse, Joiner, SealedJoin, WriterHead};
use super::keys::MemberKey;
use super::transport::{
    Fabric, Route, bindable_tailnet_addresses, local_addresses, parse_route, resolve_tool,
    tailscale_addresses,
};
use crate::error::Error;
use crate::store::{Runtime, Store, valid_fleet_node_name};

pub const DEFAULT_PORT: u16 = 31313;

/// The claim store inside a state directory.
pub fn store_path(state_dir: &Path) -> PathBuf {
    state_dir.join("claims.sqlite3")
}

/// Where a node keeps the keys it signs claims with: its own node key while it is in no fleet,
/// and the keys it holds for people and agents.
pub fn key_directory(state_dir: &Path) -> PathBuf {
    state_dir.join("keys")
}

/// This machine's member key. A node that already signed claims on its own adopts that key, so
/// its earlier claims still verify on every member once membership admits it.
fn member_key(state_dir: &Path) -> Result<MemberKey> {
    let path = fleet_dir(state_dir).join("node.key");
    let standalone = key_directory(state_dir).join("node.key");
    if !path.exists() && standalone.exists() {
        let key = MemberKey::load(&standalone)?;
        write_private(&path, &fs::read(&standalone)?)?;
        return Ok(key);
    }
    MemberKey::load_or_create(&path)
}

/// This machine's node key while it is in no fleet.
pub fn standalone_node_key(state_dir: &Path) -> Result<MemberKey> {
    MemberKey::load_or_create(&key_directory(state_dir).join("node.key"))
}

fn fleet_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("fleet")
}

/// Write a private file atomically: a new `0600` temporary file, synced, then renamed.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let directory = path.parent().context("a private file needs a directory")?;
    fs::create_dir_all(directory)?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    let temporary = directory.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file"),
        std::process::id()
    ));
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temporary, path)?;
    fs::File::open(directory)?.sync_all()?;
    Ok(())
}

/// The first free loopback port at or above `start`.
pub fn free_port(start: u16) -> Result<u16> {
    (start..=u16::MAX)
        .find(|port| std::net::TcpListener::bind(("127.0.0.1", *port)).is_ok())
        .context("no free port for the replication listener")
}

/// Settings shared by founding and joining.
#[derive(Clone, Debug, Default)]
pub struct MemberSettings {
    pub mode: FleetMode,
    pub port: Option<u16>,
    /// `None` detects Tailscale and Fabric.
    pub transports: Option<Vec<String>>,
    pub advertise_loopback: bool,
    pub fabric: Option<PathBuf>,
    pub tailscale: Option<PathBuf>,
}

impl MemberSettings {
    fn transports(&self) -> Vec<String> {
        if let Some(transports) = &self.transports {
            return transports.clone();
        }
        let mut detected = Vec::new();
        if resolve_tool(self.tailscale.as_deref(), "tailscale").is_some() {
            detected.push("tailscale".into());
        }
        if resolve_tool(self.fabric.as_deref(), "fabric").is_some() {
            detected.push("fabric".into());
        }
        detected
    }

    fn port(&self) -> Result<Option<u16>> {
        match self.mode {
            FleetMode::DialOut => Ok(None),
            FleetMode::Listening => Ok(Some(match self.port {
                Some(port) => port,
                None => free_port(DEFAULT_PORT)?,
            })),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Founded {
    pub fleet_id: String,
    pub node: String,
    pub anchor_key: String,
    pub port: Option<u16>,
    pub transports: Vec<String>,
}

/// Found a fleet on this machine: a new fleet ID, secret, and member key. This node becomes the
/// anchor; the daemon admits it when it next starts.
pub fn found(state_dir: &Path, node: &str, settings: &MemberSettings) -> Result<Founded> {
    anyhow::ensure!(
        FleetFile::load(state_dir)?.is_none(),
        "this machine is already in a fleet"
    );
    anyhow::ensure!(
        valid_fleet_node_name(node),
        "`{node}` cannot name a fleet member; use --name"
    );
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)?;
    let fleet_id = uuid::Builder::from_random_bytes(random)
        .into_uuid()
        .to_string();
    let mut secret = [0_u8; 32];
    getrandom::fill(&mut secret)?;
    let directory = fleet_dir(state_dir);
    write_private(&directory.join("secret"), hex::encode(secret).as_bytes())?;
    let key = member_key(state_dir)?;
    let port = settings.port()?;
    let transports = settings.transports();
    FleetFile {
        fleet_id: fleet_id.clone(),
        secret_file: "secret".into(),
        node_key_file: "node.key".into(),
        anchor_key: Some(key.public().into()),
        node: Some(node.into()),
        mode: settings.mode,
        port,
        transports: transports.clone(),
        advertise_loopback: settings.advertise_loopback,
        fabric: settings.fabric.clone(),
        tailscale: settings.tailscale.clone(),
        ..FleetFile::default()
    }
    .save(state_dir)?;
    Ok(Founded {
        fleet_id,
        node: node.into(),
        anchor_key: key.public().into(),
        port,
        transports,
    })
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Checkpoint {
    stage: String,
    fleet_id: String,
    name: String,
    sponsor: String,
}

fn checkpoint_path(state_dir: &Path) -> PathBuf {
    fleet_dir(state_dir).join("join.json")
}

#[derive(Clone)]
pub struct JoinOptions {
    pub state_dir: PathBuf,
    /// The configured node name, used when neither `--name` nor the code names one.
    pub configured_node: String,
    pub code: String,
    pub name: Option<String>,
    /// A loopback/tailnet HTTP or Fabric route to the sponsor, overriding its endpoints.
    pub via: Option<String>,
    pub settings: MemberSettings,
    /// A migration keeps the node's config-peer secret file.
    pub legacy_secret_file: Option<PathBuf>,
    /// A migration can keep the Fabric exposure name the node already uses.
    pub fabric_protocol: Option<String>,
    /// The runtime the store is opened with to read its writer head and begin its first sync.
    pub runtime: Arc<dyn Runtime>,
    /// The joining build's version, which the handshake tells the sponsor.
    pub version: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Joined {
    pub fleet_id: String,
    pub name: String,
    pub sponsor: String,
    pub writer_floor: Option<u64>,
    pub migrate: bool,
    /// True when this run found the join already redeemed and only resumed it.
    pub resumed: bool,
}

/// What an invite is for: the name it pins, how long it lasts, which of the sponsor's
/// endpoints it carries (`auto`, `tailscale`, `fabric` or `loopback`), the person who made it,
/// and whether it migrates a config-peer node.
#[derive(Clone, Debug)]
pub struct InviteOptions {
    pub name: Option<String>,
    pub lifetime: std::time::Duration,
    pub via: String,
    pub person: String,
    pub migrate: bool,
}

/// An invite this node sponsors, and the code that redeems it.
#[derive(Clone, Debug, Serialize)]
pub struct Invitation {
    pub invite: String,
    pub code: String,
    pub expires_at_unix_ms: u64,
}

/// Create an invite that `node`, a current listening member of `fleet_id`, sponsors, and the
/// join code that carries its advertised endpoints.
pub fn invite(
    store: &Store,
    fleet_id: &str,
    node: &str,
    options: &InviteOptions,
) -> Result<Invitation, Error> {
    let via = options.via.as_str();
    if !matches!(via, "auto" | "tailscale" | "fabric" | "loopback") {
        return Err(Error::new(
            "invalid-via",
            "--via is auto, tailscale, fabric, or loopback",
        ));
    }
    let internal = |error: &dyn std::fmt::Display| Error::new("internal", error.to_string());
    let membership = store.fleet_membership().map_err(|error| internal(&error))?;
    let super::MemberState::Current(own) = membership.state(node) else {
        return Err(Error::new(
            "not-a-member",
            "this node is not a current fleet member",
        ));
    };
    let wanted = |transport: &str| via == "auto" || via == transport;
    let text = |value: &serde_json::Value, field: &str| value[field].as_str().map(str::to_owned);
    let endpoints = own
        .endpoints
        .iter()
        .filter_map(|endpoint| {
            let transport = endpoint["transport"].as_str()?;
            if !wanted(transport) {
                return None;
            }
            match transport {
                "tailscale" => Some(CodeEndpoint::Tailscale(text(endpoint, "address")?)),
                "fabric" => Some(CodeEndpoint::Fabric {
                    node: text(endpoint, "node")?,
                    protocol: text(endpoint, "protocol")?,
                }),
                "loopback" => Some(CodeEndpoint::Loopback(text(endpoint, "address")?)),
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    if endpoints.is_empty() {
        return Err(Error::new(
            "no-endpoints",
            format!(
                "this member advertises no {} endpoint yet; is its replication worker running?",
                if via == "auto" { "reachable" } else { via }
            ),
        ));
    }
    let transports = endpoints
        .iter()
        .map(|endpoint| match endpoint {
            CodeEndpoint::Tailscale(_) => "tailscale".to_owned(),
            CodeEndpoint::Fabric { .. } => "fabric".to_owned(),
            CodeEndpoint::Loopback(_) => "loopback".to_owned(),
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let member_key = store
        .member_public_key()
        .ok_or_else(|| Error::new("no-member-key", "this node has no member key"))?;
    let invite = store.create_fleet_invite(
        options.name.as_deref(),
        options.lifetime,
        &transports,
        &options.person,
        options.migrate,
    )?;
    let code = JoinCode {
        fleet_id: uuid::Uuid::parse_str(fleet_id).map_err(|error| internal(&error))?,
        invite: hex::decode(&invite.invite)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| Error::new("internal", "the invite ID is damaged"))?,
        token: invite.token,
        fingerprint: fingerprint(&member_key),
        expires_at: invite.expires_at_unix_ms / 1000,
        name: options.name.clone(),
        migrate: options.migrate,
        endpoints,
    }
    .encode()
    .map_err(|error| internal(&error))?;
    Ok(Invitation {
        invite: format!("fleet-invite/{}", invite.invite),
        code,
        expires_at_unix_ms: invite.expires_at_unix_ms,
    })
}

/// Where to send the handshake: `--via`, else Tailscale when this machine is on a tailnet,
/// else Fabric when it runs Fabric, else an advertised loopback endpoint. Returns the URL for the
/// handshake and the lasting route to record, which for Fabric names the peer, not the tunnel.
async fn sponsor_url(code: &JoinCode, options: &JoinOptions) -> Result<(String, String)> {
    if let Some(via) = &options.via {
        let route = parse_route(via).context(
            "--via needs a loopback or tailnet http:// URL, or fabric://NODE_ID/PROTOCOL",
        )?;
        return match route {
            Route::Http(url) => Ok((url.clone(), url)),
            Route::Fabric { node, protocol } => {
                let fabric = resolve_tool(options.settings.fabric.as_deref(), "fabric")
                    .map(Fabric::new)
                    .context("Fabric is unavailable for --via")?;
                let address = fabric.dial(&node, &protocol).await?;
                Ok((format!("http://{address}"), via.clone()))
            }
        };
    }
    let tailscale = resolve_tool(options.settings.tailscale.as_deref(), "tailscale");
    if let Some(tailscale) = tailscale
        && let Ok(reported) = tailscale_addresses(&tailscale).await
        && !bindable_tailnet_addresses(&reported, &local_addresses()).is_empty()
        && let Some(address) = code.endpoints.iter().find_map(|endpoint| match endpoint {
            CodeEndpoint::Tailscale(address) => Some(address),
            _ => None,
        })
    {
        let url = format!("http://{address}");
        return Ok((url.clone(), url));
    }
    if let Some(fabric) =
        resolve_tool(options.settings.fabric.as_deref(), "fabric").map(Fabric::new)
    {
        for endpoint in &code.endpoints {
            if let CodeEndpoint::Fabric { node, protocol } = endpoint {
                match fabric.dial(node, protocol).await {
                    Ok(address) => {
                        return Ok((
                            format!("http://{address}"),
                            format!("fabric://{node}/{protocol}"),
                        ));
                    }
                    Err(error) => anyhow::bail!(
                        "Fabric could not reach the sponsor for `{protocol}`: {error:#}. \
                         The sponsor must trust this machine's Fabric NodeID and allow `{protocol}` \
                         in its peers.toml"
                    ),
                }
            }
        }
    }
    code.endpoints
        .iter()
        .find_map(|endpoint| match endpoint {
            CodeEndpoint::Loopback(address) => {
                let url = format!("http://{address}");
                Some((url.clone(), url))
            }
            _ => None,
        })
        .context("no route to the sponsor: the code has no endpoint this machine can use")
}

/// Redeem a join code and write this machine's fleet settings. The local daemon must be
/// stopped. Running it again after an interruption resumes: with the same code before the
/// answer arrived, or without one after.
pub async fn join(options: &JoinOptions) -> Result<Joined> {
    let code = JoinCode::decode(&options.code)?;
    let state_dir = &options.state_dir;
    if let Some(existing) = FleetFile::load(state_dir)? {
        anyhow::ensure!(
            existing.fleet_id == code.fleet_id.to_string(),
            "this machine is already in fleet {}",
            existing.fleet_id
        );
        let checkpoint: Option<Checkpoint> = fs::read(checkpoint_path(state_dir))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        if let Some(checkpoint) = checkpoint
            && !code.migrate
        {
            return Ok(Joined {
                fleet_id: checkpoint.fleet_id,
                name: checkpoint.name,
                sponsor: checkpoint.sponsor,
                writer_floor: existing.writer_floor,
                migrate: false,
                resumed: true,
            });
        }
        anyhow::ensure!(
            code.migrate,
            "this machine is already a member of this fleet"
        );
    }
    let name = options
        .name
        .clone()
        .or_else(|| code.name.clone())
        .unwrap_or_else(|| options.configured_node.clone());
    anyhow::ensure!(
        valid_fleet_node_name(&name),
        "`{name}` cannot name a fleet member; use --name"
    );
    if let Some(pinned) = &code.name {
        anyhow::ensure!(*pinned == name, "this code is for `{pinned}`, not `{name}`");
    }
    let directory = fleet_dir(state_dir);
    let key = member_key(state_dir)?;
    let writer_head = {
        fs::create_dir_all(state_dir)?;
        let store = Store::open(&store_path(state_dir), &name, options.runtime.clone())?;
        if let Some(bound) = store.bound_fleet()? {
            anyhow::ensure!(
                bound == code.fleet_id.to_string(),
                "this store belongs to fleet {bound}; reset it (st service reset) to join another"
            );
        }
        store
            .writer_head(&name)?
            .map(|(sequence, hash)| WriterHead { sequence, hash })
    };
    let (url, sponsor_route) = sponsor_url(&code, options).await?;
    let mode = options.settings.mode.as_str();
    let session = Joiner::start(&code, &key, &name, mode, writer_head, &options.version)?;
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let response = http
        .post(format!("{url}/v1/fleet/join"))
        .json(session.request())
        .send()
        .await
        .with_context(|| format!("reach the sponsor at {url}"))?;
    match response.status().as_u16() {
        200 => {}
        404 => anyhow::bail!("the sponsor has no open invite; ask for a new code"),
        403 => anyhow::bail!(
            "the sponsor refused this code: it is unknown, expired, revoked, for another name, \
             or already used by another machine. If you did not use it before, someone else may \
             have; check `st fleet invites` on a member"
        ),
        429 => anyhow::bail!("the sponsor is refusing join attempts for a minute; try again"),
        status => anyhow::bail!("the sponsor answered {status}"),
    }
    let answer: JoinResponse = response
        .json()
        .await
        .context("decode the sponsor's answer")?;
    let sponsor_key = answer.sponsor_key.clone();
    let sealed: SealedJoin = session.open(&answer)?;
    anyhow::ensure!(
        sealed.fleet_id == code.fleet_id.to_string(),
        "the sponsor answered for another fleet"
    );
    let secret_file = match (&sealed.secret, &options.legacy_secret_file) {
        (Some(secret), _) => {
            write_private(&directory.join("secret"), secret.as_bytes())?;
            PathBuf::from("secret")
        }
        (None, Some(legacy)) => legacy.clone(),
        (None, None) => anyhow::bail!("the sponsor sent no fleet secret"),
    };
    FleetFile {
        fleet_id: sealed.fleet_id.clone(),
        secret_file,
        node_key_file: "node.key".into(),
        anchor_key: Some(sealed.anchor_key.clone()),
        node: Some(name.clone()),
        sponsor: Some(sealed.sponsor.clone()),
        sponsor_key: Some(sponsor_key),
        sponsor_routes: vec![sponsor_route],
        mode: options.settings.mode,
        port: options.settings.port()?,
        transports: options.settings.transports(),
        fabric_protocol: options
            .fabric_protocol
            .clone()
            .or_else(|| sealed.fabric_protocol.clone()),
        legacy_peers: code.migrate,
        advertise_loopback: options.settings.advertise_loopback,
        writer_floor: sealed.writer_floor,
        fabric: options.settings.fabric.clone(),
        tailscale: options.settings.tailscale.clone(),
        removed: None,
    }
    .save(state_dir)?;
    // A new member's first sync ends by checking that it projects the same graph as a peer.
    if !code.migrate {
        Store::open(&store_path(state_dir), &name, options.runtime.clone())?
            .begin_first_sync(&sealed.sponsor)?;
    }
    write_private(
        &checkpoint_path(state_dir),
        &serde_json::to_vec(&Checkpoint {
            stage: "redeemed".into(),
            fleet_id: sealed.fleet_id.clone(),
            name: name.clone(),
            sponsor: sealed.sponsor.clone(),
        })?,
    )?;
    Ok(Joined {
        fleet_id: sealed.fleet_id,
        name,
        sponsor: sealed.sponsor,
        writer_floor: sealed.writer_floor,
        migrate: code.migrate,
        resumed: false,
    })
}

/// Make this config-peer node the anchor of its existing fleet: it keeps its fleet ID, secret,
/// store, and history, gets a member key, and admits itself when the daemon next starts.
/// Legacy exchanges stay accepted until `st fleet migrate --finish`.
pub fn migrate_anchor(
    state_dir: &Path,
    node: &str,
    fleet_id: &str,
    secret_file: &Path,
    settings: &MemberSettings,
    fabric_protocol: Option<String>,
    runtime: Arc<dyn Runtime>,
) -> Result<Founded> {
    anyhow::ensure!(
        FleetFile::load(state_dir)?.is_none(),
        "this machine already has fleet membership settings"
    );
    anyhow::ensure!(
        valid_fleet_node_name(node),
        "`{node}` cannot name a fleet member"
    );
    {
        let store = Store::open(&store_path(state_dir), node, runtime)?;
        match store.bound_fleet()? {
            Some(bound) => anyhow::ensure!(
                bound == fleet_id,
                "this store belongs to fleet {bound}, not the configured {fleet_id}"
            ),
            None => anyhow::bail!("this store is not bound to a fleet yet; start st3 once first"),
        }
    }
    let key = member_key(state_dir)?;
    let port = settings.port()?;
    let transports = settings.transports();
    FleetFile {
        fleet_id: fleet_id.into(),
        secret_file: secret_file.to_path_buf(),
        node_key_file: "node.key".into(),
        anchor_key: Some(key.public().into()),
        node: Some(node.into()),
        mode: settings.mode,
        port,
        transports: transports.clone(),
        fabric_protocol,
        legacy_peers: true,
        advertise_loopback: settings.advertise_loopback,
        fabric: settings.fabric.clone(),
        tailscale: settings.tailscale.clone(),
        ..FleetFile::default()
    }
    .save(state_dir)?;
    Ok(Founded {
        fleet_id: fleet_id.into(),
        node: node.into(),
        anchor_key: key.public().into(),
        port,
        transports,
    })
}
