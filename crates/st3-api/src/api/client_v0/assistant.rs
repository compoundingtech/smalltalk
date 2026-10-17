//! External assistants have their own versioned claims, never a delegated person chain.
use super::*;
use rusqlite::OptionalExtension as _;
mod authorization;
mod reads;
mod native;
pub use native::propose;
pub use authorization::{ReadReceipt, finish as finish_read, pin as pin_read_trust, read as read_projection};

pub const PRINCIPAL: &str = "assistant/chatgpt";
const BEGUN: &str = "custom.client.assistant.v1.begun";
const GRANTED: &str = "custom.client.assistant.v1.granted";
const POLICY: &str = "custom.client.assistant.v1.policy";
const REVOKED: &str = "custom.client.assistant.v1.revoked";
const SCOPES: &[&str] = &[
    "read.assistant.summary", "read.assistant.attention", "read.assistant.missions",
    "read.assistant.agents", "read.assistant.conversation", "read.assistant.terminal",
    "write.assistant.messages", "write.assistant.custom-reply",
];
// Serializes code consumption and grant policy transitions. Subject fences also protect
// against concurrent writers in the authoritative store.
static TRANSITIONS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantPairingBegin {
    api_version: String,
    device_name: String,
    assistant_id: String,
    scopes: Vec<String>,
    #[serde(default = "default_expiry")]
    expires_in_seconds: u64,
    disclosure_consent: bool,
    consent_evidence: String,
    #[serde(default = "default_read_mode")]
    minimal_read_mode: String,
}
fn default_expiry() -> u64 { 3600 }
fn default_read_mode() -> String { "approval-once".into() }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantGrantPolicy {
    api_version: String,
    scopes: Vec<String>,
    #[serde(default = "default_expiry")]
    expires_in_seconds: u64,
    disclosure_consent: bool,
    consent_evidence: String,
    minimal_read_mode: String,
    #[serde(default)]
    recipient_allowlist: Vec<String>,
}

fn owning_person(state: &AppState) -> Result<&str, ApiError> {
    state.owning_person.as_deref()
        .filter(|person| person.starts_with("person/") && person.matches('/').count() == 1 && person.len() > 7)
        .ok_or_else(|| forbidden("assistant grants require a configured concrete owning person"))
}
pub fn trusted_issuer<'a>(state: &'a AppState, session: &ClientSession) -> Result<&'a str, ApiError> {
    let issuer = owning_person(state)?;
    if session.transport != "unix" || session.actor != issuer
        || session.authority_actor != issuer || session.pairing_grant.is_some() || session.local_peer {
        return Err(forbidden("assistant grants require the configured owning person through trusted-local Unix"));
    }
    Ok(issuer)
}
fn validate_policy(version: &str, scopes: &[String], seconds: u64, consent: bool,
                   evidence: &str) -> Result<(), ApiError> {
    if version != CLIENT_API_VERSION || seconds == 0 || seconds > 2_592_000 {
        return Err(validation("assistant expiry must be positive and at most 30 days"));
    }
    if scopes.is_empty() || scopes.iter().any(|scope| !SCOPES.contains(&scope.as_str()))
        || SCOPES[..4].iter().any(|scope| !scopes.iter().any(|item| item == scope)) {
        return Err(validation("assistant scopes must explicitly include the four minimal scopes and no unrelated scopes"));
    }
    if !consent || evidence.trim().is_empty() || evidence.len() > 512 {
        return Err(forbidden(
            "assistant issuance requires explicit disclosure to OpenAI and consent evidence (1-512 bytes)",
        ));
    }
    Ok(())
}
fn subject(id: &str) -> Result<String, ApiError> {
    let id = id.strip_prefix("assistant-grant/").unwrap_or(id);
    if id.len() != 24 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(validation("invalid assistant grant ID"));
    }
    Ok(format!("custom/client/assistant-v1-{id}"))
}
fn fields(claim: &ClaimRecord) -> Result<&Value, ApiError> {
    claim.body.get("fields").ok_or_else(|| forbidden("invalid assistant grant claim"))
}
fn latest(state: &AppState, subject: &str, kind: &str) -> Result<Option<ClaimRecord>, ApiError> {
    Ok(state.store.claims_for_subject_kind_at(subject, kind, None, true, 1)
        .map_err(ApiError::internal)?.claims.into_iter().next())
}
fn append(state: &AppState, subject: String, kind: &str, actor: &str,
          value: Value, previous: Option<&ClaimRecord>) -> Result<ClaimRecord, ApiError> {
    state.store.append_claim(&ClaimInput {
        subject, kind: kind.into(), actor: Some(actor.into()),
        fields: serde_json::from_value(value).map_err(ApiError::internal)?,
        evidence: previous.map(|claim| vec![claim.id.clone()]).unwrap_or_default(),
        expected_subject: Some(previous.map(|claim| claim.id.clone())),
        idempotency_key: None,
    }).map_err(ApiError::bad)
}

