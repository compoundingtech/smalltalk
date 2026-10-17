use super::*;
use ring::{rand::SystemRandom, signature::{Ed25519KeyPair, KeyPair as _}};

const OWNER: &str = "person/operator";
struct Fixture {
    root: tempfile::TempDir,
    state: AppState,
    credential: String,
    grant: String,
    revision: u64,
    attestor: Ed25519KeyPair,
    shim: Ed25519KeyPair,
}
fn signing_key() -> Ed25519KeyPair {
    let bytes=Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    Ed25519KeyPair::from_pkcs8(bytes.as_ref()).unwrap()
}
async fn fixture(scopes: &[&str], pin_trust: bool) -> Fixture {
    let root=tempfile::tempdir().unwrap();
    let state=super::super::super::tests::test_state_named(root.path(),"assistant-proof-tests");
    let request=serde_json::from_value(json!({"api_version":CLIENT_API_VERSION,
        "device_name":"assistant","assistant_id":PRINCIPAL,"scopes":scopes,
        "disclosure_consent":true,"consent_evidence":"axe:igubz7"})).unwrap();
    let Json(challenge)=begin(state.clone(),ClientSession::local(Some(OWNER)).unwrap(),request).await.unwrap();
    let device_key=signing_key();
    let request=serde_json::from_value(json!({"api_version":CLIENT_API_VERSION,"code":challenge["code"],
        "device_public_key":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(device_key.public_key().as_ref())})).unwrap();
    let id=challenge["pairing_id"].as_str().unwrap().strip_prefix("pairing/").unwrap();
    let Json(paired)=complete(&state,id,&request,"fabric-loopback").await.unwrap().unwrap();
    let mut fixture=Fixture {root,state,credential:paired["credential"].as_str().unwrap().into(),
        grant:paired["grant"]["id"].as_str().unwrap().into(),revision:1,attestor:signing_key(),shim:signing_key()};
    if pin_trust {
        let Json(grant)=pin(State(fixture.state.clone()),Extension(ClientSession::local(Some(OWNER)).unwrap()),
            AxumPath(fixture.grant.clone()),Json(fixture.trust_request())).await.unwrap();
        fixture.revision=grant["policy_revision"].as_u64().unwrap();
    }
    fixture
}
impl Fixture {
    fn trust_request(&self) -> ReadTrustRequest {
        ReadTrustRequest {api_version:CLIENT_API_VERSION.into(),issuer:"scg/approval-host".into(),
            key_id:"approval-key/test".into(),
            attestor_public_key:base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.attestor.public_key().as_ref()),
            shim_public_key:base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.shim.public_key().as_ref()),
            default_response_bytes:DEFAULT_RESPONSE_BYTES,max_response_bytes:MAX_RESPONSE_BYTES}
    }
    fn payload(&self, route: &str, query: &str) -> ReadPayload {
        let uri=format!("/v1/client/assistant/{route}{query}").parse::<axum::http::Uri>().unwrap();
        let parameters=parameters(&uri).unwrap();
        ReadPayload {issuer:"scg/approval-host".into(),identity:"chatgpt".into(),assistant_id:PRINCIPAL.into(),
            grant_id:self.grant.clone(),policy_revision:self.revision,key_id:"approval-key/test".into(),
            approval_id:"approval/test".into(),execution_id:"execution/test".into(),projection:route.into(),
            max_response_bytes:parameters.get("max_response_bytes").map(|value| value.parse().unwrap()).unwrap_or(DEFAULT_RESPONSE_BYTES),
            request_digest:request_digest(uri.path(),&parameters).unwrap(),parameters,
            expires_at_unix_ms:client_now_ms() as u64 + 60_000,nonce:uuid::Uuid::new_v4().to_string()}
    }
    fn header(&self, payload: &ReadPayload, signer: &Ed25519KeyPair) -> String {
        let signature=base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signer.sign(signing_bytes(payload).unwrap().as_bytes()).as_ref());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"payload":payload,"signature":signature})).unwrap())
    }
    fn session(&self) -> ClientSession {
        super::super::authenticate(&self.state,&credential_digest(&self.credential),"fabric-loopback").unwrap().unwrap()
    }
    async fn request(&self, path: &str, header: Option<&str>) -> (StatusCode,Value,usize) {
        request(super::super::super::super::fabric_router(self.state.clone()),&self.credential,"GET",path,header).await
    }
}
async fn request(app: Router, credential: &str, method: &str, path: &str, header: Option<&str>) -> (StatusCode,Value,usize) {
    let mut request=Request::builder().method(method).uri(path)
        .header(AUTHORIZATION,format!("Bearer {credential}"));
    if let Some(header)=header {request=request.header(HEADER,header);}
    let response=app.oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
    let status=response.status();
    let bytes=to_bytes(response.into_body(),64*1024).await.unwrap();
    (status,serde_json::from_slice(&bytes).unwrap(),bytes.len())
}

