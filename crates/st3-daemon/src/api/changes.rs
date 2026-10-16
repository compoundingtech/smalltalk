use super::*;
use crate::changes::{service::Entry, view};

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReadQuery {
    repository: String,
    #[serde(default)]
    offline: bool,
    limit: Option<usize>,
    cursor: Option<String>,
    id: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    repository: String,
    pin: String,
    after: String,
    offline: bool,
}

fn entry(state: &AppState, repository: &str) -> Result<Arc<Entry>, ApiError> {
    state
        .store
        .git_change_registry()
        .and_then(|registry| registry.entry(repository))
        .ok_or_else(|| {
            ApiError::not_found("Git change repository is not configured; use the PR path")
        })
}

fn owner_diagnostics(entry: &Entry) -> Value {
    json!({"reader_node":entry.configuration.reader_node,"service_user":entry.service_user,
        "credential_source":entry.configuration.credential_source_name,
        "credential_helper_configured":entry.configuration.credential_helper.is_some(),
        "ssh_command_configured":entry.configuration.ssh_command.is_some(),
        "shared_account_isolation":"not_established"})
}

pub(super) async fn list(
    State(state): State<AppState>,
    Extension(session): Extension<client_v0::ClientSession>,
    Query(query): Query<ReadQuery>,
) -> Result<Json<Value>, ApiError> {
    client_v0::require_scope(&session, "read.projections")?;
    let limit = query.limit.unwrap_or(50);
    if !(1..=50).contains(&limit) || query.id.is_some() {
        return Err(client_v0::validation(
            "change list accepts a limit of 1 through 50 and no detail ID",
        ));
    }
    let entry = entry(&state, &query.repository)?;
    let cursor = query
        .cursor
        .as_deref()
        .map(|encoded| {
            if encoded.len() > 2048 {
                return Err(client_v0::validation("change cursor exceeds budget"));
            }
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded)
                .map_err(|_| client_v0::validation("invalid change cursor"))?;
            let cursor: Cursor = serde_json::from_slice(&bytes)
                .map_err(|_| client_v0::validation("invalid change cursor"))?;
            if cursor.repository != query.repository || cursor.offline != query.offline {
                return Err(client_v0::validation(
                    "change cursor does not match repository or mode",
                ));
            }
            crate::changes::change_id(&cursor.after)
                .map_err(|error| client_v0::validation(error.to_string()))?;
            if cursor.pin.len() != 64
                || !cursor
                    .pin
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            {
                return Err(client_v0::validation("invalid change observation pin"));
            }
            Ok(cursor)
        })
        .transpose()?;
    let (pin, observed) = entry
        .snapshot(
            query.offline,
            cursor.as_ref().map(|cursor| cursor.pin.as_str()),
        )
        .await
        .map_err(|error| client_v0::validation(error.to_string()))?;
    let store = state.store.clone();
    let repository = query.repository.clone();
    let after = cursor.map(|cursor| cursor.after).unwrap_or_default();
    let page =
        blocking_store(move || view::list(&store, &repository, observed, &after, limit)).await?;
    let next_cursor = page
        .next_after
        .as_ref()
        .map(|after| {
            serde_json::to_vec(&Cursor {
                repository: query.repository.clone(),
                pin,
                after: after.clone(),
                offline: query.offline,
            })
            .map(|bytes| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
        })
        .transpose()
        .map_err(ApiError::internal)?;
    let mut value = serde_json::to_value(page).map_err(ApiError::internal)?;
    value["next_cursor"] = json!(next_cursor);
    value["fallback"] = json!("pull_request");
    value["git_owner"] = owner_diagnostics(&entry);
    Ok(Json(value))
}

