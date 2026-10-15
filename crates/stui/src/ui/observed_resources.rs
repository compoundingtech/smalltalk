//! Optional, bounded reads of graph resources offered by the sidebar.
use super::{
    doc::{Doc, DocExt, Hit},
    text, theme,
};
use st3_client::{Client, ResourceObservation, ResourcesFilter};
use std::collections::BTreeMap;

pub(super) async fn read(
    client: &Client,
    focused: Option<&str>,
) -> Result<(Vec<ResourceObservation>, bool), st3_client::ClientError> {
    // Separate kind windows keep a busy observer's comments from hiding all PRs and repos.
    let mut rows = BTreeMap::new();
    let mut has_more = false;
    for kind in [None, Some("vcs.pull-request"), Some("vcs.repository")] {
        let page = client
            .resources_list(
                &ResourcesFilter {
                    kind: kind.map(str::to_owned),
                    ..Default::default()
                },
                None,
                Some(200),
            )
            .await?
            .value;
        has_more |= page.page.has_more;
        rows.extend(page.items.into_iter().map(|row| (row.id.clone(), row)));
    }
    if let Some(id) = focused {
        let page = client
            .resources_list(
                &ResourcesFilter {
                    subject_prefix: Some(id.into()),
                    ..Default::default()
                },
                None,
                Some(1),
            )
            .await?
            .value;
        rows.extend(
            page.items
                .into_iter()
                .filter(|row| row.id == id)
                .map(|row| (row.id.clone(), row)),
        );
    }
    Ok((rows.into_values().collect(), has_more))
}
pub(super) fn kind(row: &ResourceObservation) -> &'static str {
    match row.kind.as_str() {
        "vcs.pull-request" => "Pull requests",
        "vcs.repository" => "Repositories",
        _ => "Resources",
    }
}
pub(super) fn title(row: &ResourceObservation) -> String {
    let string = |field| {
        row.facts
            .get(field)
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.trim().is_empty())
    };
    let label = if row.kind == "vcs.repository" {
        row.id.strip_prefix("resource/github/").unwrap_or(&row.id)
    } else {
        &row.id
    };
    let title = string("title")
        .or_else(|| string("name"))
        .or_else(|| string("repository"))
        .unwrap_or(label);
    if row.kind == "vcs.pull-request"
        && let Some(number) = row.facts.get("number").and_then(serde_json::Value::as_u64)
    {
        format!("#{number} {title}")
    } else {
        title.into()
    }
}
pub(super) fn detail(id: &str, row: Option<&ResourceObservation>, width: usize) -> Doc {
    let mut doc = Doc::new();
    doc.field("Subject", id, width, theme::fg(theme::TEXT));
    let Some(row) = row else {
        doc.field(
            "Status",
            "Resource unavailable in the current window",
            width,
            theme::dim(),
        );
        return doc;
    };
    doc.field("Title", &title(row), width, theme::strong(theme::TEXT));
    doc.field("Kind", &row.kind, width, theme::dim());
    doc.field("Observed", &row.observed_at, width, theme::dim());
    if let Some(url) = row
        .facts
        .get("url")
        .and_then(serde_json::Value::as_str)
        .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
    {
        doc.buttons(&[("open", "Open link", Hit::Link(url.into()), theme::ACCENT)]);
    }
    for (name, value) in &row.facts {
        // Keep graph subjects and unknown vocabulary kinds usable without guessing their schema.
        let value = value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| serde_json::to_string(value).unwrap());
        doc.field(&text::sanitize(name), &value, width, theme::fg(theme::TEXT));
    }
    doc
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn graph_resource_labels_keep_unknown_kinds_and_canonical_subjects() {
        let mut row: ResourceObservation = serde_json::from_value(serde_json::json!({"id":"resource/github/acme/garden/issue/9","kind":"vcs.pull-request","facts":{"number":9,"title":"Build garden"},"observed_at":"2026-10-11T00:00:00Z","opened_by":null,"opened_by_run":null})).unwrap();
        assert_eq!(title(&row), "#9 Build garden");
        assert_eq!(kind(&row), "Pull requests");
        row.kind = "custom.file".into();
        assert_eq!(kind(&row), "Resources");
        row.facts.clear();
        assert_eq!(title(&row), row.id);
        let doc = detail(&row.id, Some(&row), 80);
        assert!(!doc.lines.is_empty());
    }
}

