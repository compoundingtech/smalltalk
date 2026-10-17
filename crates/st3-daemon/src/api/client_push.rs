//! Paired-session-only registration. All input/database failures are redacted at this boundary.
use super::client_v0::ClientSession;
use super::*;
use crate::store::push_registrations::{PushCleanupProof, PushRegistration, PushRegistrationStatus};

pub(super) const MAX_BODY_BYTES: usize = 4096;

static CHANGES: std::sync::LazyLock<tokio::sync::broadcast::Sender<String>> =
    std::sync::LazyLock::new(|| tokio::sync::broadcast::channel(64).0);

pub(super) fn subscribe() -> tokio::sync::broadcast::Receiver<String> {
    CHANGES.subscribe()
}

fn invalid(message: &'static str) -> ApiError {
    ApiError::bad(St3Error::new("validation-failed", message))
}

fn unavailable() -> ApiError {
    ApiError::internal("local push registration storage is unavailable")
}

/// Cleanup proof is deliberately not a session: retired credentials regain no read/write scopes.
pub(super) enum Authentication {
    Session(ClientSession),
    Cleanup(PushCleanupProof),
}

pub(super) fn authenticate(
    state: &AppState,
    request: &Request<Body>,
    transport: &'static str,
) -> Result<Authentication, ApiError> {
    if request.method() != axum::http::Method::DELETE
        || request.uri().path() != "/v1/client/push-registration"
        || transport == "unix"
    {
        return super::client_v0::authenticate(state, request, transport)
            .map(Authentication::Session);
    }
    let denied = || ApiError {
        status: StatusCode::FORBIDDEN,
        code: "forbidden".into(),
        message: "push cleanup requires proof of a pairing issued by this member".into(),
        details: Box::default(),
    };
    let credential = request.headers().get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty())
        .ok_or_else(denied)?;
    let digest = hex::encode(Sha256::digest(credential.as_bytes()));
    let pairings = state.store
        .claims_for_kind_at("custom.client.pairing-completed", None, true, 10_000)
        .map_err(|_| unavailable())?;
    let paired = pairings.claims.iter().find(|claim| {
        claim.origin == state.store.origin()
            && claim.body.pointer("/fields/credential_hash").and_then(Value::as_str)
                == Some(digest.as_str())
    }).ok_or_else(denied)?;
    let field = |name: &str| paired.body.get("fields").and_then(|fields| fields.get(name))
        .and_then(Value::as_str).filter(|value| !value.is_empty());
    let person = field("person_id")
        .filter(|person| person.starts_with("person/") && person.matches('/').count() == 1)
        .ok_or_else(denied)?;
    Ok(Authentication::Cleanup(PushCleanupProof {
        grant: paired.subject.clone(),
        pairing_index: paired.store_index,
        person: person.into(),
        device: field("device_id").ok_or_else(denied)?.into(),
        actor: field("session_actor").ok_or_else(denied)?.into(),
        credential_hash: digest,
    }))
}

fn paired(session: &ClientSession) -> Result<&str, ApiError> {
    session.pairing_grant.as_deref().filter(|_| session.pairing_issued_here && session.transport != "unix")
        .ok_or_else(|| ApiError {
            status: StatusCode::FORBIDDEN,
            code: "forbidden".into(),
            message: "push registration requires a current authenticated paired session issued by this member".into(),
            details: Box::default(),
        })
}

fn validate(registration: &PushRegistration) -> Result<(), ApiError> {
    if !(2..=512).contains(&registration.token.len())
        || !registration.token.len().is_multiple_of(2)
        || !registration
            .token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid(
            "token must contain 2 to 512 hexadecimal characters in byte pairs",
        ));
    }
    if !(1..=255).contains(&registration.bundle_id.len())
        || !registration
            .bundle_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        return Err(invalid(
            "bundle_id must contain 1 to 255 ASCII letters, digits, dots or hyphens",
        ));
    }
    if !(1..=64).contains(&registration.app_version.len())
        || !registration
            .app_version
            .bytes()
            .all(|byte| matches!(byte, b' '..=b'~'))
    {
        return Err(invalid(
            "app_version must contain 1 to 64 printable ASCII characters",
        ));
    }
    Ok(())
}

