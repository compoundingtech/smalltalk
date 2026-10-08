//! Opt-in field replacements for an already delivered row. Full snapshot/new row semantics
//! remain authoritative; identity or resource-kind changes use a complete upsert.
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(super) fn changed_rows(
    previous: &BTreeMap<String, Value>,
    current: &BTreeMap<String, Value>,
    field_deltas: bool,
) -> (Vec<Value>, Vec<Value>) {
    let mut upserts = Vec::new();
    let mut patches = Vec::new();
    for (id, value) in current {
        let before = previous.get(id);
        if before == Some(value) {
            continue;
        }
        let patch = before.filter(|_| field_deltas).and_then(|before| {
            let old = before.as_object()?;
            let new = value.as_object()?;
            if old.get("id") != new.get("id") || old.get("kind") != new.get("kind") {
                return None;
            }
            let fields = new
                .iter()
                .filter(|(key, value)| old.get(*key) != Some(*value))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<serde_json::Map<_, _>>();
            let removed_fields = old
                .keys()
                .filter(|key| !new.contains_key(*key))
                .cloned()
                .collect::<Vec<_>>();
            if fields.len() > 128 || removed_fields.len() > 128 {
                return None;
            }
            Some(json!({"id":id,"fields":fields,"removed_fields":removed_fields}))
        });
        if let Some(patch) = patch {
            patches.push(patch);
        } else {
            upserts.push(value.clone());
        }
    }
    (upserts, patches)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn optional_fields_preserve_null_removal_new_rows_and_kind_changes() {
        let previous = BTreeMap::from([
            (
                "agent/a".into(),
                json!({"id":"agent/a","kind":"agent","name":"A","nullable":"before","removed":1}),
            ),
            ("agent/b".into(), json!({"id":"agent/b","kind":"agent"})),
        ]);
        let current = BTreeMap::from([
            (
                "agent/a".into(),
                json!({"id":"agent/a","kind":"agent","name":"A","nullable":null}),
            ),
            (
                "agent/b".into(),
                json!({"id":"agent/b","kind":"unknown-agent"}),
            ),
            ("agent/c".into(), json!({"id":"agent/c","kind":"agent"})),
        ]);
        let (upserts, patches) = changed_rows(&previous, &current, true);
        assert_eq!(
            upserts,
            vec![current["agent/b"].clone(), current["agent/c"].clone()]
        );
        assert_eq!(
            patches,
            json!([{"id":"agent/a","fields":{"nullable":null},"removed_fields":["removed"]}])
                .as_array()
                .unwrap()
                .clone()
        );
        assert_eq!(changed_rows(&current, &current, true), (vec![], vec![]));
        let (legacy, patches) = changed_rows(&previous, &current, false);
        assert_eq!(legacy, current.values().cloned().collect::<Vec<_>>());
        assert!(patches.is_empty());
    }
    #[test]
    fn changing_one_status_does_not_resend_large_unchanged_fields() {
        let before = json!({"id":"agent/a","kind":"agent","state":"working","transcript":"x".repeat(120_000)});
        let mut after = before.clone();
        after["state"] = json!("idle");
        let (upserts, patches) = changed_rows(
            &BTreeMap::from([("agent/a".into(), before)]),
            &BTreeMap::from([("agent/a".into(), after.clone())]),
            true,
        );
        assert!(upserts.is_empty());
        assert_eq!(
            patches,
            json!([{"id":"agent/a","fields":{"state":"idle"},"removed_fields":[]}])
                .as_array()
                .unwrap()
                .clone()
        );
        assert!(serde_json::to_vec(&patches).unwrap().len() < 128);
        assert!(serde_json::to_vec(&after).unwrap().len() > 120_000);
    }
}

