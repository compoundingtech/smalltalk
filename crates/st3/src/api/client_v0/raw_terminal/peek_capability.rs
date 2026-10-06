//! Browser PEEK capabilities are daemon-local, session-bound and single use, never claims.
use super::*;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

struct Capability {
    state_dir: PathBuf,
    actor: String,
    person: String,
    terminal: String,
    incarnation: String,
    runtime: String,
    owner: String,
    authorization_epoch: String,
    expires: u128,
}

static CAPABILITIES: LazyLock<Mutex<BTreeMap<String, Capability>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

pub(super) fn issue(
    state: &AppState,
    session: &ClientSession,
    terminal: &str,
    live: &LiveSession,
) -> Result<String, ApiError> {
    let authorization_epoch = authorization_epoch(state, session)?;
    let mut nonce = [0_u8; 32];
    getrandom::fill(&mut nonce).map_err(ApiError::internal)?;
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(nonce);
    let now = client_now_ms();
    let mut capabilities = CAPABILITIES.lock().map_err(ApiError::internal)?;
    capabilities.retain(|_, capability| capability.expires > now);
    capabilities.insert(credential_digest(&token), Capability {
        state_dir: state.state_dir.clone(),
        actor: session.actor.clone(),
        person: session.authority_actor.clone(),
        terminal: terminal.into(),
        incarnation: live.incarnation_id.clone(),
        runtime: live.runtime_id.clone(),
        owner: live.owner_host_id.clone(),
        authorization_epoch,
        expires: now + 60_000,
    });
    Ok(token)
}

pub(super) fn consume(
    state: &AppState,
    session: &ClientSession,
    terminal: &str,
    live: &LiveSession,
    token: &str,
) -> Result<String, ApiError> {
    let digest = credential_digest(token);
    let mut capabilities = CAPABILITIES.lock().map_err(ApiError::internal)?;
    let capability = capabilities.get(&digest)
        .ok_or_else(|| forbidden("raw PEEK capability is unknown or consumed"))?;
    if capability.state_dir != state.state_dir
        || capability.actor != session.actor
        || capability.person != session.authority_actor
        || capability.terminal != terminal
        || capability.incarnation != live.incarnation_id
        || capability.runtime != live.runtime_id
        || capability.owner != live.owner_host_id
        || capability.expires <= client_now_ms()
    {
        return Err(forbidden("raw PEEK capability binding differs or expired"));
    }
    Ok(capabilities.remove(&digest).expect("validated capability exists").authorization_epoch)
}
