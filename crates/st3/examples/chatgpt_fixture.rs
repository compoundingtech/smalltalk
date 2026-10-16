//! Scratch-only daemon API harness. Never starts a managed agent or reads ambient state.
use std::{path::PathBuf,sync::Arc};
use serde_json::json;
use sha2::{Digest,Sha256};
use st3::{api::AppState,store::Store,model::{ClaimInput,PersonAskRequest}};
// Exercise the production DING renderer against the daemon's stored recipient message.
async fn delivery(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(subject): axum::extract::Path<String>,
) -> axum::Json<serde_json::Value> {
    let message = state.store.message(&subject).unwrap().unwrap();
    let rendered = st_drivers::ding::st3_notification_with_metadata(
        &message.subject, &message.from, &message.to, message.title.as_deref(),
        &message.content, &st_drivers::ding::st3_body_sha256(&message.content),
        st_drivers::ding::NotificationMetadata {
            attachments: &[],
            assistant: message.assistant_metadata().as_ref(),
        },
    );
    axum::Json(json!({"value":{"message":message,"rendered":rendered}}))
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root=PathBuf::from(std::env::args().nth(1).expect("explicit scratch state directory"));
    std::fs::create_dir_all(&root)?;
    st3::store::assistant_write_gate::install(&root.join("mutation-gate"), None)?;
    let store=Arc::new(Store::open(&root.join("store.sqlite"),"prototype")?);
    store.bind_fleet("3c9a1f2e-8b7d-4e6c-a5f4-1d2e3c4b5a69")?;
    store.set_node_key(Arc::new(smallclaims::fleet::MemberKey::generate()?.0))?;
    let source=format!("version 2\nagent \"prototype.worker\" {{ workspace {:?}; command \"true\"; restart always; }}\nmission \"prototype-mission\" state=\"ready\" {{ goal \"Prototype mission\"; }}",root);
    let intent=st3::graph::parse_intent(&source,"prototype")?;
    store.apply_internal(&intent,"prototype-seed")?;
    let ask=store.ask_person(&PersonAskRequest {legacy_request:None,person:"person/ada".into(),title:"Choose a prototype answer".into(),reason:"Exercise approved typed answer".into(),actor:"agent/prototype.worker".into(),step:None,new_run:Some("prototype-ask".into()),incarnation:None,
        request:Some(json!({"version":1,"type":"choice","question":"Which option?","why_person":"The owner chooses.","answers":[{"id":"yes","label":"Yes","consequence":"Record yes."},{"id":"no","label":"No","consequence":"Record no."}]})),idempotency_key:"prototype-ask-seed".into()})?;
    let expiry_ask=store.ask_person(&PersonAskRequest {legacy_request:None,person:"person/ada".into(),title:"Choose an expiry-test answer".into(),reason:"Exercise expiry after owner approval".into(),actor:"agent/prototype.worker".into(),step:None,new_run:Some("expiry-ask".into()),incarnation:None,
        request:Some(json!({"version":1,"type":"choice","question":"Which option?","why_person":"The owner chooses.","answers":[{"id":"yes","label":"Yes","consequence":"Record yes."},{"id":"no","label":"No","consequence":"Record no."}]})),idempotency_key:"expiry-ask-seed".into()})?;
    let home=root.join("home");
    let transcript=home.join(".codex/sessions/2026/10/14/prototype.jsonl");
    std::fs::create_dir_all(transcript.parent().unwrap())?;
    let text=format!("Prototype conversation. {}","padding ".repeat(1100));
    std::fs::write(transcript,format!("{}\n{}\n",json!({"type":"session_meta","timestamp":"2026-10-14T12:00:00Z","payload":{"id":"prototype-native","cwd":root,"source":"test"}}),json!({"type":"response_item","timestamp":"2026-10-14T12:00:01Z","payload":{"type":"message","role":"assistant","id":"prototype-answer","content":[{"type":"output_text","text":text}]}})))?;
    let directory=root.join("drivers").join(&hex::encode(Sha256::digest(b"agent/prototype.worker"))[..24]).join("state");
    std::fs::create_dir_all(&directory)?;
    std::fs::write(directory.join("runtime.json"),json!({"agent":"prototype.worker","incarnation":"prototype-provider"}).to_string())?;
    std::fs::write(directory.join("binding.json"),json!({"agent":"prototype.worker","runtimeIncarnation":"prototype-provider","threadId":"prototype-native"}).to_string())?;
    for (kind,fields) in [("runtime.observed",json!({"status":"running","runtime_id":"prototype-pty","incarnation_id":"native-pty:prototype","terminal":true})),("harness.observed",json!({"state":"working","driver":"codex","incarnation_id":"native-pty:prototype","evidence_incarnation":"prototype-provider"}))] {
        store.append_claim(&ClaimInput {subject:"agent/prototype.worker".into(),kind:kind.into(),actor:Some("agent/prototype.worker".into()),fields:serde_json::from_value(fields)?,evidence:vec![],expected_subject:None,idempotency_key:None})?;
    }
    let state=AppState {owning_person:Some("person/ada".into()),store,notify:Arc::new(tokio::sync::Notify::new()),event_notify:tokio::sync::watch::channel(0).0,node:"prototype".into(),state_dir:root.clone(),pty_root:root.join("pty"),pty_binary:PathBuf::from("pty"),fleet_id:None,configured_peers:vec![],client_relay:None,native_session_home:Some(home),planner_default:Default::default()};
    st3::api::start_agent_roster(&state);
    st3::api::start_native_session_discovery(&state);
    let local=root.join("st3.sock");let gateway=root.join("st3-client.sock");
    let app=st3::api::router(state.clone()).merge(
        axum::Router::new().route("/prototype/delivery/{*subject}", axum::routing::get(delivery)).with_state(state.clone())
    );let fabric=st3::api::fabric_router(state);
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    std::fs::write(root.join("fixture.json"),json!({"pairing_url":format!("http://{}",listener.local_addr()?),"ask_id":ask.subject,"expiry_ask_id":expiry_ask.subject,"session_id":format!("session/{}",&hex::encode(Sha256::digest(b"agent/prototype.worker:native-pty:prototype"))[..24])}).to_string())?;
    tokio::try_join!(st3::api::serve_unix(&local,app),st3::api::serve_unix(&gateway,fabric.clone()),async {axum::serve(listener,fabric).await.map_err(anyhow::Error::from)})?;
    Ok(())
}
