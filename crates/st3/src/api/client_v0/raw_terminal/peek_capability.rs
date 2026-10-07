//! Browser PEEK capabilities are daemon-local, session-bound and single use, never claims.
use super::*;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

const MAX_CAPABILITIES: usize = 4096;

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

fn prepare_issue(capabilities: &mut BTreeMap<String, Capability>, now: u128) -> Result<(), ApiError> {
    capabilities.retain(|_, capability| capability.expires > now);
    if capabilities.len() >= MAX_CAPABILITIES {
        return Err(ApiError {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "rate-limited".into(),
            message: "raw PEEK capability admission is busy; retry after consuming or expiry".into(),
            details: Box::default(),
        });
    }
    Ok(())
}

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
    prepare_issue(&mut capabilities, now)?;
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
    let now = client_now_ms();
    capabilities.retain(|_, capability| capability.expires > now);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn capability(expires: u128) -> Capability {
        Capability {
            state_dir: PathBuf::new(),
            actor: String::new(),
            person: String::new(),
            terminal: String::new(),
            incarnation: String::new(),
            runtime: String::new(),
            owner: String::new(),
            authorization_epoch: String::new(),
            expires,
        }
    }

    #[test]
    fn capability_admission_is_bounded_and_expiry_reclaims_slots() {
        let mut capabilities = BTreeMap::new();
        for index in 0..MAX_CAPABILITIES {
            capabilities.insert(index.to_string(), capability(100));
        }
        let error = prepare_issue(&mut capabilities, 99).expect_err("full cache must refuse admission");
        assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error.code, "rate-limited");
        assert_eq!(capabilities.len(), MAX_CAPABILITIES);
        prepare_issue(&mut capabilities, 100).expect("expired capabilities must reclaim admission");
        assert!(capabilities.is_empty());
        capabilities.insert("live".into(), capability(101));
        capabilities.insert("expired".into(), capability(100));
        prepare_issue(&mut capabilities, 100).expect("live capability leaves capacity");
        assert_eq!(capabilities.len(), 1);
        assert!(capabilities.contains_key("live"));
    }
}
