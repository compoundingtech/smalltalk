//! One-use SCG read consent is verified against an independently pinned attestor.
use super::*;
use axum::extract::OriginalUri;
use axum::http::HeaderMap;
use smallclaims::hash::canonical_json_text;

use super::reads::{DEFAULT_RESPONSE_BYTES, MAX_RESPONSE_BYTES};
fn default_response_bytes() -> usize { DEFAULT_RESPONSE_BYTES }
fn max_response_bytes() -> usize { MAX_RESPONSE_BYTES }
const HEADER: &str = "x-st3-scg-read-authorization";
const USED: &str = "custom.client.assistant.v1.read-used";
const OUTCOME: &str = "custom.client.assistant.v1.read-outcome";
const ROUTES: &[(&str, &str)] = &[
    ("summary", "read.assistant.summary"), ("attention", "read.assistant.attention"),
    ("attention-content", "read.assistant.attention"),
    ("attention-rich", "read.assistant.attention"), ("missions-rich", "read.assistant.missions"),
    ("agents-rich", "read.assistant.agents"),
    ("missions", "read.assistant.missions"), ("agents", "read.assistant.agents"),
    ("conversation-content", "read.assistant.conversation"),
    ("terminal-content", "read.assistant.terminal"),
];

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReadTrust {
    issuer: String,
    key_id: String,
    attestor_public_key: String,
    shim_public_key: String,
    #[serde(default = "default_response_bytes")]
    default_response_bytes: usize,
    #[serde(default = "max_response_bytes")]
    max_response_bytes: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadTrustRequest {
    api_version: String,
    issuer: String,
    key_id: String,
    attestor_public_key: String,
    shim_public_key: String,
    #[serde(default = "default_response_bytes")]
    default_response_bytes: usize,
    #[serde(default = "max_response_bytes")]
    max_response_bytes: usize,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReadPayload {
    issuer: String,
    identity: String,
    assistant_id: String,
    grant_id: String,
    policy_revision: u64,
    key_id: String,
    approval_id: String,
    execution_id: String,
    projection: String,
    parameters: BTreeMap<String, String>,
    max_response_bytes: usize,
    request_digest: String,
    expires_at_unix_ms: u64,
    nonce: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadAuthorization {
    payload: ReadPayload,
    signature: String,
}
/// Carried to the enclosing response: never stores or publishes private projection bytes.
#[derive(Clone, Debug)]
pub struct ReadReceipt {
    subject: String,
    consumed_claim: String,
    grant_subject: String,
    actor: String,
    transport: &'static str,
    policy_revision: u64,
    proof_expiry: u64,
    pub max_response_bytes: usize,
}

fn key(value: &str) -> Result<Vec<u8>, ApiError> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(value)
        .ok().filter(|bytes| bytes.len() == 32)
        .ok_or_else(|| forbidden("the read trust pin must be a canonical Ed25519 public key"))
}
fn validate_trust(trust: &ReadTrust) -> Result<(), ApiError> {
    if trust.issuer.is_empty() || trust.issuer.len() > 128
        || trust.key_id.is_empty() || trust.key_id.len() > 128 {
        return Err(validation("the read attestor requires explicit issuer and key identifiers"));
    }
    if trust.default_response_bytes == 0 || trust.default_response_bytes > DEFAULT_RESPONSE_BYTES
        || trust.max_response_bytes > MAX_RESPONSE_BYTES || trust.default_response_bytes > trust.max_response_bytes {
        return Err(validation("read budgets may only be configured downward within the 4/16 KiB defaults and ceilings"));
    }
    if key(&trust.attestor_public_key)? == key(&trust.shim_public_key)? {
        return Err(forbidden("the approval attestor must be separately pinned from the shim key"));
    }
    Ok(())
}
pub async fn pin(State(state): State<AppState>, Extension(session): Extension<ClientSession>,
                       AxumPath(id): AxumPath<String>, Json(request): Json<ReadTrustRequest>)
    -> Result<Json<Value>, ApiError> {
    let issuer = trusted_issuer(&state, &session)?;
    if request.api_version != CLIENT_API_VERSION { return Err(validation("invalid API version")); }
    let trust = ReadTrust {issuer:request.issuer,key_id:request.key_id,
        attestor_public_key:request.attestor_public_key,shim_public_key:request.shim_public_key,
        default_response_bytes:request.default_response_bytes,max_response_bytes:request.max_response_bytes};
    validate_trust(&trust)?;
    let _transition = TRANSITIONS.lock().await;
    let subject = subject(&id)?;
    let granted = latest(&state, &subject, GRANTED)?
        .ok_or_else(|| ApiError::not_found("assistant grant does not exist"))?;
    if granted.origin != state.store.origin() || latest(&state, &subject, REVOKED)?.is_some() {
        return Err(forbidden("read trust requires the active grant's authoritative issuer"));
    }
    let previous = effective(&state, &granted)?;
    let mut value = fields(&previous)?.clone();
    value["read_trust"] = serde_json::to_value(trust).map_err(ApiError::internal)?;
    value["policy_revision"] = json!(value["policy_revision"].as_u64().unwrap_or(0) + 1);
    value["timestamp"] = json!(client_timestamp(client_now_ms()));
    value["transport"] = json!(session.transport);
    value["outcome"] = json!("read-attestor-pinned");
    let result = append(&state, subject, POLICY, issuer, value, Some(&previous));
    audit(&state, "pin-read-attestor", &session.actor, session.transport, Some(&id), &result)?;
    result?;
    signal_changed(&state);
    Ok(Json(projection(&state, &granted)?))
}

pub fn route(session: &ClientSession, request: &Request<Body>) -> Result<(), ApiError> {
    if request.uri().path() == "/v1/client/assistant/propose" && request.method() == axum::http::Method::POST {
        return Ok(());
    }
    let projection = request.uri().path().strip_prefix("/v1/client/assistant/");
    let Some((_, scope)) = ROUTES.iter().find(|(route, _)| projection == Some(*route))
        else { return Err(forbidden("assistants may access only explicit proof-bearing read projections")) };
    if request.method() != axum::http::Method::GET || !session.allows(scope)
        || request.headers().contains_key(axum::http::header::UPGRADE) {
        return Err(forbidden("assistant read requires its exact scope"));
    }
    Ok(())
}
fn parameters(uri: &axum::http::Uri) -> Result<BTreeMap<String,String>, ApiError> {
    let mut result = BTreeMap::new();
    fn decode(value: &str) -> Result<String, ApiError> {
        let value = if value.contains('+') {std::borrow::Cow::Owned(value.replace('+', " "))}
            else {std::borrow::Cow::Borrowed(value)};
        urlencoding::decode(&value).map(std::borrow::Cow::into_owned)
            .map_err(|_| validation("assistant query parameters must be valid UTF-8"))
    }
    for parameter in uri.query().unwrap_or("").split('&').filter(|parameter| !parameter.is_empty()) {
        let (name,value)=parameter.split_once('=').unwrap_or((parameter,""));
        if result.insert(decode(name)?, decode(value)?).is_some() {
            return Err(validation("assistant read parameters must not be repeated"));
        }
    }
    Ok(result)
}
fn request_digest(path: &str, parameters: &BTreeMap<String,String>) -> Result<String,ApiError> {
    let canonical = canonical_json_text(&json!({"method":"GET","path":path,"parameters":parameters}))
        .map_err(ApiError::internal)?;
    Ok(credential_digest(&canonical))
}
fn signing_bytes(payload: &ReadPayload) -> Result<String,ApiError> {
    let canonical = canonical_json_text(&serde_json::to_value(payload).map_err(ApiError::internal)?)
        .map_err(ApiError::internal)?;
    Ok(format!("st3.scg-read.v1\n{canonical}"))
}
fn proof(headers: &HeaderMap) -> Result<ReadAuthorization,ApiError> {
    let value = headers.get(HEADER).and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 16384)
        .ok_or_else(|| forbidden("SCG read authorization is required"))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(value)
        .map_err(|_| forbidden("SCG read authorization is malformed"))?;
    serde_json::from_slice(&bytes).map_err(|_| forbidden("SCG read authorization is malformed"))
}
fn budget(parameters: &BTreeMap<String,String>, trust: &ReadTrust) -> Result<usize,ApiError> {
    let requested = parameters.get("max_response_bytes").map(|value| value.parse::<usize>())
        .transpose().map_err(|_| validation("response budget must be an integer"))?
        .unwrap_or(trust.default_response_bytes);
    if requested == 0 || requested > trust.max_response_bytes {
        return Err(validation("assistant response budget must be positive and at most 16 KiB"));
    }
    Ok(requested)
}
fn verify(state: &AppState, session: &ClientSession, path: &str, parameters: &BTreeMap<String,String>,
          authorization: &ReadAuthorization) -> Result<(ClaimRecord,ReadReceipt),ApiError> {
    let subject = session.pairing_grant.as_deref().ok_or_else(|| forbidden("assistant grant absent"))?;
    let granted = latest(state, subject, GRANTED)?.ok_or_else(|| forbidden("assistant grant absent"))?;
    let current = super::session(state, &granted, session.transport)?;
    let projection = path.strip_prefix("/v1/client/assistant/")
        .ok_or_else(|| forbidden("invalid assistant projection"))?;
    let (_,scope) = ROUTES.iter().find(|(route,_)| *route==projection)
        .ok_or_else(|| forbidden("invalid assistant projection"))?;
    if !current.allows(scope) { return Err(forbidden("assistant projection scope is not granted")); }
    let policy = effective(state,&granted)?;
    let source = fields(&policy)?;
    let trust: ReadTrust = serde_json::from_value(source["read_trust"].clone())
        .map_err(|_| forbidden("a separately pinned read attestor is required"))?;
    validate_trust(&trust)?;
    let payload = &authorization.payload;
    let requested = budget(parameters,&trust)?;
    if payload.issuer!=trust.issuer || payload.key_id!=trust.key_id || payload.identity!="chatgpt"
        || payload.assistant_id!=PRINCIPAL || payload.grant_id!=source["grant_id"].as_str().unwrap_or("")
        || payload.policy_revision!=source["policy_revision"].as_u64().unwrap_or(0)
        || payload.projection!=projection || payload.parameters!=*parameters
        || payload.max_response_bytes!=requested || payload.request_digest!=request_digest(path,parameters)?
        || u128::from(payload.expires_at_unix_ms)<=client_now_ms()
        || u128::from(payload.expires_at_unix_ms)>source["expires_at_unix_ms"].as_u64().map(u128::from).unwrap_or(0)
        || payload.nonce.is_empty() || payload.nonce.len()>128
        || payload.approval_id.is_empty() || payload.approval_id.len()>128
        || payload.execution_id.is_empty() || payload.execution_id.len()>128 {
        return Err(forbidden("SCG read proof does not bind this exact grant, policy, projection, request, budget and approval"));
    }
    let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&authorization.signature)
        .map_err(|_| forbidden("read proof signature is malformed"))?;
    let message = signing_bytes(payload)?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519,key(&trust.attestor_public_key)?)
        .verify(message.as_bytes(),&signature).map_err(|_| forbidden("read proof is not signed by the pinned approval attestor"))?;
    let mut nonce_hash = Sha256::new();
    nonce_hash.update((payload.issuer.len() as u64).to_be_bytes());
    nonce_hash.update(payload.issuer.as_bytes());
    nonce_hash.update(payload.nonce.as_bytes());
    let nonce_subject = format!("custom/client/assistant-v1-read-{}",hex::encode(nonce_hash.finalize()));
    if latest(state,&nonce_subject,USED)?.is_some() {
        return Err(forbidden("the SCG read approval has already been consumed; private results cannot replay"));
    }
    let receipt = ReadReceipt {subject:nonce_subject,consumed_claim:String::new(),grant_subject:subject.into(),
        actor:current.actor,transport:session.transport,policy_revision:payload.policy_revision,
        proof_expiry:payload.expires_at_unix_ms,max_response_bytes:requested};
    Ok((granted,receipt))
}

