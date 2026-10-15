//! Optional receipt links on selected native user entries, never reconstructed from text.
use super::*;

pub(super) fn annotate<'a>(
    state: &AppState,
    snapshot: &ClientSnapshot,
    session: &ClientSession,
    session_id: &str,
    owner: Option<&str>,
    items: impl Iterator<Item = &'a mut Value>,
) -> Result<(), ApiError> {
    let Some(owner) = owner.filter(|_| session.conversation_delivery_links) else {
        return Ok(());
    };
    let mut items = items.take(CLIENT_MAX_PAGE_ITEMS + 1).collect::<Vec<_>>();
    if items.len() > CLIENT_MAX_PAGE_ITEMS {
        return Ok(());
    }
    let eligible = |item: &Value| {
        item["type"] == "message"
            && item["role"] == "user"
            && item["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("timeline-entry/native-"))
    };
    let ids: BTreeSet<String> = items
        .iter()
        .filter(|item| eligible(item))
        .filter_map(|item| {
            item["body"]["message_id"]
                .as_str()
                .filter(|id| id.len() <= 256)
                .map(str::to_owned)
        })
        .collect();
    let receipts = state
        .store
        .conversation_delivery_receipts(session_id, owner, &ids, snapshot.store_index)
        .map_err(ApiError::internal)?;
    let mut links = crate::conversation_delivery::unique_links(session_id, receipts);
    let mut seen = BTreeSet::new();
    for item in items.iter().filter(|item| eligible(item)) {
        if let Some(id) = item["body"]["message_id"].as_str()
            && !seen.insert(id)
        {
            links.remove(id);
        }
    }
    for item in &mut items {
        if eligible(item)
            && let Some(link) = item["body"]["message_id"]
                .as_str()
                .and_then(|id| links.get(id))
        {
            item["delivers"] = json!(link);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_delivery_annotation_requires_negotiation_and_exact_receipt_identity() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::test_state_named(root.path(), "delivery-annotation");
        let owner = "agent/eval.worker";
        let native_session = "00000000-0000-0000-0000-000000000001";
        let session_id = crate::external_sessions::external_session_id(
            crate::external_sessions::ExternalDriver::Claude,
            native_session,
        );
        crate::mailbox::tests::ready(&state.store, "session-1");
        let fence = state
            .store
            .bind_mailbox(&crate::mailbox::Fence::new(owner, "session-1", "delivery"))
            .unwrap();
        let input = |kind: &str, fields: Value| ClaimInput {
            subject: "message/linked".into(),
            kind: kind.into(),
            actor: Some(
                if kind == "message.sent" {
                    "person/fixture"
                } else {
                    owner
                }
                .into(),
            ),
            fields: serde_json::from_value(fields).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some(format!("annotation:{kind}")),
        };
        state.store.append_claim(&input("message.sent", json!({"status":"sent","from":"person/fixture","to":owner,"content":"Invented mail"}))).unwrap();
        for status in ["staged", "delivered"] {
            state
                .store
                .append_mailbox_receipt_outcome(
                    &input(&format!("message.{status}"), json!({"status":status})),
                    &fence,
                )
                .unwrap();
        }
        state
            .store
            .append_native_mailbox_receipt_outcome(
                &input("message.read", json!({"status":"read"})),
                &fence,
                Some(&crate::conversation_delivery::NativeReceipt {
                    session_id: session_id.clone(),
                    message_id: "native-qualified".into(),
                }),
            )
            .unwrap();
        let snapshot = new_client_snapshot(&state);
        let mut session = ClientSession::local(Some("person/fixture")).unwrap();
        let base = json!({"id":"timeline-entry/native-1","sequence":1,"revision":1,
            "timestamp":"2026-10-14T00:00:00Z","role":"user","type":"message","final":true,
            "body":{"message_id":"native-qualified","reply_to":null}});
        let mut items = vec![base.clone()];
        annotate(
            &state,
            &snapshot,
            &session,
            &session_id,
            Some(owner),
            items.iter_mut(),
        )
        .unwrap();
        assert_eq!(items[0], base);
        session.conversation_delivery_links = true;
        annotate(
            &state,
            &snapshot,
            &session,
            &session_id,
            Some(owner),
            items.iter_mut(),
        )
        .unwrap();
        assert_eq!(items[0]["delivers"], "message/linked");
        assert_eq!(items[0]["body"], base["body"]);
        // Exercise the real page path: its managed route is not the native source id.
        let route = super::super::super::managed_session_id(owner, "session-1");
        assert_ne!(route, session_id);
        let path = root.path().join("qualified-native.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n",
                json!({"type":"user", "uuid":"native-qualified",
            "sessionId":native_session, "timestamp":"2026-10-14T00:00:00Z",
            "message":{"role":"user","content":"Invented native delivery"}})
            ),
        )
        .unwrap();
        let source = crate::external_sessions::ExternalSession {
            id: session_id.clone(),
            revision: "fixture".into(),
            driver: crate::external_sessions::ExternalDriver::Claude,
            native_id: native_session.into(),
            transcript: path,
            codex_home: None,
            cwd: None,
            title: None,
            started_at_unix_ms: 0,
            updated_at_unix_ms: 0,
            process: None,
        };
        let page = native_slice_page(
            &state,
            &snapshot,
            &session,
            &route,
            &ClientListQuery {
                limit: Some(10),
                ..Default::default()
            },
            &source,
        )
        .unwrap()
        .0;
        let native = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| {
                item["type"] == "message" && item["body"]["message_id"] == "native-qualified"
            })
            .unwrap();
        assert_eq!(native["delivers"], "message/linked");
        assert_eq!(page["session_id"], route);

        let mut unmatched = [base.clone()];
        unmatched[0]["body"]["message_id"] = json!("native-without-receipt");
        annotate(
            &state,
            &snapshot,
            &session,
            &session_id,
            Some(owner),
            unmatched.iter_mut(),
        )
        .unwrap();
        assert!(unmatched[0].get("delivers").is_none());
        let mut duplicated = [base.clone(), base.clone()];
        annotate(
            &state,
            &snapshot,
            &session,
            &session_id,
            Some(owner),
            duplicated.iter_mut(),
        )
        .unwrap();
        assert!(duplicated.iter().all(|item| item.get("delivers").is_none()));
        session.conversation_delivery_links = false;
        let mut page = json!({"items":items});
        conversation_blocks::legacy(&mut page, &session);
        assert_eq!(page["items"][0], base);
    }
}