#[tokio::test]
async fn assistant_approved_pages_decode_native_sync_through_typed_sdk() {
    let mut fixture = fixture(SCOPES, true).await;
    fixture.state.configured_peers = vec!["birch".into()];
    for diverged in [false, true] {
        let now = client_now_ms();
        let mut progress = smallclaims::store::PeerSyncProgress::default();
        progress.observe(0, Some((9_000, 0)), now);
        if diverged {
            progress.graph_differs_since_unix_ms = Some(now - smallclaims::store::REPLICATION_DIVERGED_AFTER_MS);
            progress.graph_compared_at_unix_ms = Some(now);
        }
        fixture.state.store.replication_sync.lock().unwrap().insert("birch".into(), progress);
        for route in ["attention", "missions", "agents"] {
            let payload = fixture.payload(route, "");
            let header = fixture.header(&payload, &fixture.attestor);
            let (status, value, _) = fixture.request(&format!("/v1/client/assistant/{route}"), Some(&header)).await;
            assert_eq!(status, StatusCode::OK, "{route}: {value}");
            assert_eq!(value["value"]["sync"]["state"], if diverged { "diverged" } else { "catching-up" });
            match route {
                "attention" => { let page: st3_client::Envelope<st3_client::AssistantAttentionPage> = serde_json::from_value(value).unwrap(); assert!(page.value.sync.is_some()); }
                "missions" => { let page: st3_client::Envelope<st3_client::AssistantMissionPage> = serde_json::from_value(value).unwrap(); assert!(page.value.sync.is_some()); }
                _ => { let page: st3_client::Envelope<st3_client::AssistantAgentPage> = serde_json::from_value(value).unwrap(); assert!(page.value.sync.is_some()); }
            }
        }
    }
}

