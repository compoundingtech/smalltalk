//! Paired push targets are local secrets. Nothing in this file appends a graph claim.
use super::client_v0::{ClientSession, revalidate_session};
use super::*;
use axum::extract::rejection::JsonRejection;
use rusqlite::{Connection, params};
use serde::Deserialize;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

pub(super) const MAX_BODY_BYTES: usize = 2048;
static ADMISSION: OnceLock<Mutex<()>> = OnceLock::new();
static WAKE: OnceLock<Mutex<BTreeMap<PathBuf, Arc<Notify>>>> = OnceLock::new();

fn wake(dir: &Path) -> Result<Arc<Notify>, ApiError> {
    let mut wakes = WAKE
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .map_err(|_| unavailable())?;
    Ok(wakes
        .entry(dir.to_owned())
        .or_insert_with(|| Arc::new(Notify::new()))
        .clone())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Registration {
    token: String,
    environment: String,
    bundle_id: String,
    app_version: String,
}

fn invalid() -> ApiError {
    ApiError::bad(St3Error::new(
        "validation-failed",
        "invalid push registration",
    ))
}

fn unavailable() -> ApiError {
    ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "local-registration-unavailable".into(),
        message: "local push registration storage is unavailable".into(),
        details: Box::default(),
    }
}

fn validate(registration: &Registration) -> Result<(), ApiError> {
    // APNs device tokens are opaque bytes; the phone sends their lowercase hex form.
    if registration.token.len() < 64
        || registration.token.len() > 512
        || registration.token.len() % 2 != 0
        || !registration
            .token
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        || !matches!(
            registration.environment.as_str(),
            "development" | "production"
        )
        || registration.bundle_id.is_empty()
        || registration.bundle_id.len() > 255
        || !registration
            .bundle_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-'))
        || registration.app_version.is_empty()
        || registration.app_version.len() > 64
        || !registration
            .app_version
            .bytes()
            .all(|c| c.is_ascii_graphic())
    {
        return Err(invalid());
    }
    Ok(())
}

fn open(dir: &Path) -> Result<Connection, ApiError> {
    let path = dir.join("push-registrations.sqlite3");
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(unavailable()),
        }
        use std::os::unix::fs::PermissionsExt;
        if path
            .symlink_metadata()
            .map_err(|_| unavailable())?
            .file_type()
            .is_symlink()
        {
            return Err(unavailable());
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| unavailable())?;
    }
    #[cfg(not(unix))]
    OpenOptions::new()
        .write(true)
        .create(true)
        .open(&path)
        .map_err(|_| unavailable())?;
    let connection = Connection::open(path).map_err(|_| unavailable())?;
    connection
        .execute_batch(
            "PRAGMA secure_delete=ON;
    CREATE TABLE IF NOT EXISTS push_registrations (
        person TEXT NOT NULL, device TEXT NOT NULL, grant TEXT NOT NULL,
        token TEXT NOT NULL, environment TEXT NOT NULL, bundle_id TEXT NOT NULL,
        app_version TEXT NOT NULL, expires_at INTEGER NOT NULL,
        PRIMARY KEY(person, device)
    ); CREATE INDEX IF NOT EXISTS push_registrations_expiry ON push_registrations(expires_at);",
        )
        .map_err(|_| unavailable())?;
    Ok(connection)
}

pub(super) fn paired(
    state: &AppState,
    session: &ClientSession,
) -> Result<(String, String, String, i64), ApiError> {
    let session = revalidate_session(state, session)?;
    let grant = session
        .pairing_grant
        .ok_or_else(|| client_v0::validation("a paired session is required"))?;
    let claim = state
        .store
        .claims_for_subject_kind_at(&grant, "custom.client.pairing-completed", None, true, 1)
        .map_err(ApiError::internal)?
        .claims
        .into_iter()
        .next()
        .ok_or_else(|| client_v0::validation("the pairing grant is absent"))?;
    if claim.origin != state.node {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            code: "issuer-required".into(),
            message: "push registration belongs on the pairing issuer".into(),
            details: Box::default(),
        });
    }
    let device = claim
        .body
        .pointer("/fields/device_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("paired device is absent"))?
        .to_owned();
    let expires = claim
        .body
        .pointer("/fields/expires_at_unix_ms")
        .and_then(Value::as_i64)
        .ok_or_else(|| ApiError::internal("pairing expiry is absent"))?;
    Ok((session.authority_actor, device, grant, expires))
}