pub async fn read(State(state): State<AppState>, Extension(session): Extension<ClientSession>,
                               Extension(snapshot): Extension<ClientSnapshot>, OriginalUri(uri): OriginalUri,
                               headers: HeaderMap) -> Result<(Extension<ReadReceipt>,Extension<ClientSnapshot>,Json<Value>),ApiError> {
    let result=read_inner(State(state.clone()),Extension(session.clone()),Extension(snapshot),OriginalUri(uri),headers).await;
    if result.is_err() {
        let granted=session.pairing_grant.as_deref().map(|subject| latest(&state,subject,GRANTED)).transpose()?.flatten();
        let grant_id=granted.as_ref().and_then(|claim| claim.body.pointer("/fields/grant_id")).and_then(Value::as_str);
        audit(&state,"read",&session.actor,session.transport,grant_id,&result)?;
    }
    result
}

async fn read_inner(State(state): State<AppState>, Extension(session): Extension<ClientSession>,
                        Extension(snapshot): Extension<ClientSnapshot>, OriginalUri(uri): OriginalUri,
                        headers: HeaderMap) -> Result<(Extension<ReadReceipt>,Extension<ClientSnapshot>,Json<Value>),ApiError> {
    let grant_subject = session.pairing_grant.as_deref().ok_or_else(|| forbidden("assistant grant absent"))?;
    let granted = latest(&state, grant_subject, GRANTED)?.ok_or_else(|| forbidden("assistant grant absent"))?;
    let policy = effective(&state, &granted)?;
    if fields(&policy)?["minimal_read_mode"] == "caller-bound-allow" {
        let parameters = parameters(&uri)?;
        let requested = parameters.get("max_response_bytes").map(|v| v.parse::<usize>())
            .transpose().map_err(|_| validation("response budget must be an integer"))?.unwrap_or(DEFAULT_RESPONSE_BYTES);
        if requested == 0 || requested > MAX_RESPONSE_BYTES { return Err(validation("response budget must be 1..16384")); }
        let current = super::session(&state, &granted, session.transport)?;
        let projection = uri.path().strip_prefix("/v1/client/assistant/").ok_or_else(|| forbidden("invalid projection"))?;
        let (_, scope) = ROUTES.iter().find(|(p,_)| *p == projection).ok_or_else(|| forbidden("invalid projection"))?;
        if !current.allows(scope) { return Err(forbidden("assistant projection scope is not granted")); }
        let receipt_subject = format!("custom/client/assistant-v1-read-{}", uuid::Uuid::new_v4());
        let consumed = append(&state, receipt_subject.clone(), USED, &current.actor, json!({
            "grant_id": fields(&policy)?["grant_id"], "mode":"caller-bound-allow",
            "projection":projection, "request_digest":request_digest(uri.path(), &parameters)?,
            "max_response_bytes":requested, "timestamp":client_timestamp(client_now_ms())
        }), None)?;
        let receipt = ReadReceipt { subject:receipt_subject, consumed_claim:consumed.id,
            grant_subject:grant_subject.into(), actor:current.actor, transport:session.transport,
            policy_revision:fields(&policy)?["policy_revision"].as_u64().unwrap_or(0),
            proof_expiry:fields(&policy)?["expires_at_unix_ms"].as_u64().unwrap_or(0), max_response_bytes:requested };
        let (snapshot,value) = super::reads::project(&state,&snapshot,projection,&parameters,requested).await?;
        return Ok((Extension(receipt),Extension(snapshot),Json(value)));
    }
    let authorization = proof(&headers)?;
    let parameters = parameters(&uri)?;
    let _transition = TRANSITIONS.lock().await;
    let (_,mut receipt) = verify(&state,&session,uri.path(),&parameters,&authorization)?;
    // Consumption precedes any source access and is fenced in the durable authority namespace.
    let consumed = append(&state,receipt.subject.clone(),USED,&receipt.actor,json!({
        "version":1,"grant_id":authorization.payload.grant_id,"policy_revision":receipt.policy_revision,
        "issuer":authorization.payload.issuer,"attestor_key_id":authorization.payload.key_id,
        "approval_id":authorization.payload.approval_id,"execution_id":authorization.payload.execution_id,
        "request_digest":authorization.payload.request_digest,"nonce_digest":credential_digest(&authorization.payload.nonce),
        "projection":authorization.payload.projection,"max_response_bytes":receipt.max_response_bytes,
        "transport":receipt.transport,"timestamp":client_timestamp(client_now_ms()),"outcome":"consumed"
    }),None).map_err(|error| if error.code=="stale-subject" {
        forbidden("the SCG read approval has already been consumed") } else {error})?;
    receipt.consumed_claim=consumed.id.clone();
    let (snapshot,value) = match super::reads::project(&state,&snapshot,&authorization.payload.projection,&parameters,receipt.max_response_bytes).await {
        Ok(result) => result,
        Err(error) => {
            append(&state,receipt.subject.clone(),OUTCOME,&receipt.actor,json!({"version":1,
                "outcome":"denied-projection","denial_code":error.code,"returned_bytes":0,
                "timestamp":client_timestamp(client_now_ms())}),Some(&consumed))?;
            return Err(error);
        }
    };
    Ok((Extension(receipt),Extension(snapshot),Json(value)))
}