#[cfg(test)]
mod socket_tests {
    use super::super::*;
    use std::sync::Mutex;
    use tokio_tungstenite::tungstenite::Message;
    type Socket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn frame(socket: &mut Socket) -> Value {
        loop {
            let item = tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            match item {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                Message::Ping(data) => socket.send(Message::Pong(data)).await.unwrap(),
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }
    fn commit(state: &AppState, sequence: u64) {
        state
            .store
            .append_claim(&ClaimInput {
                subject: format!("glass/person/ada/019a0000-0000-7000-8000-{sequence:012x}"),
                kind: "glass.upserted".into(),
                actor: Some("person/ada".into()),
                fields: serde_json::from_value(json!({"body":{"name":format!("sequence/{sequence}"),"layout":{"tabs":[{"pane":"fixture"}]}},"base_revision":null})).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        signal_visible_change(state);
    }
    #[tokio::test]
    async fn collection_field_patches_socket_preserves_legacy_reorder_remove_silence_and_reconnect()
    {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::test_state_named(root.path(), "field-patch-control");
        let writer = state.clone();
        let a = json!({"id":"agent/a","kind":"future-resource","revision":"r1","updated_at":"now","state":"working","nullable":1,"removed":true,"huge":"x".repeat(120_000)});
        let b = json!({"id":"agent/b","kind":"future-resource","revision":"r1","updated_at":"now","state":"working"});
        let rows = Arc::new(Mutex::new(vec![a.clone(), b.clone()]));
        let live_rows = rows.clone();
        let app = axum::Router::new().route(
            "/stream",
            axum::routing::get(move |upgrade: WebSocketUpgrade| {
                let (state, rows) = (state.clone(), live_rows.clone());
                async move {
                    upgrade.on_upgrade(move |socket| {
                        collection_stream_socket_with_reader(
                            socket,
                            state,
                            ClientSession::local(None).unwrap(),
                            None,
                            move |state, _, _, permit| {
                                let rows = rows.clone();
                                async move {
                                    let _permit = permit;
                                    Ok((
                                        new_client_snapshot(&state),
                                        rows.lock().unwrap().clone(),
                                        false,
                                    ))
                                }
                            },
                        )
                    })
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/stream"))
            .await
            .unwrap();
        for (id, compact) in [("fields", true), ("legacy", false)] {
            socket.send(Message::Text(json!({"kind":"subscribe","id":id,"collection":"agents","field_deltas":compact}).to_string().into())).await.unwrap();
            let initial = frame(&mut socket).await;
            assert_eq!(initial["kind"], "snapshot");
            assert_eq!(initial["items"], json!([a, b]));
            assert!(initial.get("patches").is_none());
        }
        let mut changed = a.clone();
        changed["state"] = json!("idle");
        changed["nullable"] = Value::Null;
        changed.as_object_mut().unwrap().remove("removed");
        let c = json!({"id":"agent/c","kind":"future-resource","revision":"r1","updated_at":"now","state":"new"});
        *rows.lock().unwrap() = vec![b.clone(), changed.clone(), c.clone()];
        commit(&writer, 1);
        let mut changes = BTreeMap::new();
        for _ in 0..2 {
            let next = frame(&mut socket).await;
            changes.insert(next["id"].as_str().unwrap().to_string(), next);
        }
        assert_eq!(
            changes["fields"]["patches"],
            json!([{"id":"agent/a","fields":{"state":"idle","nullable":null},"removed_fields":["removed"]}])
        );
        assert_eq!(changes["fields"]["upserts"], json!([c]));
        assert_eq!(
            changes["fields"]["order"],
            json!(["agent/b", "agent/a", "agent/c"])
        );
        assert!(changes["legacy"].get("patches").is_none());
        assert_eq!(changes["legacy"]["upserts"], json!([changed, c]));
        assert!(serde_json::to_vec(&changes["fields"]).unwrap().len() < 1500);
        assert!(serde_json::to_vec(&changes["legacy"]).unwrap().len() > 120_000);
        *rows.lock().unwrap() = vec![changed.clone(), c.clone()];
        commit(&writer, 2);
        for _ in 0..2 {
            let next = frame(&mut socket).await;
            assert_eq!(next["removes"], json!(["agent/b"]));
            assert_eq!(next["upserts"], json!([]));
            assert!(next.get("patches").is_none());
        }
        commit(&writer, 3);
        assert!(
            tokio::time::timeout(Duration::from_millis(1700), socket.next())
                .await
                .is_err()
        );
        socket.close(None).await.unwrap();
        let (mut reopened, _) = tokio_tungstenite::connect_async(format!("ws://{address}/stream"))
            .await
            .unwrap();
        reopened
            .send(Message::Text(
                json!({"kind":"subscribe","id":"fields","collection":"agents","field_deltas":true})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let initial = frame(&mut reopened).await;
        assert_eq!(initial["kind"], "snapshot");
        assert_eq!(initial["items"], json!([changed, c]));
        assert!(initial.get("patches").is_none());
        reopened.close(None).await.unwrap();
        server.abort();
    }
}