#[tokio::test]
async fn assistant_all_six_reads_require_proof_even_on_native_router() {
    let fixture=fixture(SCOPES,true).await;
    for (route,_) in ROUTES {
        let path=format!("/v1/client/assistant/{route}");
        for app in [super::super::super::super::fabric_router(fixture.state.clone()),
                    super::super::super::super::router(fixture.state.clone())] {
            let (status,value,_)=request(app,&fixture.credential,"GET",&path,None).await;
            assert_eq!(status,StatusCode::FORBIDDEN,"{path} {value}");
            assert!(value.get("value").is_none());
        }
    }
}
#[tokio::test]
async fn assistant_read_proofs_reject_unpinned_fabricated_shim_expired_and_wrong_filter() {
    let unpinned=fixture(SCOPES,false).await;
    let payload=unpinned.payload("summary","");
    let header=unpinned.header(&payload,&unpinned.attestor);
    assert_eq!(unpinned.request("/v1/client/assistant/summary",Some(&header)).await.0,StatusCode::FORBIDDEN);
    let fixture=fixture(SCOPES,true).await;
    let payload=fixture.payload("summary","");
    for signer in [&signing_key(),&fixture.shim] {
        let header=fixture.header(&payload,signer);
        let (status,value,_)=fixture.request("/v1/client/assistant/summary",Some(&header)).await;
        assert_eq!(status,StatusCode::FORBIDDEN,"{value}");assert!(value.get("value").is_none());
    }
    let mut expired=payload.clone();expired.expires_at_unix_ms=1;
    let header=fixture.header(&expired,&fixture.attestor);
    assert_eq!(fixture.request("/v1/client/assistant/summary",Some(&header)).await.0,StatusCode::FORBIDDEN);
    let header=fixture.header(&payload,&fixture.attestor);
    assert_eq!(fixture.request("/v1/client/assistant/summary?limit=1",Some(&header)).await.0,StatusCode::FORBIDDEN);
    let mut wrong_grant=payload.clone();wrong_grant.grant_id="assistant-grant/000000000000000000000000".into();
    let header=fixture.header(&wrong_grant,&fixture.attestor);
    assert_eq!(fixture.request("/v1/client/assistant/summary",Some(&header)).await.0,StatusCode::FORBIDDEN);
    let mut stale=payload;stale.policy_revision-=1;
    let header=fixture.header(&stale,&fixture.attestor);
    assert_eq!(fixture.request("/v1/client/assistant/summary",Some(&header)).await.0,StatusCode::FORBIDDEN);
}
#[tokio::test]
async fn assistant_read_trust_rejects_shim_as_attestor_and_remote_pinning() {
    let fixture=fixture(SCOPES,true).await;
    let mut trust=fixture.trust_request();trust.attestor_public_key=trust.shim_public_key.clone();
    assert_eq!(pin(State(fixture.state.clone()),Extension(ClientSession::local(Some(OWNER)).unwrap()),
        AxumPath(fixture.grant.clone()),Json(trust)).await.unwrap_err().status,StatusCode::FORBIDDEN);
    assert_eq!(pin(State(fixture.state.clone()),Extension(fixture.session()),AxumPath(fixture.grant.clone()),
        Json(fixture.trust_request())).await.unwrap_err().status,StatusCode::FORBIDDEN);
}
#[tokio::test]
async fn assistant_read_nonce_is_atomic_and_private_results_never_replay_after_restart() {
    let fixture=fixture(SCOPES,true).await;
    let payload=fixture.payload("summary","");let header=fixture.header(&payload,&fixture.attestor);
    let (left,right)=tokio::join!(fixture.request("/v1/client/assistant/summary",Some(&header)),
        fixture.request("/v1/client/assistant/summary",Some(&header)));
    assert_eq!([left.0,right.0].iter().filter(|status| **status==StatusCode::OK).count(),1);
    assert_eq!([left.0,right.0].iter().filter(|status| **status==StatusCode::FORBIDDEN).count(),1);
    let reopened=super::super::super::tests::test_state_named(fixture.root.path(),"assistant-proof-tests");
    let (status,value,_)=request(super::super::super::super::fabric_router(reopened),&fixture.credential,
        "GET","/v1/client/assistant/summary",Some(&header)).await;
    assert_eq!(status,StatusCode::FORBIDDEN);assert!(value.get("value").is_none());
    let mut next_page=fixture.payload("agents","?limit=1");next_page.nonce=payload.nonce;
    let header=fixture.header(&next_page,&fixture.attestor);
    assert_eq!(fixture.request("/v1/client/assistant/agents?limit=1",Some(&header)).await.0,StatusCode::FORBIDDEN);
}
#[tokio::test]
async fn assistant_response_budget_includes_envelope_and_supports_downward_policy() {
    let mut fixture=fixture(SCOPES,true).await;
    let payload=fixture.payload("summary","");let header=fixture.header(&payload,&fixture.attestor);
    let (status,value,bytes)=fixture.request("/v1/client/assistant/summary",Some(&header)).await;
    assert_eq!(status,StatusCode::OK,"{value}");assert!(bytes<=DEFAULT_RESPONSE_BYTES);
    // Admit enough bytes for the actual projection, but not its enclosing snapshot envelope.
    let projection_bytes=serde_json::to_vec(&value["value"]).unwrap().len();
    assert!(projection_bytes<bytes);
    let envelope_budget=projection_bytes+(bytes-projection_bytes)/2;
    let query=format!("?max_response_bytes={envelope_budget}");
    let payload=fixture.payload("summary",&query);
    let receipt=verify(&fixture.state,&fixture.session(),"/v1/client/assistant/summary",
        &payload.parameters,&ReadAuthorization {payload:payload.clone(),signature:
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(fixture.attestor.sign(signing_bytes(&payload).unwrap().as_bytes()).as_ref())}).unwrap().1;
    let header=fixture.header(&payload,&fixture.attestor);
    let (status,value,_)=fixture.request(&format!("/v1/client/assistant/summary{query}"),Some(&header)).await;
    assert_eq!(status,StatusCode::PAYLOAD_TOO_LARGE,"{value}");assert!(value.get("value").is_none());
    let reopened=super::super::super::tests::test_state_named(fixture.root.path(),"assistant-proof-tests");
    let outcome=latest(&reopened,&receipt.subject,OUTCOME).unwrap().unwrap();
    let fields=fields(&outcome).unwrap();
    assert_eq!(fields["outcome"],"denied-budget");
    assert_eq!(fields["returned_bytes"],0);
    assert!(fields["budget_used"].as_u64().unwrap()>envelope_budget as u64);
    assert_eq!(fields["max_response_bytes"],envelope_budget);
    assert!(fields["response_digest"].is_null());
    assert_eq!(fixture.request(&format!("/v1/client/assistant/summary{query}"),Some(&header)).await.0,StatusCode::FORBIDDEN);
    let payload=fixture.payload("summary","?max_response_bytes=16385");let header=fixture.header(&payload,&fixture.attestor);
    assert!(!fixture.request("/v1/client/assistant/summary?max_response_bytes=16385",Some(&header)).await.0.is_success());
    let mut trust=fixture.trust_request();trust.default_response_bytes=1024;trust.max_response_bytes=1024;
    let Json(grant)=pin(State(fixture.state.clone()),Extension(ClientSession::local(Some(OWNER)).unwrap()),
        AxumPath(fixture.grant.clone()),Json(trust)).await.unwrap();fixture.revision=grant["policy_revision"].as_u64().unwrap();
    let mut payload=fixture.payload("summary","");payload.max_response_bytes=1024;
    let header=fixture.header(&payload,&fixture.attestor);
    let (status,value,bytes)=fixture.request("/v1/client/assistant/summary",Some(&header)).await;
    assert_eq!(status,StatusCode::OK,"{value}");assert!(bytes<=1024);
    let payload=fixture.payload("summary","?max_response_bytes=1025");let header=fixture.header(&payload,&fixture.attestor);
    assert!(!fixture.request("/v1/client/assistant/summary?max_response_bytes=1025",Some(&header)).await.0.is_success());
}
fn concrete_path(path:&str)->String {
    let mut output=String::new();let mut parameter=false;
    for character in path.chars() {match character {'{'=>{parameter=true;output.push_str("fixture");},
        '}'=>parameter=false,_ if !parameter=>output.push(character),_=>{}}}
    output
}
#[tokio::test]
async fn assistant_four_scopes_forbid_all_other_catalog_routes_and_mutations() {
    let fixture=fixture(&SCOPES[..4],true).await;
    let catalog:Value=serde_json::from_str(&std::fs::read_to_string(test_env!("CARGO_MANIFEST_DIR","/../../docs/st3/client-v0/schemas/operations.json")).unwrap()).unwrap();
    for section in ["reads","commands","streams"] {
        for operation in catalog[section].as_array().unwrap() {
            let path=concrete_path(operation["path"].as_str().unwrap());
            if ROUTES.iter().any(|(route,_)|path==format!("/v1/client/assistant/{route}")) {continue;}
            let method=operation["method"].as_str().unwrap();
            let (status,value,_)=request(super::super::super::super::fabric_router(fixture.state.clone()),
                &fixture.credential,method,&path,None).await;
            assert_eq!(status,StatusCode::FORBIDDEN,"{method} {path}: {value}");assert!(value.get("value").is_none());
        }
    }
    for path in ["/v1/status","/v1/events","/v1/claims","/v1/messages/by-key",
        "/v1/client/status/foreign-handle","/v1/client/operations/foreign-handle",
        "/v1/client/assistant/status/foreign-handle","/v1/client/assistant/cache/foreign-handle"] {
        let (status,value,_)=request(super::super::super::super::router(fixture.state.clone()),
            &fixture.credential,"GET",path,None).await;
        assert_eq!(status,StatusCode::FORBIDDEN,"{path}: {value}");assert!(value.get("value").is_none());
    }
}
#[tokio::test]
async fn assistant_revocation_and_policy_change_invalidate_signed_read_proofs() {
    let fixture=fixture(SCOPES,true).await;
    let payload=fixture.payload("summary","");let header=fixture.header(&payload,&fixture.attestor);
    let mut trust=fixture.trust_request();trust.key_id="approval-key/rotated".into();
    let _=pin(State(fixture.state.clone()),Extension(ClientSession::local(Some(OWNER)).unwrap()),
        AxumPath(fixture.grant.clone()),Json(trust)).await.unwrap();
    assert_eq!(fixture.request("/v1/client/assistant/summary",Some(&header)).await.0,StatusCode::FORBIDDEN);
    let _=super::super::revoke(State(fixture.state.clone()),Extension(ClientSession::local(Some(OWNER)).unwrap()),
        AxumPath(fixture.grant.clone())).await.unwrap();
    assert_eq!(fixture.request("/v1/client/assistant/summary",Some(&header)).await.0,StatusCode::FORBIDDEN);
}
#[test]
fn assistant_read_request_digest_is_canonical_and_rejects_duplicate_filters() {
    let left="/v1/client/assistant/agents?limit=1&cursor=a%2Fb".parse::<axum::http::Uri>().unwrap();
    let right="/v1/client/assistant/agents?cursor=a%2fb&limit=1".parse::<axum::http::Uri>().unwrap();
    assert_eq!(request_digest(left.path(),&parameters(&left).unwrap()).unwrap(),
        request_digest(right.path(),&parameters(&right).unwrap()).unwrap());
    let duplicate="/v1/client/assistant/agents?limit=1&limit=2".parse().unwrap();
    assert!(parameters(&duplicate).is_err());
}