fn audit<T>(state: &AppState, operation: &str, actor: &str, transport: &str,
            grant_id: Option<&str>, result: &Result<T, ApiError>) -> Result<(), ApiError> {
    let mut random = [0_u8; 12];
    getrandom::fill(&mut random).map_err(ApiError::internal)?;
    let mut referenced = Vec::new();
    if let Some(subject) = grant_id.and_then(|id| subject(id).ok()) {
        for kind in [BEGUN, GRANTED, POLICY, REVOKED] {
            if let Some(claim) = latest(state, &subject, kind)? {
                referenced.push(claim.id);
            }
        }
    }
    state.store.append_claim(&ClaimInput {
        subject: format!("custom/client/assistant-audit-v1-{}", hex::encode(random)),
        kind: "custom.client.assistant.v1.audit".into(), actor: Some(actor.into()),
        fields: serde_json::from_value(json!({"version":1,"operation":operation,
            "issuer":state.owning_person,"principal":PRINCIPAL,"actor":actor,"transport":transport,
            "grant_id":grant_id,"timestamp":client_timestamp(client_now_ms()),
            "outcome":if result.is_ok() {"accepted"} else {"denied"},
            "denial_code":result.as_ref().err().map(|error| error.code.as_str()),
            "claim_ids":referenced})).map_err(ApiError::internal)?,
        evidence: Vec::new(), expected_subject: None, idempotency_key:None,
    }).map_err(ApiError::bad)?;
    Ok(())
}

pub async fn begin(state: AppState, session: ClientSession,
                         request: AssistantPairingBegin) -> Result<Json<Value>, ApiError> {
    let result = begin_inner(state.clone(), session.clone(), request).await;
    let id = result.as_ref().ok().and_then(|Json(value)| value["grant_id"].as_str());
    audit(&state, "create", &session.actor, session.transport, id, &result)?;
    result
}
pub async fn complete(state: &AppState, id: &str, request: &PairingComplete,
                            transport: &str) -> Result<Option<Json<Value>>, ApiError> {
    let result = complete_inner(state, id, request, transport).await;
    if !matches!(&result, Ok(None)) {
        let actor = result.as_ref().ok().and_then(Option::as_ref)
            .and_then(|Json(value)| value["session_actor"].as_str()).unwrap_or("custom/client/pairing-completion");
        audit(state, "complete", actor, transport, Some(id), &result)?;
    }
    result
}
pub async fn revoke(State(state): State<AppState>, Extension(session): Extension<ClientSession>,
                          AxumPath(id): AxumPath<String>) -> Result<Json<Value>, ApiError> {
    let result = revoke_inner(State(state.clone()), Extension(session.clone()), AxumPath(id.clone())).await;
    audit(&state, "revoke", &session.actor, session.transport, Some(&id), &result)?;
    result
}
pub async fn update_policy(State(state): State<AppState>, Extension(session): Extension<ClientSession>,
                                 AxumPath(id): AxumPath<String>, Json(request): Json<AssistantGrantPolicy>)
    -> Result<Json<Value>, ApiError> {
    let result = update_policy_inner(State(state.clone()), Extension(session.clone()), AxumPath(id.clone()), Json(request)).await;
    audit(&state, "renew-policy", &session.actor, session.transport, Some(&id), &result)?;
    result
}

async fn begin_inner(state: AppState, session: ClientSession,
                         request: AssistantPairingBegin) -> Result<Json<Value>, ApiError> {
    let issuer = trusted_issuer(&state, &session)?;
    validate_policy(&request.api_version, &request.scopes, request.expires_in_seconds,
                    request.disclosure_consent, &request.consent_evidence)?;
    if !matches!(request.minimal_read_mode.as_str(), "approval-once" | "caller-bound-allow") {
        return Err(forbidden("unsupported assistant read mode"));
    }
    if request.assistant_id != PRINCIPAL || request.device_name.trim().is_empty()
        || request.device_name.len() > 120 {
        return Err(validation("invalid assistant principal or device name"));
    }
    let mut random = [0_u8; 32];
    getrandom::fill(&mut random).map_err(ApiError::internal)?;
    let id = hex::encode(&random[..12]);
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let code: String = random[12..20].iter()
        .map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char).collect();
    let now = client_now_ms();
    let grant_id = format!("assistant-grant/{id}");
    let expires = now + u128::from(request.expires_in_seconds) * 1000;
    let scopes: BTreeSet<_> = request.scopes.into_iter().collect();
    append(&state, subject(&id)?, BEGUN, issuer, json!({
        "version":1,"grant_id":grant_id,"principal":PRINCIPAL,"sender_class":"external-assistant",
        "issuer":issuer,"device_name":request.device_name,"scopes":scopes,
        "expires_at_unix_ms":expires,"challenge_expires_at_unix_ms":now + 300_000,
        "code_hash":credential_digest(&code),"policy_revision":1,
        "minimal_read_mode":request.minimal_read_mode,"recipient_allowlist":[],
        "disclosure_consent":true,"consent_evidence":request.consent_evidence,
        "transport":session.transport,"timestamp":client_timestamp(now),"outcome":"created"
    }), None)?;
    signal_changed(&state);
    Ok(Json(json!({"kind":"pairing-challenge","pairing_id":format!("pairing/{id}"),
        "code":code,"expires_at":client_timestamp(now + 300_000),"grant_kind":"assistant",
        "grant_id":grant_id,"principal":PRINCIPAL,"grant_expires_at":client_timestamp(expires),
        "policy_revision":1,"minimal_read_mode":request.minimal_read_mode})))
}