pub(super) async fn show(
    State(state): State<AppState>,
    Extension(session): Extension<client_v0::ClientSession>,
    Query(query): Query<ReadQuery>,
) -> Result<Json<Value>, ApiError> {
    client_v0::require_scope(&session, "read.projections")?;
    let entry = entry(&state, &query.repository)?;
    let id = query
        .id
        .ok_or_else(|| client_v0::validation("change ID is required"))?;
    crate::changes::change_id(&id).map_err(|error| client_v0::validation(error.to_string()))?;
    if query.limit.is_some() || query.cursor.is_some() {
        return Err(client_v0::validation(
            "change detail does not accept list pagination",
        ));
    }
    let observed = entry
        .observation(query.offline)
        .await
        .map_err(ApiError::internal)?;
    let store = state.store.clone();
    let repository = query.repository;
    let detail = blocking_store(move || view::show(&store, &repository, observed, &id)).await?;
    let mut value = serde_json::to_value(detail).map_err(ApiError::internal)?;
    value["fallback"] = json!("pull_request");
    value["git_owner"] = owner_diagnostics(&entry);
    Ok(Json(value))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SyncRequest {
    repository: String,
}

pub(super) async fn sync(
    State(state): State<AppState>,
    Extension(session): Extension<client_v0::ClientSession>,
    Json(request): Json<SyncRequest>,
) -> Result<Json<Value>, ApiError> {
    client_v0::require_scope(&session, "control.work")?;
    let entry = entry(&state, &request.repository)?;
    let admission = entry
        .sync(state.store.clone())
        .await
        .map_err(ApiError::internal)?;
    state.notify.notify_waiters();
    Ok(Json(
        json!({"repository":request.repository,"sync":admission,"fallback":"pull_request"}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::config::{Config, Repository};
    use std::path::PathBuf;

    fn configured(state: &AppState) {
        state
            .store
            .install_git_change_registry(Arc::new(
                crate::changes::service::Registry::new(
                    &Config {
                        enabled: true,
                        repositories: vec![Repository {
                            name: "fixture/garden".into(),
                            reader_node: "fixture-other".into(),
                            transport_directory: PathBuf::from("/missing/fixture/transport"),
                            remote: "origin".into(),
                            remote_url: "https://example.invalid/fixture/garden.git".into(),
                            pilot_started_at: "2026-01-01T00:00:00Z".into(),
                            ..Repository::default()
                        }],
                    },
                    &state.node,
                )
                .unwrap(),
            ))
            .unwrap();
    }

    fn session(scopes: &[&str]) -> Extension<client_v0::ClientSession> {
        Extension(client_v0::ClientSession::for_tests_scoped(
            "person/fixture",
            "person/fixture",
            scopes,
        ))
    }

    #[tokio::test]
    async fn scope_and_argument_refusals_precede_discovery() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::api::tests::state(root.path());
        configured(&state);
        let denied = list(
            State(state.clone()),
            session(&[]),
            Query(ReadQuery {
                repository: "fixture/garden".into(),
                ..ReadQuery::default()
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(denied.status, StatusCode::FORBIDDEN);
        let invalid = list(
            State(state.clone()),
            session(&["read.projections"]),
            Query(ReadQuery {
                repository: "fixture/garden".into(),
                limit: Some(51),
                ..ReadQuery::default()
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(invalid.code, "validation-failed");
        let invalid = show(
            State(state),
            session(&["read.projections"]),
            Query(ReadQuery {
                repository: "fixture/garden".into(),
                id: Some("not-a-change-id".into()),
                ..ReadQuery::default()
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(invalid.code, "validation-failed");
    }

    #[tokio::test]
    async fn offline_pages_are_bounded_and_cursor_scope_is_explicit() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::api::tests::state(root.path());
        configured(&state);
        for number in 1..=2 {
            let mut revision = crate::store::git_changes::tests::revision();
            revision.oid = format!("{number:040x}");
            revision.record.change_id = format!("I{number:040x}");
            state
                .store
                .index_git_change("fixture/garden", &revision)
                .unwrap();
        }
        let page = list(
            State(state.clone()),
            session(&["read.projections"]),
            Query(ReadQuery {
                repository: "fixture/garden".into(),
                offline: true,
                limit: Some(1),
                ..ReadQuery::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert_eq!(page["items"][0]["status"], "offline");
        assert!(page["observation"]["refs_observed_at"].is_null());
        assert!(page["observation"]["error"].is_string());
        assert_eq!(page["fallback"], "pull_request");
        let cursor = page["next_cursor"].as_str().unwrap().to_owned();
        let wrong_mode = list(
            State(state.clone()),
            session(&["read.projections"]),
            Query(ReadQuery {
                repository: "fixture/garden".into(),
                cursor: Some(cursor.clone()),
                ..ReadQuery::default()
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(wrong_mode.code, "validation-failed");
        let next = list(
            State(state),
            session(&["read.projections"]),
            Query(ReadQuery {
                repository: "fixture/garden".into(),
                offline: true,
                cursor: Some(cursor),
                limit: Some(1),
                ..ReadQuery::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(next["items"].as_array().unwrap().len(), 1);
        assert_ne!(next["items"][0]["change_id"], page["items"][0]["change_id"]);
        assert!(next["next_cursor"].is_null());
    }
    #[tokio::test]
    async fn explicit_sync_requires_control_scope_and_reports_named_unavailable_owner() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::api::tests::state(root.path());
        configured(&state);
        let denied = sync(
            State(state.clone()),
            session(&["read.projections"]),
            Json(SyncRequest {
                repository: "fixture/garden".into(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(denied.status, StatusCode::FORBIDDEN);
        let value = sync(
            State(state),
            session(&["control.work"]),
            Json(SyncRequest {
                repository: "fixture/garden".into(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(value["sync"]["status"], "unavailable");
        assert_eq!(value["sync"]["reader_node"], "fixture-other");
        assert_eq!(value["fallback"], "pull_request");
    }
}
