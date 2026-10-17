use super::*;
use crate::store::push_registrations::{PushEnvironment, PushRegistration};

const GRANT: &str = "custom/client/push-device";
const PERSON: &str = "person/example";
const ACTOR: &str = "person/example/session/phone";
const TOKEN: &str = "a1b2c3d4e5f60123456789abcdef00112";

fn pair(state: &AppState) {
    state
        .store
        .append_claim(&ClaimInput {
            subject: GRANT.into(),
            kind: "custom.client.pairing-completed".into(),
            actor: Some(PERSON.into()),
            fields: serde_json::from_value(json!({
                "device_id":"device/phone","person_id":PERSON,"session_actor":ACTOR,
                "scopes":["read.projections"],"expires_at_unix_ms":client_now_ms() as u64 + 600_000
            }))
            .unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

fn register(state: &AppState) {
    let registration = PushRegistration {
        token: TOKEN.into(),
        environment: PushEnvironment::Development,
        bundle_id: "org.example.push".into(),
        app_version: "1.0".into(),
    };
    state
        .store
        .update_push_registration(
            GRANT,
            PERSON,
            ACTOR,
            Some(&registration),
            || client_now_ms() as u64,
        )
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn device_push_summary_is_secret_free_and_pairing_revoke_deletes_the_local_row() {
    let root = tempfile::tempdir().unwrap();
    let state = tests::test_state_named(root.path(), "push-device-test");
    pair(&state);
    let session = ClientSession::local(Some(PERSON)).unwrap();
    let capability = capabilities(&session)
        .into_iter()
        .find(|entry| entry["id"] == "push-registration")
        .unwrap();
    assert_eq!(capability["state"], "unavailable");
    let resources = device_resources(&state, &new_client_snapshot(&state), PERSON).unwrap();
    assert_eq!(resources[0]["push"], json!({"registered":false}));
    register(&state);
    let resources = device_resources(&state, &new_client_snapshot(&state), PERSON).unwrap();
    assert_eq!(
        resources[0]["push"],
        json!({"registered":true,"environment":"development"})
    );
    let serialized = serde_json::to_string(&resources).unwrap();
    let debug = format!("{resources:?}");
    assert!(!serialized.contains(TOKEN));
    assert!(!debug.contains(TOKEN));
    assert!(!serialized.contains("org.example.push"));
    let snapshot = new_client_snapshot(&state);
    let action = ActionRequest {
        api_version: CLIENT_API_VERSION.into(),
        id: "action/push-revoke".into(),
        action_type: "pairing.revoke".into(),
        idempotency_key: "push-revoke".into(),
        fence: Fence {
            snapshot_id: snapshot.id.clone(),
            ..Default::default()
        },
        parameters: json!({"target_id":"device/phone"}),
    };
    assert_eq!(
        dispatch_action(&state, &snapshot, &session, &action)
            .await
            .unwrap(),
        vec!["device/phone"]
    );
    let count: u64 = state
        .store
        .readers
        .get()
        .query_row("SELECT COUNT(*) FROM local_push_registrations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
    let resources = device_resources(&state, &new_client_snapshot(&state), PERSON).unwrap();
    assert_eq!(resources[0]["state"], "revoked");
    assert_eq!(resources[0]["push"], json!({"registered":false}));
}

#[test]
fn device_push_summary_is_omitted_on_nonissuer_members() {
    let root = tempfile::tempdir().unwrap();
    let mut state = tests::test_state_named(root.path(), "push-device-test");
    pair(&state);
    state.node = "other-member".into();
    let resources = device_resources(&state, &new_client_snapshot(&state), PERSON).unwrap();
    assert!(resources[0].get("push").is_none());
}

#[test]
fn push_registration_capability_is_granted_only_on_the_pairing_issuer() {
    let root = tempfile::tempdir().unwrap();
    let state = tests::test_state_named(root.path(), "push-device-test");
    pair(&state);
    let mut grant = state
        .store
        .claims_for_subject_kind_at(GRANT, "custom.client.pairing-completed", None, true, 1)
        .unwrap()
        .claims
        .remove(0);
    let session = paired_client_session(&state, &grant, "fabric-loopback", false).unwrap();
    let capability = capabilities(&session)
        .into_iter()
        .find(|entry| entry["id"] == "push-registration")
        .unwrap();
    assert_eq!(capability["state"], "granted");
    grant.origin = "another-member".into();
    let session = paired_client_session(&state, &grant, "fabric-loopback", false).unwrap();
    let capability = capabilities(&session)
        .into_iter()
        .find(|entry| entry["id"] == "push-registration")
        .unwrap();
    assert_eq!(capability["state"], "unavailable");
}

#[tokio::test]
async fn push_secret_expiry_cleanup_runs_at_startup_without_any_attention_reader() {
    let root = tempfile::tempdir().unwrap();
    let state = tests::test_state_named(root.path(), "push-device-test");
    pair(&state);
    register(&state);
    state
        .store
        .connection
        .lock()
        .expect("fixture writer")
        .execute("UPDATE local_push_registrations SET expires_at=0", [])
        .unwrap();
    super::super::client_push::start_push_registration_cleanup(&state);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let count: u64 = state
                .store
                .readers
                .get()
                .query_row("SELECT COUNT(*) FROM local_push_registrations", [], |row| {
                    row.get(0)
                })
                .unwrap();
            if count == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