#[tokio::test]
async fn assistant_final_delivery_denials_leave_durable_receipts() {
    for reason in ["expiry","policy","revocation"] {
        let fixture=fixture(SCOPES,true).await;
        let payload=fixture.payload("summary","");
        let mut headers=HeaderMap::new();
        headers.insert(HEADER,fixture.header(&payload,&fixture.attestor).parse().unwrap());
        let (Extension(mut receipt),_,Json(value))=read_inner(State(fixture.state.clone()),
            Extension(fixture.session()),Extension(new_client_snapshot(&fixture.state)),
            OriginalUri("/v1/client/assistant/summary".parse().unwrap()),headers).await.unwrap();
        match reason {
            "expiry"=>receipt.proof_expiry=1,
            "policy"=>{let _=pin(State(fixture.state.clone()),Extension(ClientSession::local(Some(OWNER)).unwrap()),
                AxumPath(fixture.grant.clone()),Json(fixture.trust_request())).await.unwrap();},
            "revocation"=>{let _=super::super::revoke(State(fixture.state.clone()),
                Extension(ClientSession::local(Some(OWNER)).unwrap()),AxumPath(fixture.grant.clone())).await.unwrap();},
            _=>unreachable!(),
        }
        let bytes=serde_json::to_vec(&value).unwrap();
        assert_eq!(finish(&fixture.state,&receipt,&bytes,true).await.unwrap_err().status,StatusCode::FORBIDDEN);
        let reopened=super::super::super::tests::test_state_named(fixture.root.path(),"assistant-proof-tests");
        let outcome=latest(&reopened,&receipt.subject,OUTCOME).unwrap().unwrap();
        let fields=fields(&outcome).unwrap();
        assert_eq!(fields["outcome"],"denied-authorization","{reason}");
        assert_eq!(fields["denial_code"],"forbidden");
        assert_eq!(fields["returned_bytes"],0);
        assert!(fields["response_digest"].is_null());
        let audits=reopened.store.claims_for_kind_at("custom.client.assistant.v1.audit",None,true,100).unwrap().claims;
        assert!(audits.iter().any(|claim| {
            let fields=&claim.body["fields"];
            fields["operation"]=="read" && fields["outcome"]=="denied"
                && fields["grant_id"]==fixture.grant && fields["actor"]==receipt.actor
        }),"{reason} must leave a final denial audit");
        let header=fixture.header(&payload,&fixture.attestor);
        let (status,value,_)=fixture.request("/v1/client/assistant/summary",Some(&header)).await;
        assert_eq!(status,StatusCode::FORBIDDEN);assert!(value.get("value").is_none());
    }
}

