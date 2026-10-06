//! Authority of raw PEEK observation, independent of single-use acquisition capabilities.
use super::*;
use rusqlite::{Connection, OptionalExtension as _, params};
use std::sync::LazyLock;

fn epoch() -> &'static str {
    static EPOCH: LazyLock<String> = LazyLock::new(|| uuid::Uuid::now_v7().to_string());
    &EPOCH
}

fn latest(
    connection: &Connection,
    subject: &str,
    kind: &str,
) -> anyhow::Result<Option<(String, Value)>> {
    let query = smallclaims::store::canonical_sql(
        "SELECT origin,body FROM claims WHERE subject=?1 AND kind=?2 ORDER BY CANONICAL_DESC(claims) LIMIT 1",
    );
    let row: Option<(String, String)> = connection
        .query_row(&query, params![subject, kind], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?;
    row.map(|(origin, body)| Ok((origin, serde_json::from_str(&body)?)))
        .transpose()
}

/// A local free-person session has an explicit daemon/principal epoch, not a fabricated grant.
/// Revoking a principal key or changing its declaration changes this epoch synchronously.
fn principal_epoch(connection: &Connection, person: &str) -> anyhow::Result<Value> {
    let mut statement = connection.prepare(
        "SELECT id FROM claims WHERE subject=?1 AND kind IN ('principal.key-granted','principal.key-revoked') ORDER BY id",
    )?;
    let keys = statement
        .query_map([person], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let revision: Option<String> = connection
        .query_row(
            "SELECT revision FROM desired WHERE subject=?1",
            [person],
            |row| row.get(0),
        )
        .optional()?;
    Ok(json!({"daemon_epoch":epoch(), "person":person, "revision":revision, "keys":keys}))
}

fn authority(connection: &Connection, actor: &str, person: &str) -> anyhow::Result<Value> {
    let principal = principal_epoch(connection, person)?;
    if actor == person {
        return Ok(json!({"local_principal_epoch":principal}));
    }
    let query = smallclaims::store::canonical_sql(
        "SELECT subject,body,origin FROM claims WHERE kind='custom.client.pairing-completed' AND json_extract(body,'$.fields.session_actor')=?1 ORDER BY CANONICAL_DESC(claims) LIMIT 1",
    );
    let (subject, body, issuer): (String, String, String) =
        connection.query_row(&query, [actor], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    let paired: Value = serde_json::from_str(&body)?;
    anyhow::ensure!(
        paired.pointer("/fields/person_id").and_then(Value::as_str) == Some(person),
        "person changed"
    );
    anyhow::ensure!(
        paired
            .pointer("/fields/expires_at_unix_ms")
            .and_then(Value::as_u64)
            .map(u128::from)
            .is_some_and(|expiry| expiry > client_now_ms()),
        "grant expired"
    );
    anyhow::ensure!(
        paired
            .pointer("/fields/scopes")
            .and_then(Value::as_array)
            .is_some_and(|scopes| scopes
                .iter()
                .any(|scope| scope.as_str() == Some("terminal.read"))),
        "terminal.read revoked"
    );
    // A revoked pairing is never reused; a replacement has a new session actor.
    anyhow::ensure!(
        latest(connection, &subject, "custom.client.pairing-revoked")?.is_none(),
        "pairing revoked"
    );
    Ok(
        json!({"principal":principal,"pairing":subject,"grant":paired,"issuer":client_host_id(&issuer)}),
    )
}

pub(super) fn authorization_epoch(
    state: &AppState,
    session: &ClientSession,
) -> Result<String, ApiError> {
    let connection = state.store.readers.get();
    let authority = authority(&connection, &session.actor, &session.authority_actor)
        .map_err(|error| forbidden(error.to_string()))?;
    if authority
        .get("issuer")
        .and_then(Value::as_str)
        .is_some_and(|issuer| issuer != client_host_id(&state.node))
    {
        return Err(forbidden(
            "raw PEEK requires the pairing issuer gateway's authoritative watch",
        ));
    }
    serde_json::to_string(&authority)
        .map(|authority| credential_digest(&authority))
        .map_err(ApiError::internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, AppState, ClientSession) {
        let root = tempfile::tempdir().unwrap();
        let state = AppState {
            store: Arc::new(Store::open_memory("lease-owner").unwrap()),
            notify: Arc::new(Notify::new()),
            event_notify: watch::channel(0).0,
            node: "lease-owner".into(),
            state_dir: root.path().into(),
            pty_root: root.path().join("pty"),
            pty_binary: root.path().join("unused-pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: crate::model::PlannerSpec::default(),
        };
        claim(
            &state,
            "agent/shell",
            "runtime.observed",
            json!({"status":"running","runtime_id":"runtime-shell","incarnation_id":"incarnation-one","terminal":true}),
        );
        (
            root,
            state,
            ClientSession::local(Some("person/alex")).unwrap(),
        )
    }

    fn claim(state: &AppState, subject: &str, kind: &str, fields: Value) {
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: None,
                fields: fields
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }

    #[tokio::test]
    async fn non_issuer_pairing_revoke_fails_without_committing() {
        let (_root, mut state, session) = fixture();
        let subject = "custom/client/issuer-only";
        claim(
            &state,
            subject,
            "custom.client.pairing-completed",
            json!({
                "device_id":"device/issuer-only",
                "person_id":"person/alex",
                "session_actor":"client/issuer-only",
                "scopes":["terminal.read"],
                "expires_at_unix_ms":client_now_ms()+600_000
            }),
        );
        // The claim keeps its issuer origin while the API runs on another member.
        state.node = "other-member".into();
        let snapshot = new_client_snapshot(&state);
        let request = ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: "action/issuer-only".into(),
            action_type: "pairing.revoke".into(),
            idempotency_key: "issuer-only".into(),
            fence: Fence {
                snapshot_id: snapshot.id.clone(),
                ..Default::default()
            },
            parameters: json!({"target_id":"device/issuer-only"}),
        };
        let error = dispatch_action(&state, &snapshot, &session, &request)
            .await
            .unwrap_err();
        assert_eq!(error.code, "issuer-required");
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(
            error.details.get("issuer_host_id"),
            Some(&json!("host/lease-owner"))
        );
        assert!(
            state
                .store
                .claims_for_subject_kind_at(subject, "custom.client.pairing-revoked", None, true, 1)
                .unwrap()
                .claims
                .is_empty()
        );
        state.node = "lease-owner".into();
        dispatch_action(&state, &new_client_snapshot(&state), &session, &request)
            .await
            .unwrap();
        assert_eq!(
            state
                .store
                .claims_for_subject_kind_at(subject, "custom.client.pairing-revoked", None, true, 1)
                .unwrap()
                .claims
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn principal_key_changes_move_the_authorization_epoch() {
        let (_root, state, session) = fixture();
        let acquired = authorization_epoch(&state, &session).unwrap();
        claim(
            &state,
            "person/alex",
            "principal.key-revoked",
            json!({"key":"acquired-key","reason":"race"}),
        );
        assert_ne!(authorization_epoch(&state, &session).unwrap(), acquired);
    }
}
