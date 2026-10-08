//! Paired-device diagnostics: no user content, arbitrary claims or raw crash dumps.
use super::{ApiError, AppState, ClaimInput, client_now_ms, client_v0};
use axum::{Extension, Json, extract::State};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;

pub(super) const MAX_REQUEST_BYTES: usize = 128 * 1024;
const MAX_EVENTS: usize = 32;
const MAX_FRAMES: usize = 32;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

pub(super) async fn ingest(
    State(state): State<AppState>,
    Extension(session): Extension<client_v0::ClientSession>,
    Json(batch): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    client_v0::require_scope(&session, "write.client-diagnostics")?;
    let grant = session.pairing_grant.as_deref().ok_or_else(|| {
        client_v0::validation("diagnostics require a paired device, including on the Unix transport")
    })?;
    let now = u64::try_from(client_now_ms()).map_err(ApiError::internal)?;
    let inputs = sanitize_batch(&batch, &session.actor, &session.authority_actor, grant, now)?;
    let acknowledged = inputs.iter().map(|input| input.fields["event_id"].clone()).collect::<Vec<_>>();
    let store = state.store.clone();
    let device = session.actor;
    tokio::task::spawn_blocking(move || store.ingest_client_diagnostics(&device, &inputs, now))
        .await.map_err(ApiError::internal)?.map_err(ApiError::bad)?;
    Ok(Json(json!({"acknowledged_event_ids": acknowledged})))
}

fn invalid() -> ApiError {
    // Never echo untrusted strings (which could themselves contain credentials).
    client_v0::validation("invalid or oversized client diagnostic batch")
}

fn enumeration<'a>(value: &'a Value, options: &[&str]) -> Result<&'a str, ApiError> {
    value.as_str().filter(|text| options.contains(text)).ok_or_else(invalid)
}

fn integer(value: &Value, max: u64) -> Result<u64, ApiError> {
    value.as_u64().filter(|number| *number <= max).ok_or_else(invalid)
}

fn boolean(value: &Value) -> Result<bool, ApiError> {
    value.as_bool().ok_or_else(invalid)
}

fn uuid(value: &Value) -> Result<String, ApiError> {
    let text = value.as_str().filter(|text| text.len() == 36).ok_or_else(invalid)?;
    uuid::Uuid::parse_str(text).map(|id| id.hyphenated().to_string()).map_err(|_| invalid())
}