// Use the native Codex transcript/runtime-binding fixture, not a synthetic projection.
fn content_source(fixture: &mut Fixture, text: &str) -> (String,String,String) {
    let home=fixture.root.path().join("home");
    let transcript=home.join(".codex/sessions/2026/10/11/assistant.jsonl");
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let agent="agent/reads-codex";
    let incarnation="native-pty:reads";
    let provider_incarnation="provider-reads";
    let native_id="native-reads-codex-test";
    std::fs::write(&transcript,format!("{}\n{}\n",
        json!({"type":"session_meta","timestamp":"2026-10-11T12:00:00Z","payload":{
            "id":native_id,"cwd":fixture.root.path(),"source":"test"}}),
        json!({"type":"response_item","timestamp":"2026-10-11T12:00:01Z","payload":{
            "type":"message","role":"assistant","id":"answer","content":[{"type":"output_text","text":text}]}}),
    )).unwrap();
    fixture.state.native_session_home=Some(home);
    let directory=fixture.state.state_dir.join("drivers")
        .join(&hex::encode(Sha256::digest(agent.as_bytes()))[..24]).join("state");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("runtime.json"),serde_json::to_vec(&json!({
        "agent":"reads-codex","incarnation":provider_incarnation})).unwrap()).unwrap();
    std::fs::write(directory.join("binding.json"),serde_json::to_vec(&json!({
        "agent":"reads-codex","runtimeIncarnation":provider_incarnation,"threadId":native_id})).unwrap()).unwrap();
    for (kind,fields) in [
        ("runtime.observed",json!({"status":"running","runtime_id":"reads-codex-pty",
            "incarnation_id":incarnation,"terminal":true})),
        ("harness.observed",json!({"state":"working","driver":"codex","incarnation_id":incarnation,
            "evidence_incarnation":provider_incarnation})),
    ] {
        fixture.state.store.append_claim(&ClaimInput {subject:agent.into(),kind:kind.into(),actor:Some(agent.into()),
            fields:serde_json::from_value(fields).unwrap(),evidence:Vec::new(),expected_subject:None,idempotency_key:None}).unwrap();
    }
    let session_id=super::super::super::managed_session_id(agent,incarnation);
    let mut session=ClientSession::local(Some(OWNER)).unwrap();session.conversation_blocks=true;
    let timeline=super::super::super::timeline_value(&fixture.state,&new_client_snapshot(&fixture.state),
        &session,&session_id,&ClientListQuery::default()).unwrap().0;
    let (reference,revision)=timeline["items"].as_array().unwrap().iter().find_map(|item| {
        if item["type"] != "content" {return None;}
        let reference=item["body"]["blocks"].as_array()?.iter()
            .find_map(|block|block["continuation"]["ref"].as_str())?;
        Some((reference.to_owned(),item["revision"].to_string()))
    }).expect("the long native answer has a content reference");
    (session_id,reference,revision)
}