async fn complete_inner(state: &AppState, id: &str, request: &PairingComplete,
                            transport: &str) -> Result<Option<Json<Value>>, ApiError> {
    let Ok(subject) = subject(id) else { return Ok(None) };
    let _transition = TRANSITIONS.lock().await;
    let Some(begun) = latest(state, &subject, BEGUN)? else { return Ok(None) };
    let issuer = owning_person(state)?;
    if begun.origin != state.store.origin() || begun.actor.as_deref() != Some(issuer) {
        return Err(forbidden("assistant pairing must complete on its authoritative issuer"));
    }
    let source = fields(&begun)?;
    let now = client_now_ms();
    if latest(state, &subject, GRANTED)?.is_some() || latest(state, &subject, REVOKED)?.is_some()
        || source["code_hash"].as_str() != Some(credential_digest(&request.code).as_str())
        || source["challenge_expires_at_unix_ms"].as_u64().map(u128::from).unwrap_or(0) <= now
        || source["expires_at_unix_ms"].as_u64().map(u128::from).unwrap_or(0) <= now {
        return Err(forbidden("assistant pairing code is invalid, expired, or already used"));
    }
    // The key identifies this assistant only; do not enroll it into any person's signing chain.
    let key = device_signing_key(&request.device_public_key)
        .ok_or_else(|| validation("assistant completion requires a valid public key"))?;
    let hash = credential_digest(key);
    let device_id = format!("device/{}", &hash[..24]);
    let grant_id = source["grant_id"].as_str().ok_or_else(|| forbidden("invalid assistant grant"))?;
    let actor = format!("{PRINCIPAL}/session/{grant_id}");
    let mut random = [0_u8; 32];
    getrandom::fill(&mut random).map_err(ApiError::internal)?;
    let credential = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random);
    let mut grant = source.clone();
    let object = grant.as_object_mut().ok_or_else(|| forbidden("invalid assistant grant"))?;
    object.remove("code_hash");
    object.remove("challenge_expires_at_unix_ms");
    object.insert("credential_hash".into(), json!(credential_digest(&credential)));
    object.insert("key_id".into(), json!(hash));
    object.insert("device_public_key".into(), json!(key));
    object.insert("device_id".into(), json!(device_id));
    object.insert("session_actor".into(), json!(actor));
    object.insert("transport".into(), json!(transport));
    object.insert("timestamp".into(), json!(client_timestamp(now)));
    object.insert("outcome".into(), json!("completed"));
    let claim = append(state, subject, GRANTED, issuer, grant, Some(&begun))?;
    let projection = projection(state, &claim)?;
    signal_changed(state);
    Ok(Some(Json(json!({"kind":"assistant-paired-session","api_version":CLIENT_API_VERSION,"principal":PRINCIPAL,
        "sender_class":"external-assistant","device_id":device_id,"session_actor":actor,
        "credential":credential,"scopes":projection["scopes"],"expires_at":projection["expires_at"],
        "grant":projection}))))
}
fn effective(state: &AppState, granted: &ClaimRecord) -> Result<ClaimRecord, ApiError> {
    Ok(latest(state, &granted.subject, POLICY)?.unwrap_or_else(|| granted.clone()))
}
fn projection(state: &AppState, granted: &ClaimRecord) -> Result<Value, ApiError> {
    let policy = effective(state, granted)?;
    let source = fields(&policy)?;
    let mut value = json!({"kind":"assistant-grant","id":source["grant_id"],
        "principal":PRINCIPAL,"sender_class":"external-assistant","issuer":source["issuer"],
        "expires_at":client_timestamp(source["expires_at_unix_ms"].as_u64().map(u128::from).unwrap_or(0)),
        "revoked":latest(state, &granted.subject, REVOKED)?.is_some()});
    for name in ["session_actor","device_id","key_id","scopes","policy_revision",
                 "minimal_read_mode","recipient_allowlist","disclosure_consent","consent_evidence"] {
        value[name] = source[name].clone();
    }
    if let Some(trust) = source.get("read_trust") {
        value["read_trust"] = trust.clone();
    }
    Ok(value)
}