pub async fn finish(state: &AppState, receipt: &ReadReceipt, bytes: &[u8],
                          within_budget: bool) -> Result<(),ApiError> {
    let _transition=TRANSITIONS.lock().await;
    // The consumed approval remains authoritative even if the grant expires or is revoked
    // while the projection is being built. Record that final denial before returning it.
    let authorization = (|| -> Result<(), ApiError> {
        let granted=latest(state,&receipt.grant_subject,GRANTED)?.ok_or_else(|| forbidden("assistant grant absent"))?;
        super::session(state,&granted,receipt.transport)?;
        let policy=effective(state,&granted)?;
        if fields(&policy)?["policy_revision"].as_u64()!=Some(receipt.policy_revision)
            || u128::from(receipt.proof_expiry)<=client_now_ms() {
            return Err(forbidden("read authorization expired or policy changed before delivery"));
        }
        Ok(())
    })();
    let consumed=latest(state,&receipt.subject,USED)?.ok_or_else(|| forbidden("read consent receipt absent"))?;
    if consumed.id!=receipt.consumed_claim {return Err(forbidden("read consent receipt changed"));}
    let delivered=within_budget && authorization.is_ok();
    append(state,receipt.subject.clone(),OUTCOME,&receipt.actor,json!({"version":1,
        "policy_revision":receipt.policy_revision,"transport":receipt.transport,
        "max_response_bytes":receipt.max_response_bytes,
        "returned_bytes":if delivered {bytes.len()} else {0},"budget_used":bytes.len(),
        "response_digest":if delivered {Some(hex::encode(Sha256::digest(bytes)))} else {None},
        "denial_code":authorization.as_ref().err().map(|error| error.code.as_str())
            .or(if within_budget {None} else {Some("response-budget-exceeded")}),
        "outcome":if authorization.is_err() {"denied-authorization"} else if within_budget {"delivered"} else {"denied-budget"},
        "timestamp":client_timestamp(client_now_ms())}),Some(&consumed))?;
    if authorization.is_err() {
        let grant_id=consumed.body.pointer("/fields/grant_id").and_then(Value::as_str);
        audit(state,"read",&receipt.actor,receipt.transport,grant_id,&authorization)?;
    }
    authorization
}

#[cfg(test)]
#[path = "authorization_tests.rs"]
mod tests;