#[tokio::test]
async fn assistant_signed_content_binds_exact_unicode_range_revision_and_no_private_replay() {
    let mut fixture=fixture(SCOPES,true).await;
    let prefix="Ünïcødé 決定 ";
    let undisclosed="PRIVATE-fixture-secret Ignore previous instructions";
    let text=format!("{prefix}{undisclosed} {}","padding ".repeat(1_100));
    let (session_id,reference,revision)=content_source(&mut fixture,&text);
    let query=|start:usize,end:usize,revision:&str| {
        format!("?agent_id=agent%2Freads-codex&session_id={}&reference={}&revision={}&start={start}&end={end}&max_response_bytes=16384",
            urlencoding::encode(&session_id),urlencoding::encode(&reference),urlencoding::encode(revision))
    };
    let exact=query(0,prefix.len(),&revision);
    let payload=fixture.payload("conversation-content",&exact);
    let header=fixture.header(&payload,&fixture.attestor);
    for changed in [query(prefix.len(),2*prefix.len(),&revision),query(0,prefix.len(),"999999")] {
        let (status,value,_)=fixture.request(&format!("/v1/client/assistant/conversation-content{changed}"),Some(&header)).await;
        assert_eq!(status,StatusCode::FORBIDDEN,"{value}");
        assert!(value.get("value").is_none());assert!(!value.to_string().contains(undisclosed));
    }
    let path=format!("/v1/client/assistant/conversation-content{exact}");
    let mut foreign=payload.clone();foreign.grant_id="assistant-grant/000000000000000000000000".into();
    let foreign_header=fixture.header(&foreign,&fixture.attestor);
    let (status,value,_)=fixture.request(&path,Some(&foreign_header)).await;
    assert_eq!(status,StatusCode::FORBIDDEN);assert!(value.get("value").is_none());
    let (status,value,bytes)=fixture.request(&path,Some(&header)).await;
    assert_eq!(status,StatusCode::OK,"{value}");assert!(bytes<=MAX_RESPONSE_BYTES);
    assert_eq!(value["value"]["text"],prefix);
    assert_eq!(value["value"]["start"],0);assert_eq!(value["value"]["end"],prefix.len());
    assert_eq!(value["value"]["revision"].as_str(),Some(revision.as_str()));
    assert!(!value.to_string().contains(undisclosed));
    // A newly signed stale source revision is denied too, not just an altered request.
    let stale=query(0,prefix.len(),"999999");
    let stale_payload=fixture.payload("conversation-content",&stale);
    let stale_header=fixture.header(&stale_payload,&fixture.attestor);
    let (status,value,_)=fixture.request(&format!("/v1/client/assistant/conversation-content{stale}"),Some(&stale_header)).await;
    assert!(!status.is_success());assert!(value.get("value").is_none());
    let mut reopened=super::super::super::tests::test_state_named(fixture.root.path(),"assistant-proof-tests");
    reopened.native_session_home=fixture.state.native_session_home.clone();
    for probe in [path,format!("/v1/client/assistant/conversation-content{}",query(prefix.len(),2*prefix.len(),&revision)),
        "/v1/client/assistant/status/execution/test".into(),"/v1/client/assistant/cache/execution/test".into()] {
        let (status,value,_)=request(super::super::super::super::fabric_router(reopened.clone()),
            &fixture.credential,"GET",&probe,Some(&header)).await;
        assert_eq!(status,StatusCode::FORBIDDEN,"{probe}: {value}");
        assert!(value.get("value").is_none());assert!(!value.to_string().contains(prefix));
        assert!(!value.to_string().contains(undisclosed));
    }
}