#[cfg(test)]
mod socket_tests {
    use super::*;
    use std::sync::Arc;
    #[tokio::test]
    async fn optional_resource_read_lists_typed_prs_repositories_and_other_subjects_over_unix() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("resources.sock");
        let store = Arc::new(
            st3::store::Store::open(&root.path().join("graph.db"), "resource-read-test").unwrap(),
        );
        let state = st3::api::AppState {
            owning_person: None,
            store: store.clone(),
            notify: Arc::new(tokio::sync::Notify::new()),
            event_notify: tokio::sync::watch::channel(0_u64).0,
            node: "resource-read-test".into(),
            state_dir: root.path().into(),
            pty_root: root.path().join("pty"),
            pty_binary: "pty".into(),
            fleet_id: None,
            configured_peers: vec![],
            client_relay: None,
            native_session_home: None,
            planner_default: Default::default(),
        };
        for (id, kind, facts) in [
            (
                "resource/github/acme/garden/issue/9",
                "vcs.pull-request",
                serde_json::json!({"title":"Build garden","number":9,"state":"open"}),
            ),
            (
                "resource/github/acme/garden",
                "vcs.repository",
                serde_json::json!({"url":"https://github.com/acme/garden"}),
            ),
            (
                "resource/notes/one",
                "custom.acme.note",
                serde_json::json!({"title":"One note"}),
            ),
        ] {
            store
                .append_client_claim(&st3::model::ClaimInput {
                    subject: id.into(),
                    kind: "resource.observed".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("kind".into(), serde_json::json!(kind)),
                        ("facts".into(), facts),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        let path = socket.clone();
        let server = tokio::spawn(async move {
            st3::api::serve_unix(&path, st3::api::router(state))
                .await
                .unwrap();
        });
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let client = Client::unix_as(&socket, "person/avery");
        let (rows, has_more) = read(&client, Some("resource/github/acme/garden/issue/9"))
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            3,
            "overlapping kind windows deduplicate canonical subject IDs"
        );
        assert!(!has_more);
        let pull = rows.iter().find(|r| r.kind == "vcs.pull-request").unwrap();
        assert_eq!(title(pull), "#9 Build garden");
        let pull_id = pull.id.clone();
        let arrangement = format!("arrangement/person/avery/{}", uuid::Uuid::now_v7());
        let folder = uuid::Uuid::now_v7().to_string();
        let snapshot = client.capabilities().await.unwrap().snapshot;
        let (action, idempotency) = crate::feed::action_pair();
        client
            .arrangement_edit(
                action,
                idempotency,
                st3_client::Fence {
                    snapshot_id: snapshot.id,
                    ..Default::default()
                },
                st3_client::ArrangementEditParameters {
                    subject: arrangement.clone(),
                    owner: "person/avery".into(),
                    operations: vec![
                        st3_client::ArrangementOperation::Create {
                            name: "Sidebar".into(),
                        },
                        st3_client::ArrangementOperation::FolderCreate {
                            id: folder.clone(),
                            name: "Garden".into(),
                            parent: None,
                            key: "a0".into(),
                        },
                        st3_client::ArrangementOperation::SubjectPlace {
                            subject: pull.id.clone(),
                            folder: Some(folder.clone()),
                            key: "a0".into(),
                        },
                    ],
                },
            )
            .await
            .unwrap();
        let a = client
            .arrangements_get("avery", arrangement.rsplit('/').next().unwrap())
            .await
            .unwrap()
            .value;
        assert_eq!(a.resolved.unwrap().folders[&pull.id], Some(folder));
        let mut sidebar = super::super::resource_sidebar::Sidebar::default();
        sidebar.observations = rows;
        sidebar.observe(&super::super::demo::world());
        assert!(sidebar.rows().iter().any(|row| matches!(row, super::super::resource_sidebar::Row::Resource(row) if row.id == pull_id)));
        sidebar.observations.clear();
        sidebar.observe(&super::super::demo::world());
        assert!(sidebar.rows().iter().any(|row| matches!(row, super::super::resource_sidebar::Row::Resource(row) if row.id == pull_id && row.missing)));
        server.abort();
    }
}