pub(super) async fn register(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    headers: axum::http::HeaderMap,
    body: Result<axum::body::Bytes, axum::extract::rejection::BytesRejection>,
) -> Result<Json<Value>, ApiError> {
    paired(&session)?;
    let body = body.map_err(|error| ApiError {
        status: error.status(),
        code: "validation-failed".into(),
        message: "invalid or oversized push registration".into(),
        details: Box::default(),
    })?;
    let json_content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| {
            let value = value.trim();
            value == "application/json"
                || value.starts_with("application/") && value.ends_with("+json")
        });
    if !json_content_type {
        return Err(ApiError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            code: "validation-failed".into(),
            message: "push registration requires a JSON content type".into(),
            details: Box::default(),
        });
    }
    // JsonRejection can log serde's error before a handler catches it, including a secret
    // unknown-field name or enum value. Decode bytes directly and never format that error.
    let mut registration: PushRegistration =
        serde_json::from_slice(&body).map_err(|_| invalid("invalid push registration"))?;
    validate(&registration)?;
    registration.token.make_ascii_lowercase();
    update(state, session, Some(registration)).await
}

pub(super) async fn unregister(
    State(state): State<AppState>,
    proof: Option<Extension<PushCleanupProof>>,
    body: Result<axum::body::Bytes, axum::extract::rejection::BytesRejection>,
) -> Result<Json<Value>, ApiError> {
    let proof = proof.ok_or_else(|| ApiError {
        status: StatusCode::FORBIDDEN,
        code: "forbidden".into(),
        message: "push cleanup requires proof of a pairing issued by this member".into(),
        details: Box::default(),
    })?.0;
    let body = body.map_err(|error| ApiError {
        status: error.status(),
        code: "validation-failed".into(),
        message: "invalid or oversized push unregistration".into(),
        details: Box::default(),
    })?;
    if !body.is_empty() {
        return Err(invalid("push unregistration does not accept a body"));
    }
    let store = state.store.clone();
    let status = tokio::task::spawn_blocking(move || {
        store.cleanup_push_registration(&proof)
            .map_err(|_| unavailable())?
            .ok_or_else(|| ApiError {
                status: StatusCode::FORBIDDEN,
                code: "forbidden".into(),
                message: "push cleanup proof is no longer valid".into(),
                details: Box::default(),
            })
    }).await.map_err(|_| unavailable())??;
    let _ = CHANGES.send(state.node.clone());
    Ok(Json(json!(status)))
}

async fn update(
    state: AppState,
    session: ClientSession,
    registration: Option<PushRegistration>,
) -> Result<Json<Value>, ApiError> {
    paired(&session)?;
    let store = state.store.clone();
    let status: PushRegistrationStatus = tokio::task::spawn_blocking(move || {
        store.update_push_registration(
            session.pairing_grant.as_deref().expect("paired session checked"),
            &session.authority_actor, &session.actor, registration.as_ref(), || client_now_ms() as u64,
        ).map_err(|_| unavailable())?.ok_or_else(|| ApiError {
            status: StatusCode::FORBIDDEN, code: "forbidden".into(),
            message: "push registration requires a current authenticated paired session issued by this member".into(),
            details: Box::default(),
        })
    }).await.map_err(|_| unavailable())??;
    // No graph claim changes: wake only this member's held device windows.
    let _ = CHANGES.send(state.node.clone());
    Ok(Json(json!(status)))
}

/// No attention subscription or publication is needed to remove an expired secret.
pub fn start_push_registration_cleanup(state: &AppState) {
    let store = state.store.clone();
    tokio::spawn(async move {
        let mut clock = tokio::time::interval(Duration::from_secs(30));
        clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            clock.tick().await;
            let store = store.clone();
            match tokio::task::spawn_blocking(move || {
                store.sweep_push_registrations(client_now_ms() as u64)
            })
            .await
            {
                Ok(Ok(_)) => {}
                // Never format a database or task error: registration inputs are secret.
                _ => eprintln!("st3: local push registration cleanup is unavailable"),
            }
        }
    });
}

#[cfg(test)]
mod tests;