const AUTH_PTY: &str = "assistant-auth-pty";
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assistant_signed_terminal_binds_live_screen_revision_and_refuses_replay() {
    use super::super::reads::tests::{fixture_screen,live_terminal};
    let mut fixture=fixture(SCOPES,true).await;
    let Some((state,runtime,incarnation,_cleanup))=live_terminal(fixture.state.clone(),AUTH_PTY).await else {
        return;
    };
    fixture.state=state;
    let mut screens=crate::api::terminal_view::subscribe(&fixture.state.pty_root,AUTH_PTY,&incarnation);
    let screen=fixture_screen(&mut screens,"ready").await;
    let revision=screen.revision().to_owned();
    let expected=screen.line_texts().collect::<Vec<_>>().join("\n");
    let query=|incarnation:&str,revision:&str| {
        format!("?agent_id=agent%2Freads-fixture.worker&terminal_id=terminal%2Fagent%2Freads-fixture.worker&incarnation_id={}&screen_revision={}",
            urlencoding::encode(incarnation),urlencoding::encode(revision))
    };
    let exact=query(&incarnation,&revision);
    let payload=fixture.payload("terminal-content",&exact);
    let header=fixture.header(&payload,&fixture.attestor);
    for changed in [query("foreign-incarnation",&revision),query(&incarnation,"foreign-revision")] {
        let (status,value,_)=fixture.request(&format!("/v1/client/assistant/terminal-content{changed}"),Some(&header)).await;
        assert_eq!(status,StatusCode::FORBIDDEN);assert!(value.get("value").is_none());
    }
    let path=format!("/v1/client/assistant/terminal-content{exact}");
    let (status,value,bytes)=fixture.request(&path,Some(&header)).await;
    assert_eq!(status,StatusCode::OK,"{value}");assert!(bytes<=DEFAULT_RESPONSE_BYTES);
    assert_eq!(value["value"]["text"],expected);
    assert_eq!(value["value"]["screen_revision"],revision);
    assert_eq!(value["value"]["incarnation_id"],incarnation);
    assert!(!value.to_string().contains("SECRET-sk-live-0123456789"));
    // An unconsumed, valid proof for the old screen must fail once the actual screen changes.
    let stale_payload=fixture.payload("terminal-content",&exact);
    let stale_header=fixture.header(&stale_payload,&fixture.attestor);
    let sender=runtime.clone();
    crate::api::read_deadline::spawn_blocking(move ||sender.send_line(AUTH_PTY,"next")).await.unwrap().unwrap();
    let changed=fixture_screen(&mut screens,"changed:next").await;
    assert_ne!(changed.revision(),revision);
    let (status,value,_)=fixture.request(&path,Some(&stale_header)).await;
    assert_eq!(status,StatusCode::CONFLICT);assert!(value.get("value").is_none());
    let current=query(&incarnation,changed.revision());
    let current_payload=fixture.payload("terminal-content",&current);
    let current_header=fixture.header(&current_payload,&fixture.attestor);
    let current_path=format!("/v1/client/assistant/terminal-content{current}");
    let (status,value,bytes)=fixture.request(&current_path,Some(&current_header)).await;
    assert_eq!(status,StatusCode::OK,"{value}");assert!(bytes<=DEFAULT_RESPONSE_BYTES);
    assert_eq!(value["value"]["screen_revision"],changed.revision());
    assert!(value["value"]["text"].as_str().unwrap().contains("changed:next"));
    let reopened=super::super::super::tests::test_state_named(fixture.root.path(),"assistant-proof-tests");
    for probe in [current_path,"/v1/client/assistant/cache/execution/test".into(),
        "/v1/client/assistant/status/execution/test".into()] {
        let (status,value,_)=request(super::super::super::super::fabric_router(reopened.clone()),
            &fixture.credential,"GET",&probe,Some(&current_header)).await;
        assert_eq!(status,StatusCode::FORBIDDEN);assert!(value.get("value").is_none());
        assert!(!value.to_string().contains("changed:next"));
        assert!(!value.to_string().contains("SECRET-sk-live-0123456789"));
    }
}

async fn projection_read_mode(fixture: &mut Fixture, mode: &str) {
    let policy=serde_json::from_value(json!({"api_version":CLIENT_API_VERSION,"scopes":SCOPES,
        "expires_in_seconds":3600,"disclosure_consent":true,"consent_evidence":"projection-isolation",
        "minimal_read_mode":mode})).unwrap();
    let Json(grant)=super::super::update_policy(State(fixture.state.clone()),
        Extension(ClientSession::local(Some(OWNER)).unwrap()),AxumPath(fixture.grant.clone()),Json(policy)).await.unwrap();
    fixture.revision=grant["policy_revision"].as_u64().unwrap();
}