pub(super) async fn register(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    body: Result<Json<Registration>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(registration) = body.map_err(|_| invalid())?;
    validate(&registration)?;
    let changed_state = state.clone();
    tokio::task::spawn_blocking(move || {
        let _guard = ADMISSION
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| unavailable())?;
        let (person, device, grant, expires) = paired(&state, &session)?;
        let connection = open(&state.state_dir)?;
        connection
            .execute(
                "INSERT INTO push_registrations
            (person,device,grant,token,environment,bundle_id,app_version,expires_at)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
            ON CONFLICT(person,device) DO UPDATE SET grant=excluded.grant,token=excluded.token,
            environment=excluded.environment,bundle_id=excluded.bundle_id,
            app_version=excluded.app_version,expires_at=excluded.expires_at",
                params![
                    person,
                    device,
                    grant,
                    registration.token,
                    registration.environment,
                    registration.bundle_id,
                    registration.app_version,
                    expires
                ],
            )
            .map_err(|_| unavailable())?;
        Ok::<_, ApiError>(())
    })
    .await
    .map_err(|_| unavailable())??;
    wake(&changed_state.state_dir)?.notify_one();
    Ok(Json(json!({"registered":true})))
}

pub(super) async fn revoke(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
) -> Result<Json<Value>, ApiError> {
    tokio::task::spawn_blocking(move || {
        let _guard = ADMISSION
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| unavailable())?;
        let (person, device, grant, _) = paired(&state, &session)?;
        let connection = open(&state.state_dir)?;
        connection
            .execute(
                "DELETE FROM push_registrations WHERE person=?1 AND device=?2 AND grant=?3",
                params![person, device, grant],
            )
            .map_err(|_| unavailable())?;
        Ok::<_, ApiError>(())
    })
    .await
    .map_err(|_| unavailable())??;
    Ok(Json(json!({"revoked":true})))
}

pub(super) fn revoked_pairing(state: &AppState, grant: &str) -> Result<(), ApiError> {
    let _guard = ADMISSION
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| unavailable())?;
    let connection = open(&state.state_dir)?;
    connection
        .execute("DELETE FROM push_registrations WHERE grant=?1", [grant])
        .map_err(|_| unavailable())?;
    Ok(())
}

fn clean(state: &AppState) -> Result<Option<u64>, ApiError> {
    let _guard = ADMISSION
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| unavailable())?;
    let connection = open(&state.state_dir)?;
    let now = i64::try_from(client_now_ms()).map_err(|_| unavailable())?;
    connection
        .execute("DELETE FROM push_registrations WHERE expires_at<=?1", [now])
        .map_err(|_| unavailable())?;
    let mut statement = connection
        .prepare("SELECT person,device,grant,expires_at FROM push_registrations")
        .map_err(|_| unavailable())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|_| unavailable())?;
    let mut entries = Vec::new();
    for row in rows {
        entries.push(row.map_err(|_| unavailable())?);
    }
    drop(statement);
    for (person, device, grant, _) in entries {
        let completed = state
            .store
            .claims_for_subject_kind_at(&grant, "custom.client.pairing-completed", None, true, 1)
            .map_err(ApiError::internal)?
            .claims
            .into_iter()
            .next();
        let revoked = state
            .store
            .claims_for_subject_kind_at(&grant, "custom.client.pairing-revoked", None, true, 1)
            .map_err(ApiError::internal)?
            .claims
            .into_iter()
            .next();
        let live = completed.is_some_and(|claim| {
            claim.origin == state.node
                && claim
                    .body
                    .pointer("/fields/person_id")
                    .and_then(Value::as_str)
                    == Some(person.as_str())
                && claim
                    .body
                    .pointer("/fields/device_id")
                    .and_then(Value::as_str)
                    == Some(device.as_str())
                && revoked
                    .as_ref()
                    .is_none_or(|r| r.store_index <= claim.store_index)
        });
        if !live {
            connection
                .execute(
                    "DELETE FROM push_registrations WHERE person=?1 AND device=?2 AND grant=?3",
                    params![person, device, grant],
                )
                .map_err(|_| unavailable())?;
        }
    }
    let next: Option<i64> = connection
        .query_row(
            "SELECT MIN(expires_at) FROM push_registrations",
            [],
            |row| row.get(0),
        )
        .map_err(|_| unavailable())?;
    let scheduled_at = i64::try_from(client_now_ms()).map_err(|_| unavailable())?;
    Ok(next.and_then(|expiry| u64::try_from(expiry.saturating_sub(scheduled_at)).ok()))
}