fn metadata_token(text: &str) -> bool {
    text.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && text.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn metadata(value: &Value, max: usize) -> Result<String, ApiError> {
    let text = value.as_str().filter(|text| !text.is_empty() && text.len() <= max).ok_or_else(invalid)?;
    Ok(if metadata_token(text) { text } else { "unknown" }.to_owned())
}

fn runtime_metadata(value: &Value) -> Result<String, ApiError> {
    let text = value.as_str().filter(|text| !text.is_empty() && text.len() <= 128).ok_or_else(invalid)?;
    // Match the iOS safeRuntime grammar, including Expo nativeVersion's build suffix.
    let safe = match text.split_once('(') {
        Some((version, build)) => metadata_token(version)
            && build.strip_suffix(')').is_some_and(metadata_token),
        None => metadata_token(text),
    };
    Ok(if safe { text } else { "unknown" }.to_owned())
}

fn frames(payload: &Value, native: bool) -> Result<Value, ApiError> {
    let frames = payload["frames"].as_array().filter(|frames| frames.len() <= MAX_FRAMES).ok_or_else(invalid)?;
    let sanitized = frames.iter().map(|frame| {
        if native {
            let binary = enumeration(&frame["binary"], &["app", "system", "unknown"])?;
            let offset = frame["offset"].as_str().filter(|offset| {
                !offset.is_empty() && offset.len() <= 32
                    && offset.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            }).ok_or_else(invalid)?;
            Ok(json!({"binary":binary,"offset":offset}))
        } else {
            Ok(json!({
                "module":enumeration(&frame["module"], &["app", "react-native", "unknown"])?,
                "line":integer(&frame["line"], u64::from(u32::MAX))?,
                "column":integer(&frame["column"], u64::from(u32::MAX))?,
            }))
        }
    }).collect::<Result<Vec<_>, ApiError>>()?;
    Ok(Value::Array(sanitized))
}

fn nullable_code(value: &Value) -> Result<Value, ApiError> {
    if value.is_null() { Ok(Value::Null) } else { integer(value, u64::from(u32::MAX)).map(Value::from) }
}

fn sanitize_payload(payload: &Value) -> Result<(String, Value), ApiError> {
    let kind = enumeration(&payload["kind"], &["launch", "js-error", "native-crash", "hang"])?;
    let sanitized = match kind {
        "launch" => {
            let breadcrumb = enumeration(&payload["breadcrumb"], &[
                "native-start", "js-start", "root-mounted", "foreground", "background", "previous-launch-unclean",
            ])?;
            let inferred = boolean(&payload["inferred"])?;
            if inferred != (breadcrumb == "previous-launch-unclean") { return Err(invalid()); }
            json!({"kind":kind,"breadcrumb":breadcrumb,"inferred":inferred})
        }
        "js-error" => {
            // Error names, messages and stack symbols can all be application/user content.
            let name = payload["name"].as_str().filter(|name| name.len() <= 64).ok_or_else(invalid)?;
            let name = if ["Error", "TypeError", "RangeError", "ReferenceError", "SyntaxError", "UnknownError"].contains(&name) {
                name
            } else { "UnknownError" };
            json!({"kind":kind,"name":name,"fatal":boolean(&payload["fatal"])?,"frames":frames(payload,false)?})
        }
        "native-crash" => json!({
            "kind":kind,"exception_type":nullable_code(&payload["exception_type"])?,
            "signal":nullable_code(&payload["signal"])?,"frames":frames(payload,true)?,
        }),
        "hang" => json!({"kind":kind,"duration_ms":integer(&payload["duration_ms"],MAX_SAFE_INTEGER)?,"frames":frames(payload,true)?}),
        _ => unreachable!(),
    };
    Ok((format!("client.{kind}"), sanitized))
}

/// Rebuild every object from an allowlist. Unknown fields are deliberately discarded,
/// rather than trusting a caller's claim that its own redaction was sufficient.
fn sanitize_batch(batch: &Value, device: &str, person: &str, grant: &str, now: u64) -> Result<Vec<ClaimInput>, ApiError> {
    if batch["version"] != 1 { return Err(invalid()); }
    let events = batch["events"].as_array().filter(|events| !events.is_empty() && events.len() <= MAX_EVENTS).ok_or_else(invalid)?;
    let subject = format!("client/{}", hex::encode(Sha256::digest(device.as_bytes())));
    events.iter().map(|event| {
        let occurred = integer(&event["occurred_at_unix_ms"],MAX_SAFE_INTEGER)?;
        let captured = integer(&event["captured_at_unix_ms"],MAX_SAFE_INTEGER)?;
        if occurred > captured.saturating_add(300_000) || captured > now.saturating_add(300_000) { return Err(invalid()); }
        let update = if event["update_id"] == "embedded" { "embedded".to_owned() } else { uuid(&event["update_id"])? };
        let (kind, payload) = sanitize_payload(&event["payload"])?;
        let mut fields = BTreeMap::from([
            ("event_id".into(),json!(uuid(&event["event_id"])?)),
            ("launch_id".into(),json!(uuid(&event["launch_id"])?)),
            ("sequence".into(),json!(integer(&event["sequence"],u64::from(u32::MAX))?)),
            ("occurred_at_unix_ms".into(),json!(occurred)),
            ("captured_at_unix_ms".into(),json!(captured)),
            ("app_version".into(),json!(metadata(&event["app_version"],64)?)),
            ("native_build".into(),json!(metadata(&event["native_build"],64)?)),
            ("runtime_version".into(),json!(runtime_metadata(&event["runtime_version"])?)),
            ("update_id".into(),json!(update)),
            ("os_version".into(),json!(metadata(&event["os_version"],32)?)),
            ("payload".into(),payload),
            ("paired_device".into(),json!(device)),
            ("pairing_grant".into(),json!(grant)),
            ("person_id".into(),json!(person)),
        ]);
        for (key, options) in [
            ("platform", &["ios"][..]),
            ("severity", &["info", "warning", "error", "fatal"][..]),
            ("capture_source", &["js", "native-marker", "metrickit"][..]),
            ("occurrence_time_basis", &["exact", "metric-interval-end"][..]),
            ("launch_id_basis", &["process", "metric-interval"][..]),
        ] {
            fields.insert(key.into(),json!(enumeration(&event[key], options)?));
        }
        Ok(ClaimInput { subject:subject.clone(),kind,actor:Some(device.into()),fields,evidence:Vec::new(),expected_subject:None,idempotency_key:None })
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::{Body,to_bytes}, http::{Request,StatusCode}};
    use std::sync::Arc;
    use tower::ServiceExt as _;

    fn state(root: &std::path::Path) -> AppState {
        AppState {
            store:Arc::new(crate::store::Store::open(&root.join("graph.db"),"diagnostic-test").unwrap()),
            notify:Arc::new(tokio::sync::Notify::new()),event_notify:tokio::sync::watch::channel(0).0,
            node:"diagnostic-test".into(),state_dir:root.into(),pty_root:root.join("pty"),pty_binary:root.join("unused-pty"),
            fleet_id:None,configured_peers:Vec::new(),client_relay:None,native_session_home:None,planner_default:Default::default(),
        }
    }
    fn pair(state: &AppState, credential: &str, actor: &str, scopes: Value, expires: u64) {
        state.store.append_claim(&ClaimInput {
            subject:format!("custom/client/{credential}"),kind:"custom.client.pairing-completed".into(),actor:Some("person/ada".into()),
            fields:BTreeMap::from([
                ("credential_hash".into(),json!(hex::encode(Sha256::digest(credential.as_bytes())))),
                ("session_actor".into(),json!(actor)),("person_id".into(),json!("person/ada")),
                ("scopes".into(),scopes),("expires_at_unix_ms".into(),json!(expires)),
            ]),evidence:Vec::new(),expected_subject:None,idempotency_key:None,
        }).unwrap();
    }
    fn event(id: u128) -> Value {
        let now = client_now_ms() as u64;
        json!({"event_id":uuid::Uuid::from_u128(id).to_string(),"launch_id":uuid::Uuid::from_u128(42).to_string(),"sequence":1,
            "occurred_at_unix_ms":now-1000,"captured_at_unix_ms":now,"occurrence_time_basis":"exact","launch_id_basis":"process",
            "app_version":"1.0","native_build":"1","runtime_version":"0.1.0(1)","update_id":"embedded","platform":"ios","os_version":"27.0",
            "severity":"error","capture_source":"js","payload":{"kind":"js-error","name":"TypeError","fatal":false,"frames":[]}})
    }
    async fn post(app: Router, credential: Option<&str>, body: Value) -> (StatusCode,Value) {
        let mut request = Request::builder().method("POST").uri("/v1/client/diagnostics").header("Content-Type","application/json");
        if let Some(credential) = credential { request = request.header("Authorization",format!("Bearer {credential}")); }
        let response = app.oneshot(request.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(),super::MAX_REQUEST_BYTES * 2).await.unwrap();
        (status,serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
    #[test]
    fn diagnostics_runtime_metadata_matches_expo_native_version() {
        for (runtime, expected) in [
            ("0.1.0(1)", "0.1.0(1)"),
            ("ios-42", "ios-42"),
            ("0.1.0(1)/private", "unknown"),
            ("0.1.0(secret message)", "unknown"),
            ("0.1.0(1)\n", "unknown"),
            ("0.1.0((1))", "unknown"),
            ("0.1.0()", "unknown"),
        ] {
            let mut report = event(1);
            report["runtime_version"] = json!(runtime);
            let inputs = sanitize_batch(&json!({"version":1,"events":[report]}),
                "client/one","person/ada","custom/client/one",client_now_ms() as u64).unwrap();
            assert_eq!(inputs[0].fields["runtime_version"], json!(expected));
        }
        let max = "x".repeat(128);
        assert_eq!(runtime_metadata(&json!(max)).unwrap(), max);
        assert!(runtime_metadata(&json!(format!("{}(1)", "x".repeat(126)))).is_err());
        // The broader runtime grammar must not loosen app/build/OS metadata.
        assert_eq!(metadata(&json!("0.1.0(1)"), 64).unwrap(), "unknown");
    }
    #[tokio::test]
    async fn diagnostics_auth_scope_expiry_and_revocation() {
        let root = tempfile::tempdir().unwrap(); let state = state(root.path());
        let now = client_now_ms() as u64;
        pair(&state,"reader","client/reader",json!(["read.projections"]),now+60_000);
        pair(&state,"writer","client/writer",json!(["write.client-diagnostics"]),now+60_000);
        pair(&state,"expired","client/expired",json!(["write.client-diagnostics"]),now-1);
        let app = super::super::fabric_router(state.clone()); let body = json!({"version":1,"events":[event(1)]});
        for credential in [None,Some("unknown"),Some("reader"),Some("expired")] {
            assert_eq!(post(app.clone(),credential,body.clone()).await.0,StatusCode::FORBIDDEN);
        }
        assert_eq!(post(app.clone(),Some("writer"),body.clone()).await.0,StatusCode::OK);
        state.store.append_claim(&ClaimInput {subject:"custom/client/writer".into(),kind:"custom.client.pairing-revoked".into(),actor:None,fields:BTreeMap::new(),evidence:Vec::new(),expected_subject:None,idempotency_key:None}).unwrap();
        assert_eq!(post(app,Some("writer"),body).await.0,StatusCode::FORBIDDEN);
    }
    #[tokio::test]
    async fn diagnostics_redaction_attribution_durable_ack_and_device_dedup() {
        let root = tempfile::tempdir().unwrap(); let state = state(root.path()); let now = client_now_ms() as u64;
        for credential in ["one","two"] {pair(&state,credential,&format!("client/{credential}"),json!(["write.client-diagnostics"]),now+60_000);}
        let app = super::super::fabric_router(state.clone()); let mut report = event(1);
        report["device_id"] = json!("client/forged");report["person_id"] = json!("person/alex");
        report["headers"] = json!({"Authorization":"Bearer private"});report["native_build"] = json!("/work/private");
        report["payload"]["message"] = json!("private user message");report["payload"]["stack"] = json!("https://example.invalid/?token=private");
        report["payload"]["frames"] = json!([{"module":"app","line":12,"column":2,"file":"/work/private/index.js","function":"private message"}]);
        let body = json!({"version":1,"events":[report]});
        let (status,ack) = post(app.clone(),Some("one"),body.clone()).await;
        assert_eq!(status,StatusCode::OK,"{ack}");assert_eq!(ack["value"]["acknowledged_event_ids"],json!([uuid::Uuid::from_u128(1).to_string()]));
        let records = state.store.local_observations_after(0,10).unwrap(); assert_eq!(records.len(),1);
        let fields = &records[0].body["fields"]; assert_eq!(fields["paired_device"],"client/one");assert_eq!(fields["person_id"],"person/ada");assert_eq!(fields["native_build"],"unknown");
        assert_eq!(fields["runtime_version"], "0.1.0(1)");
        let serialized = records[0].body.to_string(); for secret in ["private","forged","person/alex","Authorization","https://","/work/"] {assert!(!serialized.contains(secret),"{serialized}");}
        assert_eq!(post(app.clone(),Some("one"),body.clone()).await.0,StatusCode::OK);
        // IDs deduplicate across discriminants, not just within one observation kind.
        let mut changed = body.clone();changed["events"][0]["payload"] = json!({"kind":"launch","breadcrumb":"js-start","inferred":false});
        assert_eq!(post(app.clone(),Some("one"),changed).await.0,StatusCode::OK);
        assert_eq!(state.store.local_observations_after(0,10).unwrap().len(),1);
        assert_eq!(post(app,Some("two"),body).await.0,StatusCode::OK);
        assert_eq!(state.store.local_observations_after(0,10).unwrap().len(),2);
        drop(state);
        let reopened = crate::store::Store::open(&root.path().join("graph.db"),"diagnostic-test").unwrap();
        assert_eq!(reopened.local_observations_after(0,10).unwrap().len(),2);
    }
    #[tokio::test]
    async fn diagnostics_bounds_and_atomic_rate_admission() {
        let root = tempfile::tempdir().unwrap();let state = state(root.path());let now = client_now_ms() as u64;
        pair(&state,"writer","client/writer",json!(["write.client-diagnostics"]),now+60_000);
        let app = super::super::fabric_router(state.clone());
        for body in [json!({"version":2,"events":[event(1)]}),json!({"version":1,"events":[]}),json!({"version":1,"events":vec![event(1);33]})] {
            assert_eq!(post(app.clone(),Some("writer"),body).await.0,StatusCode::UNPROCESSABLE_ENTITY);
        }
        let mut too_many_frames = event(1);too_many_frames["payload"]["frames"] = json!(vec![json!({"module":"app","line":1,"column":1});33]);
        assert_eq!(post(app.clone(),Some("writer"),json!({"version":1,"events":[too_many_frames]})).await.0,StatusCode::UNPROCESSABLE_ENTITY);
        let huge = json!({"version":1,"events":[event(1)],"content":"x".repeat(MAX_REQUEST_BYTES)});
        assert_eq!(post(app.clone(),Some("writer"),huge).await.0,StatusCode::PAYLOAD_TOO_LARGE);
        for batch in 0..8 { let events = (1..=32).map(|id|event(batch*32+id)).collect::<Vec<_>>(); assert_eq!(post(app.clone(),Some("writer"),json!({"version":1,"events":events})).await.0,StatusCode::OK); }
        let (status,_) = post(app.clone(),Some("writer"),json!({"version":1,"events":[event(257),event(258)]})).await;
        assert_eq!(status,StatusCode::TOO_MANY_REQUESTS);assert_eq!(state.store.local_observations_after(0,300).unwrap().len(),256);
        assert_eq!(post(app,Some("writer"),json!({"version":1,"events":[event(1)]})).await.0,StatusCode::OK);
    }
    #[test]
    fn diagnostics_payload_bounds_and_otlp_occurrence_severity() {
        let store = crate::store::Store::open_memory("diagnostic-test").unwrap();
        let now = client_now_ms() as u64;
        for (id, severity, number, payload) in [
            (1, "info", 9, json!({"kind":"launch","breadcrumb":"js-start","inferred":false})),
            (2, "warning", 13, json!({"kind":"hang","duration_ms":2500,"frames":[{"binary":"system","offset":"abc","raw_dump":"private"}]})),
            (3, "error", 17, json!({"kind":"js-error","name":"private secret","fatal":false,"frames":[]})),
            (4, "fatal", 21, json!({"kind":"native-crash","exception_type":1,"signal":11,"frames":[]})),
        ] {
            let mut report = event(id);
            report["severity"] = json!(severity); report["payload"] = payload;
            let occurred = report["occurred_at_unix_ms"].as_u64().unwrap();
            let inputs = sanitize_batch(&json!({"version":1,"events":[report]}),"client/one","person/ada","custom/client/one",now).unwrap();
            store.ingest_client_diagnostics("client/one",&inputs,now).unwrap();
            let records = store.local_observations_tail(1).unwrap();
            let logs = crate::otlp::otlp_logs("diagnostic-test",&records);
            let log = &logs["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
            assert_eq!(log["severityNumber"],number);
            assert_eq!(log["timeUnixNano"],(u128::from(occurred)*1_000_000).to_string());
            assert_eq!(log["observedTimeUnixNano"],(u128::from(now)*1_000_000).to_string());
            assert!(!logs.to_string().contains("private"));
            assert!(log["attributes"].as_array().unwrap().iter().any(|attribute|attribute["key"]=="st3.client.update_id"));
        }
        let mut malformed = event(5);
        for value in [json!(u64::from(u32::MAX)+1),json!(-1)] {
            malformed["sequence"] = value;
            assert!(sanitize_batch(&json!({"version":1,"events":[malformed.clone()]}),"client/one","person/ada","custom/client/one",now).is_err());
        }
        malformed = event(6); malformed["app_version"] = json!("x".repeat(65));
        assert!(sanitize_batch(&json!({"version":1,"events":[malformed]}),"client/one","person/ada","custom/client/one",now).is_err());
        // A receipt outlives observation retention and still acknowledges the retry.
        // Local retention preserves the newest row of each subject/kind. Add a
        // later launch so the first launch can be trimmed without changing that policy.
        let mut later = event(7);
        later["payload"] = json!({"kind":"launch","breadcrumb":"root-mounted","inferred":false});
        let inputs = sanitize_batch(&json!({"version":1,"events":[later]}),"client/one","person/ada","custom/client/one",now).unwrap();
        store.ingest_client_diagnostics("client/one",&inputs,now).unwrap();
        assert_eq!(store.trim_local_observations(u128::from(now)+1,100,100).unwrap(),1);
        let retained = store.local_observations_tail(10).unwrap();
        assert_eq!(retained.len(),4);
        assert!(retained.iter().all(|record|record.body["fields"]["event_id"] != uuid::Uuid::from_u128(1).to_string()));
        let inputs = sanitize_batch(&json!({"version":1,"events":[event(1)]}),"client/one","person/ada","custom/client/one",now).unwrap();
        store.ingest_client_diagnostics("client/one",&inputs,now).unwrap();
        assert_eq!(serde_json::to_value(store.local_observations_tail(10).unwrap()).unwrap(),serde_json::to_value(retained).unwrap());
    }
}