pub fn authenticate(state: &AppState, digest: &str, transport: &'static str)
    -> Result<Option<ClientSession>, ApiError> {
    let id: Option<String> = state.store.readers.get().query_row(
        "SELECT id FROM claims WHERE kind=?1 AND json_extract(body,'$.fields.credential_hash')=?2 ORDER BY store_index DESC LIMIT 1",
        rusqlite::params![GRANTED, digest], |row| row.get(0))
        .optional().map_err(ApiError::internal)?;
    let Some(id) = id else { return Ok(None) };
    let granted = state.store.claim_by_id(&id).map_err(ApiError::internal)?
        .ok_or_else(|| forbidden("assistant grant is absent"))?;
    session(state, &granted, transport).map(Some)
}
fn session(state: &AppState, granted: &ClaimRecord, transport: &'static str)
    -> Result<ClientSession, ApiError> {
    let policy = effective(state, granted)?;
    let source = fields(&policy)?;
    if granted.origin != state.store.origin() || policy.origin != granted.origin
        || granted.actor.as_deref() != Some(owning_person(state)?) || policy.actor.as_deref() != Some(owning_person(state)?) {
        return Err(forbidden("assistant requests require the authoritative grant issuer"));
    }
    if latest(state, &granted.subject, REVOKED)?.is_some()
        || source["expires_at_unix_ms"].as_u64().map(u128::from).unwrap_or(0) <= client_now_ms()
        || source["principal"] != PRINCIPAL || source["issuer"] != owning_person(state)?
        || !matches!(source["minimal_read_mode"].as_str(), Some("approval-once" | "caller-bound-allow")) || source["disclosure_consent"] != true {
        return Err(forbidden("assistant grant was revoked, expired, or has invalid policy"));
    }
    let scopes = source["scopes"].as_array().ok_or_else(|| forbidden("assistant scopes absent"))?
        .iter().map(|value| value.as_str().filter(|scope| SCOPES.contains(scope)).map(str::to_owned))
        .collect::<Option<BTreeSet<_>>>().ok_or_else(|| forbidden("assistant scopes invalid"))?;
    Ok(ClientSession { actor: source["session_actor"].as_str()
        .ok_or_else(|| forbidden("assistant actor absent"))?.into(), authority_actor: PRINCIPAL.into(),
        pairing_grant: Some(granted.subject.clone()),
        grant_expires_at: source["expires_at_unix_ms"].as_u64().map(u128::from), transport, custom_forms:false,
        assistant_write: None,
        conversation_blocks:false, local_peer:false, scopes })
}

pub fn authorize_route(session: &ClientSession, request: &Request<Body>)
    -> Result<(), ApiError> {
    authorization::route(session, request)
}
pub async fn inspect(State(state): State<AppState>, Extension(session): Extension<ClientSession>,
                           AxumPath(id): AxumPath<String>) -> Result<Json<Value>, ApiError> {
    trusted_issuer(&state, &session)?;
    let granted = latest(&state, &subject(&id)?, GRANTED)?
        .ok_or_else(|| ApiError::not_found("assistant grant does not exist"))?;
    Ok(Json(projection(&state, &granted)?))
}
async fn revoke_inner(State(state): State<AppState>, Extension(session): Extension<ClientSession>,
                          AxumPath(id): AxumPath<String>) -> Result<Json<Value>, ApiError> {
    let issuer = trusted_issuer(&state, &session)?;
    let _transition = TRANSITIONS.lock().await;
    let subject = subject(&id)?;
    let granted = latest(&state, &subject, GRANTED)?
        .ok_or_else(|| ApiError::not_found("assistant grant does not exist"))?;
    if granted.origin != state.store.origin() {
        return Err(forbidden("assistant revocation must run on its authoritative issuer"));
    }
    if latest(&state, &subject, REVOKED)?.is_none() {
        let previous = effective(&state, &granted)?;
        append(&state, subject, REVOKED, issuer, json!({"version":1,"grant_id":fields(&granted)?["grant_id"],
            "principal":PRINCIPAL,"issuer":issuer,"transport":session.transport,
            "timestamp":client_timestamp(client_now_ms()),"outcome":"revoked"}), Some(&previous))?;
        signal_changed(&state);
    }
    Ok(Json(projection(&state, &granted)?))
}
async fn update_policy_inner(State(state): State<AppState>, Extension(session): Extension<ClientSession>,
                                 AxumPath(id): AxumPath<String>, Json(request): Json<AssistantGrantPolicy>)
    -> Result<Json<Value>, ApiError> {
    let issuer = trusted_issuer(&state, &session)?;
    validate_policy(&request.api_version, &request.scopes, request.expires_in_seconds,
                    request.disclosure_consent, &request.consent_evidence)?;
    if !matches!(request.minimal_read_mode.as_str(), "approval-once" | "caller-bound-allow") {
        return Err(forbidden("unsupported assistant read mode"));
    }
    let _transition = TRANSITIONS.lock().await;
    let subject = subject(&id)?;
    let granted = latest(&state, &subject, GRANTED)?
        .ok_or_else(|| ApiError::not_found("assistant grant does not exist"))?;
    if granted.origin != state.store.origin() {
        return Err(forbidden("assistant policy must change on its authoritative issuer"));
    }
    if latest(&state, &subject, REVOKED)?.is_some() { return Err(forbidden("assistant grant was revoked")); }
    let previous = effective(&state, &granted)?;
    // Policy changes must not revive an expired grant: fail closed instead of extending it.
    if fields(&previous)?["expires_at_unix_ms"].as_u64().map(u128::from).unwrap_or(0) <= client_now_ms() {
        return Err(forbidden("assistant grant expired; begin a new grant instead of renewing it"));
    }
    let mut value = fields(&previous)?.clone();
    value["scopes"] = json!(request.scopes.into_iter().collect::<BTreeSet<_>>());
    value["minimal_read_mode"] = json!(request.minimal_read_mode);
    if request.recipient_allowlist.iter().any(|id| !id.starts_with("agent/") || id.len() > 256 || id.chars().any(char::is_whitespace)) {
        return Err(validation("message recipient allowlist must contain exact agent IDs"));
    }
    value["recipient_allowlist"] = json!(request.recipient_allowlist);
    value["expires_at_unix_ms"] = json!(client_now_ms() + u128::from(request.expires_in_seconds) * 1000);
    value["policy_revision"] = json!(value["policy_revision"].as_u64().unwrap_or(0) + 1);
    value["transport"] = json!(session.transport);
    value["timestamp"] = json!(client_timestamp(client_now_ms()));
    value["outcome"] = json!("renewed-policy");
    append(&state, subject, POLICY, issuer, value, Some(&previous))?;
    signal_changed(&state);
    Ok(Json(projection(&state, &granted)?))
}

pub fn device_resources(state: &AppState, snapshot: &ClientSnapshot) -> Result<Vec<Value>, ApiError> {
    let mut claims = Vec::new();
    for kind in [GRANTED, POLICY, REVOKED] {
        let mut before = snapshot.store_index.checked_add(1);
        loop {
            let page = state.store.claims_for_kind_at(kind, before, true, 256).map_err(ApiError::internal)?;
            claims.extend(page.claims);
            let Some(cursor) = page.next_cursor else { break };
            before = Some(cursor);
        }
    }
    claims.sort_by_key(|claim| claim.store_index);
    let mut grants = BTreeMap::<&str, (&ClaimRecord, bool)>::new();
    for claim in &claims {
        if claim.kind == REVOKED {
            if let Some((_, revoked)) = grants.get_mut(claim.subject.as_str()) { *revoked = true; }
        } else {
            let revoked = grants.get(claim.subject.as_str()).is_some_and(|(_, revoked)| *revoked);
            grants.insert(&claim.subject, (claim, revoked));
        }
    }
    grants.into_values().map(|(claim, revoked)| {
        let source = fields(claim)?;
        let expires = source["expires_at_unix_ms"].as_u64().map(u128::from).unwrap_or(0);
        let state = if revoked {"revoked"} else if expires <= client_snapshot_time(snapshot) {"expired"} else {"active"};
        Ok(json!({"id":source["device_id"],"kind":"device","revision":claim.id,
            "updated_at":client_timestamp(claim.accepted_at_unix_ms),"name":source["device_name"],
            "principal":PRINCIPAL,"sender_class":"external-assistant","grant_kind":"assistant",
            "grant_id":source["grant_id"],"session_actor":source["session_actor"],"state":state,
            "scopes":source["scopes"],"expires_at":client_timestamp(expires),
            "policy_revision":source["policy_revision"],"minimal_read_mode":source["minimal_read_mode"],
            "operational":{"layer":if state=="active" {"current"} else {"history"},
                "actionable":state=="active","reasons":if state=="active" {Vec::<&str>::new()} else {vec![state]}}}))
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    const ISSUER: &str = "person/operator";
    fn state(root: &Path) -> AppState { super::super::tests::test_state_named(root, "assistant-tests") }
    fn request() -> AssistantPairingBegin { serde_json::from_value(json!({"api_version":CLIENT_API_VERSION,
        "device_name":"assistant","assistant_id":PRINCIPAL,"scopes":SCOPES[..4],
        "expires_in_seconds":3600,"disclosure_consent":true,"consent_evidence":"axe decision ask recorded by the owning person"})).unwrap() }
    async fn paired(state: &AppState) -> Value {
        let Json(challenge) = begin(state.clone(), ClientSession::local(Some(ISSUER)).unwrap(), request()).await.unwrap();
        let request: PairingComplete = serde_json::from_value(json!({"api_version":CLIENT_API_VERSION,
            "code":challenge["code"],"device_public_key":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7_u8;32])})).unwrap();
        complete(state, challenge["pairing_id"].as_str().unwrap().strip_prefix("pairing/").unwrap(),
                 &request, "fabric-loopback").await.unwrap().unwrap().0
    }
    #[test]
    fn assistant_grant_ttl_contract_matches_thirty_day_policy() {
        let operations: Value = serde_json::from_str(include_str!(
            "../../../../../docs/st3/client-v0/schemas/operations.json"
        )).unwrap();
        let schema: Value = serde_json::from_str(include_str!(
            "../../../../../docs/st3/client-v0/schemas/client-v0.schema.json"
        )).unwrap();
        let maximum = operations["limits"]["assistant_grant_max_ttl_seconds"].as_u64().unwrap();
        assert_eq!(maximum, 30 * 24 * 60 * 60);
        for definition in ["AssistantPairingBegin", "AssistantGrantPolicy"] {
            assert_eq!(schema["$defs"][definition]["properties"]["expires_in_seconds"]["maximum"], maximum);
        }
        let policy = request();
        assert!(validate_policy(CLIENT_API_VERSION, &policy.scopes, maximum, true, "recorded consent").is_ok());
        for seconds in [0, maximum + 1] {
            assert!(validate_policy(CLIENT_API_VERSION, &policy.scopes, seconds, true, "recorded consent").is_err());
        }
    }
    #[tokio::test]
    async fn assistant_owner_reads_reject_forged_bound_unix_and_local_peers() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let grant = paired(&state).await;
        let app = super::super::super::router_for_transport(state, ClientTransportBoundary::Unix);
        let inspection = format!("/v1/client/assistant/grants/{}", urlencoding::encode(grant["grant"]["id"].as_str().unwrap()));
        for path in [inspection.as_str(), "/v1/client/devices"] {
            for peer in 0..3 {
                let mut request = Request::builder().uri(path)
                    .header(LOCAL_PERSON_HEADER, ISSUER).body(Body::empty()).unwrap();
                match peer {
                    0 => { request.extensions_mut().insert(super::super::super::BoundAgent("agent/test".into())); }
                    1 => { request.extensions_mut().insert(super::super::super::LocalPeer {
                        pid: 7, ancestor: Some("agent/test".into()),
                    }); }
                    _ => { request.extensions_mut().insert(super::super::super::NativeDeliveryPeer {
                        agent: "agent/test".into(), transport: "omp-channel", pid: 7, archives_inbox: false,
                    }); }
                }
                assert_eq!(app.clone().oneshot(request).await.unwrap().status(), StatusCode::FORBIDDEN, "{path}");
            }
            let mut owner_request = Request::builder().uri(path)
                .header(LOCAL_PERSON_HEADER, ISSUER).body(Body::empty()).unwrap();
            owner_request.extensions_mut().insert(super::super::super::LocalPeer { pid: 8, ancestor: None });
            let response = app.clone().oneshot(owner_request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert!(String::from_utf8(body.to_vec()).unwrap().contains(grant["grant"]["id"].as_str().unwrap()));
        }
        for path in ["/v1/client/agents", "/v1/client/devices"] {
            let mut read = Request::builder().uri(path)
                .header(LOCAL_PERSON_HEADER, "agent/test").body(Body::empty()).unwrap();
            read.extensions_mut().insert(super::super::super::BoundAgent("agent/test".into()));
            let response = app.clone().oneshot(read).await.unwrap();
            assert_eq!(response.status(), if path == "/v1/client/devices" { StatusCode::FORBIDDEN } else { StatusCode::OK });
            let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert!(!String::from_utf8(body.to_vec()).unwrap().contains(grant["grant"]["id"].as_str().unwrap()));
        }
    }
    #[tokio::test]
    async fn assistant_grant_administration_rejects_paired_person_over_unix() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let grant = paired(&state).await;
        let credential = "paired-owner-test-credential";
        state.store.append_claim(&ClaimInput {
            subject: "custom/client/paired-owner".into(),
            kind: "custom.client.pairing-completed".into(), actor: Some(ISSUER.into()),
            fields: serde_json::from_value(json!({
                "credential_hash": credential_digest(credential),
                "session_actor": "person/operator/session/phone", "person_id": ISSUER,
                "scopes": ALL_SCOPES, "expires_at_unix_ms": u64::MAX,
            })).unwrap(), evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
        let app = super::super::super::router_for_transport(state, ClientTransportBoundary::Unix);
        let path = format!("/v1/client/assistant/grants/{}", urlencoding::encode(grant["grant"]["id"].as_str().unwrap()));
        for (method, suffix) in [("GET", ""), ("POST", "/revoke")] {
            let response = app.clone().oneshot(Request::builder().method(method).uri(format!("{path}{suffix}"))
                .header(AUTHORIZATION, format!("Bearer {credential}"))
                .header(LOCAL_PERSON_HEADER, ISSUER).body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method}");
        }
    }
    #[tokio::test]
    async fn assistant_issuance_rejects_remote_agents_and_non_owner() {
        let root = tempfile::tempdir().unwrap(); let state = state(root.path());
        for (actor, transport) in [(ISSUER,"fabric-loopback"),("agent/test","unix"),("person/other","unix")] {
            let session = ClientSession::for_tests(actor,actor,transport);
            assert_eq!(begin(state.clone(),session,request()).await.unwrap_err().status,StatusCode::FORBIDDEN);
        }
    }
    #[tokio::test]
    async fn assistant_issuance_rejects_expiry_scope_and_consent_escalation() {
        let root = tempfile::tempdir().unwrap(); let state = state(root.path());
        for seconds in [0,2_592_001] { let mut req=request(); req.expires_in_seconds=seconds;
            assert!(begin(state.clone(),ClientSession::local(Some(ISSUER)).unwrap(),req).await.is_err()); }
        let mut req=request(); req.scopes.push("control.messages".into());
        assert!(begin(state.clone(),ClientSession::local(Some(ISSUER)).unwrap(),req).await.is_err());
        let mut req=request(); req.disclosure_consent=false;
        assert!(begin(state,ClientSession::local(Some(ISSUER)).unwrap(),req).await.is_err());
    }
    #[tokio::test]
    async fn assistant_session_is_never_person_and_has_no_signing_chain() {
        let root=tempfile::tempdir().unwrap(); let state=state(root.path()); let paired=paired(&state).await;
        let session=authenticate(&state,&credential_digest(paired["credential"].as_str().unwrap()),"unix").unwrap().unwrap();
        assert_eq!(session.authority_actor,PRINCIPAL); assert!(!acting_party(&session));
        assert!(session.actor.starts_with("assistant/chatgpt/session/assistant-grant/"));
        assert!(paired.get("device_key_chain").is_none()); assert!(paired.get("person_id").is_none());
        assert!(!serde_json::to_string(&paired["grant"]).unwrap().contains("credential"));
        assert_eq!(paired["grant"]["consent_evidence"],"axe decision ask recorded by the owning person");
        assert!(state.store.claims_for_kind_at("principal.key-granted",None,true,10).unwrap().claims.is_empty());
        assert!(capabilities(&state,&session).iter()
            .filter(|capability| capability["id"].as_str().is_some_and(|id| id.starts_with("message.")))
            .all(|capability| capability["state"] != "granted"));
        let response=super::super::super::router_for_transport(state.clone(),ClientTransportBoundary::Unix)
            .oneshot(Request::builder().uri("/v1/client/capabilities")
                .header(AUTHORIZATION,format!("Bearer {}",paired["credential"].as_str().unwrap()))
                .body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(),StatusCode::FORBIDDEN);
        let body=axum::body::to_bytes(response.into_body(),usize::MAX).await.unwrap();
        assert!(!String::from_utf8(body.to_vec()).unwrap().contains("max_message_bytes"));
    }
    #[tokio::test]
    async fn assistant_revocation_and_expiry_survive_restart() {
        let root=tempfile::tempdir().unwrap(); let state1=state(root.path()); let paired1=paired(&state1).await;
        let digest=credential_digest(paired1["credential"].as_str().unwrap());
        let id=paired1["grant"]["id"].as_str().unwrap().to_owned();
        let _ = revoke(State(state1.clone()),Extension(ClientSession::local(Some(ISSUER)).unwrap()),AxumPath(id)).await.unwrap();
        assert!(authenticate(&state(root.path()),&digest,"fabric-loopback").is_err());
        let paired2=paired(&state1).await; let granted=latest(&state1,&subject(paired2["grant"]["id"].as_str().unwrap()).unwrap(),GRANTED).unwrap().unwrap();
        let mut expired=fields(&granted).unwrap().clone(); expired["expires_at_unix_ms"]=json!(1);
        append(&state1,granted.subject.clone(),POLICY,ISSUER,expired,Some(&granted)).unwrap();
        assert!(authenticate(&state(root.path()),&credential_digest(paired2["credential"].as_str().unwrap()),"unix").is_err());
    }
    #[tokio::test]
    async fn assistant_pairing_code_is_single_use_and_completion_cannot_change_policy() {
        let root=tempfile::tempdir().unwrap(); let state=state(root.path());
        let Json(challenge)=begin(state.clone(),ClientSession::local(Some(ISSUER)).unwrap(),request()).await.unwrap();
        let value=json!({"api_version":CLIENT_API_VERSION,"code":challenge["code"],
            "device_public_key":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([8_u8;32])});
        let mut escalated=value.clone(); escalated["scopes"]=json!(["control.messages"]);
        assert!(serde_json::from_value::<PairingComplete>(escalated).is_err());
        let request: PairingComplete=serde_json::from_value(value).unwrap();
        let id=challenge["pairing_id"].as_str().unwrap().strip_prefix("pairing/").unwrap();
        let invalid_key = PairingComplete {
            api_version: request.api_version.clone(), code: request.code.clone(),
            device_public_key: hex::encode([8_u8; 32]), key_storage: None,
        };
        assert_eq!(complete(&state, id, &invalid_key, "fabric-loopback").await.unwrap_err().code, "validation-failed");
        let _ = complete(&state,id,&request,"fabric-loopback").await.unwrap().unwrap();
        assert_eq!(complete(&state,id,&request,"fabric-loopback").await.unwrap_err().status,StatusCode::FORBIDDEN);
    }
    #[tokio::test]
    async fn assistant_four_scope_grant_denies_generic_and_native_routes() {
        let root=tempfile::tempdir().unwrap(); let state=state(root.path()); let paired=paired(&state).await;
        for app in [super::super::super::router_for_transport(state.clone(),ClientTransportBoundary::Unix),
                    super::super::super::fabric_router(state.clone())] {
            for (method,path) in [("GET","/v1/client/agents"),("GET","/v1/client/messages"),
                ("GET","/v1/client/events"),("GET","/v1/client/terminals/test/stream"),
                ("POST","/v1/client/actions"),("POST","/v1/client/observations"),
                ("POST","/v1/client/pairings"),("POST","/v1/messages"),
                ("POST","/v1/claims"),("GET","/v1/status")] {
                let response=app.clone().oneshot(Request::builder().method(method).uri(path)
                    .header(AUTHORIZATION,format!("Bearer {}",paired["credential"].as_str().unwrap()))
                    .header(LOCAL_PERSON_HEADER,ISSUER).body(Body::empty()).unwrap()).await.unwrap();
                assert_eq!(response.status(),StatusCode::FORBIDDEN,"{method} {path}");
            }
        }
    }
    #[tokio::test]
    async fn assistant_reserved_claims_and_audit_do_not_leak_credentials() {
        let root=tempfile::tempdir().unwrap(); let state=state(root.path());
        let paired=paired(&state).await;
        let forged=ClaimInput {subject:subject(paired["grant"]["id"].as_str().unwrap()).unwrap(),
            kind:POLICY.into(),actor:Some(ISSUER.into()),fields:BTreeMap::new(),evidence:Vec::new(),
            expected_subject:None,idempotency_key:None};
        assert_eq!(state.store.append_client_claim(&forged).unwrap_err().code,"claim-write-forbidden");
        let _=begin(state.clone(),ClientSession::for_tests("agent/test","agent/test","unix"),request()).await;
        let audits=state.store.claims_for_kind_at("custom.client.assistant.v1.audit",None,true,100).unwrap().claims;
        assert!(audits.iter().any(|claim| claim.body["fields"]["outcome"]=="denied"));
        let serialized=serde_json::to_string(&audits).unwrap();
        assert!(!serialized.contains(paired["credential"].as_str().unwrap()));
        assert!(!serialized.contains("code_hash"));
    }
    #[tokio::test]
    async fn assistant_completion_uses_generated_client_and_private_assistant_profile() {
        let root=tempfile::tempdir().unwrap(); let state=state(root.path());
        let Json(challenge)=begin(state.clone(),ClientSession::local(Some(ISSUER)).unwrap(),request()).await.unwrap();
        let listener=tokio::net::TcpListener::bind(("127.0.0.1",0)).await.unwrap();
        let endpoint=format!("http://{}",listener.local_addr().unwrap());
        let app=super::super::super::fabric_router(state.clone());
        let server=tokio::spawn(async move {axum::serve(listener,app).await.unwrap()});
        let profile=root.path().join("assistant-devices.json");
        let key=st3_client::device::SigningKey::generate(st3_client::device::KeyAlgorithm::Ed25519).unwrap();
        let device=st3_client::device::complete_assistant(&profile,&endpoint,
            challenge["pairing_id"].as_str().unwrap(),challenge["code"].as_str().unwrap(),key,false).await.unwrap();
        assert_eq!(serde_json::to_value(&device.session.principal).unwrap(),PRINCIPAL);
        assert!(device.client().unwrap().capabilities().await.is_err());
        assert_eq!(fs::metadata(&profile).unwrap().permissions().mode() & 0o777,0o600);
        assert!(!root.path().join("devices.json").exists());
        let Json(grant)=inspect(State(state),Extension(ClientSession::local(Some(ISSUER)).unwrap()),
            AxumPath(device.session.grant.id)).await.unwrap();
        assert_eq!(grant["principal"],PRINCIPAL);
        server.abort();
    }
    #[tokio::test]
    async fn assistant_policy_is_local_explicit_and_rechecks_every_session() {
        let root=tempfile::tempdir().unwrap(); let state=state(root.path()); let paired=paired(&state).await;
        let id=paired["grant"]["id"].as_str().unwrap().to_owned();
        let policy=|| serde_json::from_value::<AssistantGrantPolicy>(json!({
            "api_version":CLIENT_API_VERSION,"scopes":SCOPES,"expires_in_seconds":86400,
            "disclosure_consent":true,"consent_evidence":"axe decision ask recorded by the owning person","minimal_read_mode":"approval-once"})).unwrap();
        for (actor,transport) in [(ISSUER,"fabric-loopback"),("agent/test","unix"),("person/other","unix")] {
            let session=ClientSession::for_tests(actor,actor,transport);
            assert_eq!(update_policy(State(state.clone()),Extension(session.clone()),AxumPath(id.clone()),Json(policy())).await.unwrap_err().status,StatusCode::FORBIDDEN);
            assert_eq!(revoke(State(state.clone()),Extension(session),AxumPath(id.clone())).await.unwrap_err().status,StatusCode::FORBIDDEN);
        }
        let Json(changed)=update_policy(State(state.clone()),Extension(ClientSession::local(Some(ISSUER)).unwrap()),AxumPath(id.clone()),Json(policy())).await.unwrap();
        assert_eq!(changed["policy_revision"],2);
        let session=authenticate(&state,&credential_digest(paired["credential"].as_str().unwrap()),"fabric-loopback").unwrap().unwrap();
        assert!(session.allows("read.assistant.terminal")); assert!(!session.allows("control.messages"));
        let mut allow=policy(); allow.minimal_read_mode="caller-bound-allow".into();
        let Json(updated)=update_policy(State(state),Extension(ClientSession::local(Some(ISSUER)).unwrap()),AxumPath(id),Json(allow)).await.unwrap();
        assert_eq!(updated["minimal_read_mode"],"caller-bound-allow");
    }
    #[tokio::test]
    async fn assistant_policy_update_cannot_revive_an_expired_or_revoked_grant() {
        let root=tempfile::tempdir().unwrap(); let state=state(root.path()); let paired=paired(&state).await;
        let id=paired["grant"]["id"].as_str().unwrap().to_owned();
        let policy=|| serde_json::from_value::<AssistantGrantPolicy>(json!({
            "api_version":CLIENT_API_VERSION,"scopes":SCOPES,"expires_in_seconds":86400,
            "disclosure_consent":true,"consent_evidence":"axe decision ask recorded by the owning person","minimal_read_mode":"approval-once"})).unwrap();
        let granted=latest(&state,&subject(&id).unwrap(),GRANTED).unwrap().unwrap();
        let mut expired=fields(&granted).unwrap().clone(); expired["expires_at_unix_ms"]=json!(1);
        append(&state,granted.subject.clone(),POLICY,ISSUER,expired,Some(&granted)).unwrap();
        assert_eq!(update_policy(State(state.clone()),Extension(ClientSession::local(Some(ISSUER)).unwrap()),
            AxumPath(id.clone()),Json(policy())).await.unwrap_err().status,StatusCode::FORBIDDEN);
        let _ = revoke(State(state.clone()),Extension(ClientSession::local(Some(ISSUER)).unwrap()),AxumPath(id.clone())).await.unwrap();
        assert_eq!(update_policy(State(state),Extension(ClientSession::local(Some(ISSUER)).unwrap()),
            AxumPath(id),Json(policy())).await.unwrap_err().status,StatusCode::FORBIDDEN);
    }
    #[tokio::test]
    async fn assistant_policy_update_uses_renewed_expiry_not_original_expiry() {
        let root=tempfile::tempdir().unwrap(); let state=state(root.path()); let paired=paired(&state).await;
        let id=paired["grant"]["id"].as_str().unwrap().to_owned();
        let granted=latest(&state,&subject(&id).unwrap(),GRANTED).unwrap().unwrap();
        let mut expired=fields(&granted).unwrap().clone(); expired["expires_at_unix_ms"]=json!(1);
        let original=append(&state,granted.subject.clone(),GRANTED,ISSUER,expired,Some(&granted)).unwrap();
        let mut renewed=fields(&original).unwrap().clone();
        renewed["expires_at_unix_ms"]=json!(client_now_ms()+86_400_000);
        renewed["policy_revision"]=json!(2);
        append(&state,granted.subject.clone(),POLICY,ISSUER,renewed,Some(&original)).unwrap();
        let policy=serde_json::from_value::<AssistantGrantPolicy>(json!({
            "api_version":CLIENT_API_VERSION,"scopes":SCOPES,"expires_in_seconds":86400,
            "disclosure_consent":true,"consent_evidence":"recorded owner consent","minimal_read_mode":"approval-once"})).unwrap();
        let Json(changed)=update_policy(State(state),Extension(ClientSession::local(Some(ISSUER)).unwrap()),
            AxumPath(id),Json(policy)).await.unwrap();
        assert_eq!(changed["policy_revision"],3);
    }
    #[tokio::test]
    async fn assistant_missing_or_changed_owner_configuration_fails_closed() {
        let root=tempfile::tempdir().unwrap(); let mut state=state(root.path());
        state.owning_person=None;
        assert_eq!(begin(state.clone(),ClientSession::local(Some(ISSUER)).unwrap(),request()).await.unwrap_err().status,StatusCode::FORBIDDEN);
        state.owning_person=Some(ISSUER.into());
        let paired=paired(&state).await;
        state.owning_person=Some("person/other".into());
        assert!(authenticate(&state,&credential_digest(paired["credential"].as_str().unwrap()),"unix").is_err());
    }
}