pub(super) fn start_cleanup(state: AppState) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let Ok(notify) = wake(&state.state_dir) else {
        return;
    };
    runtime.spawn(async move {
        loop {
            let current = state.clone();
            let expiry = tokio::task::spawn_blocking(move || clean(&current)).await;
            let expiry = match expiry {
                Ok(Ok(expiry)) => expiry,
                _ => {
                    tracing::error!("local push registration cleanup failed");
                    None
                }
            };
            match expiry {
                Some(milliseconds) => tokio::select! {
                    _ = notify.notified() => {},
                    _ = tokio::time::sleep(Duration::from_millis(milliseconds.max(1))) => {},
                },
                None => notify.notified().await,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OptionalExtension;

    fn state(root: &Path) -> AppState {
        AppState {
            store: Arc::new(Store::open(&root.join("claims.sqlite3"), "push-test").unwrap()),
            notify: Arc::new(Notify::new()),
            event_notify: watch::channel(0u64).0,
            node: "push-test".into(),
            state_dir: root.into(),
            pty_root: root.join("pty"),
            pty_binary: root.join("unused"),
            fleet_id: None,
            configured_peers: vec![],
            client_relay: None,
            native_session_home: None,
            planner_default: Default::default(),
        }
    }

    fn pairing(state: &AppState, grant: &str, expires: u64) -> ClientSession {
        let claim = state
            .store
            .append_claim(&ClaimInput {
                subject: grant.into(),
                kind: "custom.client.pairing-completed".into(),
                actor: Some("person/fixture".into()),
                fields: BTreeMap::from([
                    ("device_id".into(), json!("device/fixture")),
                    ("person_id".into(), json!("person/fixture")),
                    ("session_actor".into(), json!("client/fixture")),
                    ("expires_at_unix_ms".into(), json!(expires)),
                    ("scopes".into(), json!([])),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        client_v0::paired_client_session(state, &claim, "fabric-loopback", false).unwrap()
    }

    fn registration(token: char) -> Registration {
        Registration {
            token: token.to_string().repeat(64),
            environment: "development".into(),
            bundle_id: "test.example.fixture".into(),
            app_version: "1.0".into(),
        }
    }

    fn token(state: &AppState) -> Option<String> {
        open(&state.state_dir).unwrap().query_row(
            "SELECT token FROM push_registrations WHERE person='person/fixture' AND device='device/fixture'",
            [], |row| row.get(0),
        ).optional().unwrap()
    }

    #[tokio::test]
    async fn registration_replaces_exact_local_target_and_unregisters() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let session = pairing(
            &state,
            "custom/client/push-one",
            client_now_ms() as u64 + 60_000,
        );
        let first = register(
            State(state.clone()),
            Extension(session.clone()),
            Ok(Json(registration('a'))),
        )
        .await
        .unwrap();
        assert_eq!(first.0, json!({"registered":true}));
        assert_eq!(token(&state), Some("a".repeat(64)));
        assert!(
            !state
                .store
                .claims_for_subject_kind_at(
                    "custom/client/push-one",
                    "custom.client.pairing-completed",
                    None,
                    true,
                    1,
                )
                .unwrap()
                .claims[0]
                .body
                .to_string()
                .contains(&"a".repeat(64))
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(state.state_dir.join("push-registrations.sqlite3"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
        register(
            State(state.clone()),
            Extension(session.clone()),
            Ok(Json(registration('b'))),
        )
        .await
        .unwrap();
        assert_eq!(token(&state), Some("b".repeat(64)));
        let rows: i64 = open(&state.state_dir)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM push_registrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 1);
        let result = revoke(State(state.clone()), Extension(session))
            .await
            .unwrap();
        assert_eq!(result.0, json!({"revoked":true}));
        assert_eq!(token(&state), None);
    }

    #[tokio::test]
    async fn pairing_revocation_removes_target_and_blocks_registration() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let session = pairing(
            &state,
            "custom/client/push-revoked",
            client_now_ms() as u64 + 60_000,
        );
        register(
            State(state.clone()),
            Extension(session.clone()),
            Ok(Json(registration('a'))),
        )
        .await
        .unwrap();
        state
            .store
            .append_claim(&ClaimInput {
                subject: "custom/client/push-revoked".into(),
                kind: "custom.client.pairing-revoked".into(),
                actor: Some("person/fixture".into()),
                fields: BTreeMap::new(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        revoked_pairing(&state, "custom/client/push-revoked").unwrap();
        assert_eq!(token(&state), None);
        assert!(
            register(
                State(state.clone()),
                Extension(session),
                Ok(Json(registration('b')))
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn expiry_removes_target() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let session = pairing(
            &state,
            "custom/client/push-expiry",
            client_now_ms() as u64 + 60_000,
        );
        register(
            State(state.clone()),
            Extension(session),
            Ok(Json(registration('a'))),
        )
        .await
        .unwrap();
        open(&state.state_dir)
            .unwrap()
            .execute(
                "UPDATE push_registrations SET expires_at=?1",
                [client_now_ms() as i64 - 1],
            )
            .unwrap();
        clean(&state).unwrap();
        assert_eq!(token(&state), None);
    }

    #[tokio::test]
    async fn invalid_token_and_other_member_are_rejected_before_storage() {
        let root = tempfile::tempdir().unwrap();
        let mut state = state(root.path());
        let session = pairing(
            &state,
            "custom/client/push-issuer",
            client_now_ms() as u64 + 60_000,
        );
        let mut invalid = registration('a');
        invalid.token = "raw-secret".into();
        assert!(
            register(
                State(state.clone()),
                Extension(session.clone()),
                Ok(Json(invalid))
            )
            .await
            .is_err()
        );
        assert_eq!(token(&state), None);
        state.node = "another-member".into();
        assert!(
            register(
                State(state.clone()),
                Extension(session),
                Ok(Json(registration('a')))
            )
            .await
            .is_err()
        );
        assert_eq!(token(&state), None);
    }
}
