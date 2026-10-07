//! Authorized requests from st: GitHub API calls made through this host's sekrets gateway with
//! a profile's token, which st never holds. See `sekrets::authorized` for the contract.

pub use ::sekrets::authorized::{AuthorizedError, AuthorizedRequest, AuthorizedResponse};
use ::sekrets::authorized::HostIdentity;

use crate::config::Config;

/// Make `request` with `profile`'s credential, through this host's gateway. Blocking: call it
/// from `spawn_blocking`. One request, one response; nothing is retried. A 4xx or 5xx is a
/// response, not an error. The daemon and st commands such as gates both call this.
pub fn authorized_request(
    config: &Config,
    profile: &str,
    request: &AuthorizedRequest,
) -> Result<AuthorizedResponse, AuthorizedError> {
    ::sekrets::authorized::request_at(
        &::sekrets::client::socket_path(),
        || host_identity(config),
        profile,
        request,
    )
}

/// This node's name, its person, and the key it signs with: the fleet member key, or the
/// standalone node key, the same files the daemon loads. Never creates one.
fn host_identity(config: &Config) -> Result<HostIdentity, String> {
    use std::os::unix::fs::PermissionsExt as _;
    let person = config
        .person
        .clone()
        .ok_or_else(|| "set `person` in st's configuration".to_owned())?;
    let mut config = config.clone();
    if config.fleet.is_none() {
        config
            .apply_fleet_file()
            .map_err(|error| format!("read the fleet file: {error:#}"))?;
    }
    let path = match &config.fleet {
        Some(file) => file.node_key_path(&config.state_dir),
        None => crate::fleet::join::key_directory(&config.state_dir).join("node.key"),
    };
    let metadata = std::fs::metadata(&path)
        .map_err(|error| format!("this node has no key at {}: {error}", path.display()))?;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(format!("{} is open to other users", path.display()));
    }
    let document = std::fs::read(&path)
        .map_err(|error| format!("read the node key {}: {error}", path.display()))?;
    let key = ::sekrets::keys::Signer::from_pkcs8(&document).map_err(|error| format!("{error:#}"))?;
    Ok(HostIdentity {
        node: config.node.clone(),
        person,
        key,
    })
}