async fn projection_cursor_isolation(change_mode: bool) {
    let mut fixture=fixture(SCOPES,true).await;
    let source=r#"version 2
agent "projection-one" { workspace "/tmp"; command "true"; restart always; }
agent "projection-two" { workspace "/tmp"; command "true"; restart always; }
mission "projection-one" state="ready" { goal "PRIVATE mission one"; }
mission "projection-two" state="ready" { goal "PRIVATE mission two"; }
"#;
    let intent=crate::graph::parse_internal_intent(source,"projection-isolation").unwrap();
    fixture.state.store.apply_internal(&intent,"projection-isolation").unwrap();
    for id in ["one","two"] {
        fixture.state.store.ask_person(&serde_json::from_value(json!({
            "person":OWNER,"actor":format!("agent/projection-isolation.projection-{id}"),
            "title":format!("PRIVATE attention {id}"),"reason":"PRIVATE attention detail",
            "new_run":format!("projection-{id}"),"idempotency_key":format!("projection-{id}")
        })).unwrap()).unwrap();
    }
    projection_read_mode(&mut fixture,"caller-bound-allow").await;
    let mut cursors=Vec::new();
    for projection in ["missions","attention","agents"] {
        let (status,value,_)=fixture.request(
            &format!("/v1/client/assistant/{projection}-rich?limit=1&max_response_bytes=16384"),None).await;
        assert_eq!(status,StatusCode::OK,"{projection}: {value}");
        assert!(value["value"]["items"][0]["title"].is_string(),"{value}");
        if projection=="attention" {
            assert!(value["value"]["items"][0]["title"].as_str().unwrap().contains("PRIVATE attention"));
        }
        let cursor=value["value"]["page"]["next_cursor"].as_str().expect("two source rows").to_owned();
        // Same-class continuation still serves its original rich snapshot.
        let (status,continued,_)=fixture.request(&format!(
            "/v1/client/assistant/{projection}-rich?cursor={}&max_response_bytes=16384",
            urlencoding::encode(&cursor)),None).await;
        assert_eq!(status,StatusCode::OK,"{continued}");
        assert!(continued["value"]["items"][0]["title"].is_string(),"{continued}");
        cursors.push((projection,cursor));
    }
    if change_mode {
        projection_read_mode(&mut fixture,"approval-once").await;
    } else {
        // A second independently paired grant shares the daemon/cache, but not approval.
        let request=serde_json::from_value(json!({"api_version":CLIENT_API_VERSION,
            "device_name":"metadata-only","assistant_id":PRINCIPAL,"scopes":SCOPES,
            "disclosure_consent":true,"consent_evidence":"projection-isolation"})).unwrap();
        let Json(challenge)=begin(fixture.state.clone(),ClientSession::local(Some(OWNER)).unwrap(),request).await.unwrap();
        let device_key=signing_key();
        let request=serde_json::from_value(json!({"api_version":CLIENT_API_VERSION,"code":challenge["code"],
            "device_public_key":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(device_key.public_key().as_ref())})).unwrap();
        let id=challenge["pairing_id"].as_str().unwrap().strip_prefix("pairing/").unwrap();
        let Json(paired)=complete(&fixture.state,id,&request,"fabric-loopback").await.unwrap().unwrap();
        fixture.credential=paired["credential"].as_str().unwrap().into();
        fixture.grant=paired["grant"]["id"].as_str().unwrap().into();
        let Json(grant)=pin(State(fixture.state.clone()),Extension(ClientSession::local(Some(OWNER)).unwrap()),
            AxumPath(fixture.grant.clone()),Json(fixture.trust_request())).await.unwrap();
        fixture.revision=grant["policy_revision"].as_u64().unwrap();
    }
    for (projection,cursor) in cursors {
        // Also rewrite the untrusted cursor's collection: cache isolation must deny this,
        // independently of the outer collection mismatch check.
        let mut forged=decode_client_cursor(&cursor).unwrap();
        forged.collection=format!("assistant.{projection}");
        for cursor in [cursor,encode_client_cursor(&forged).unwrap()] {
            let query=format!("?cursor={}&max_response_bytes=16384",urlencoding::encode(&cursor));
            let payload=fixture.payload(projection,&query);
            let header=fixture.header(&payload,&fixture.attestor);
            let (status,value,_)=fixture.request(&format!("/v1/client/assistant/{projection}{query}"),Some(&header)).await;
            assert_eq!(status,StatusCode::GONE,"{projection}: {value}");
            assert!(value.get("value").is_none(),"{value}");
            assert!(!value.to_string().contains("PRIVATE"),"{value}");
        }
        // A fresh metadata approval still works, and its own cursor remains usable.
        let query="?limit=1&max_response_bytes=16384";
        let payload=fixture.payload(projection,query);
        let header=fixture.header(&payload,&fixture.attestor);
        let (status,value,_)=fixture.request(&format!("/v1/client/assistant/{projection}{query}"),Some(&header)).await;
        assert_eq!(status,StatusCode::OK,"{value}");
        assert!(value["value"]["items"][0].get("title").is_none(),"{value}");
        assert!(!value.to_string().contains("PRIVATE"),"{value}");
        let cursor=value["value"]["page"]["next_cursor"].as_str().unwrap();
        let query=format!("?cursor={}&max_response_bytes=16384",urlencoding::encode(cursor));
        let payload=fixture.payload(projection,&query);
        let header=fixture.header(&payload,&fixture.attestor);
        let (status,value,_)=fixture.request(&format!("/v1/client/assistant/{projection}{query}"),Some(&header)).await;
        assert_eq!(status,StatusCode::OK,"{value}");
        assert!(value["value"]["items"][0].get("title").is_none(),"{value}");
        assert!(!value.to_string().contains("PRIVATE"),"{value}");
    }
}

#[tokio::test]
async fn assistant_rich_cursors_cannot_disclose_content_through_another_grants_metadata_proof() {
    projection_cursor_isolation(false).await;
}

#[tokio::test]
async fn assistant_rich_cursors_cannot_disclose_content_after_read_mode_change() {
    projection_cursor_isolation(true).await;
}
