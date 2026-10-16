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

    struct AnnotatedPage {
        state: AppState,
        fence: crate::mailbox::Fence,
        source: crate::external_sessions::ExternalSession,
        session: ClientSession,
        route: String,
    }

    impl AnnotatedPage {
        fn new(root: &std::path::Path, name: &str) -> Self {
            let state = super::super::tests::test_state_named(root, name);
            let owner = "agent/eval.worker";
            let native = "00000000-0000-0000-0000-000000000002";
            crate::mailbox::tests::ready(&state.store, "session-1");
            let fence = state
                .store
                .bind_mailbox(&crate::mailbox::Fence::new(owner, "session-1", "delivery"))
                .unwrap();
            let mut session = ClientSession::local(Some("person/fixture")).unwrap();
            session.conversation_delivery_links = true;
            Self {
                state,
                fence,
                session,
                route: super::super::super::managed_session_id(owner, "session-1"),
                source: crate::external_sessions::ExternalSession {
                    id: crate::external_sessions::external_session_id(
                        crate::external_sessions::ExternalDriver::Claude,
                        native,
                    ),
                    revision: "fixture".into(),
                    driver: crate::external_sessions::ExternalDriver::Claude,
                    native_id: native.into(),
                    transcript: root.join("composition.jsonl"),
                    codex_home: None,
                    cwd: None,
                    title: None,
                    started_at_unix_ms: 0,
                    updated_at_unix_ms: 0,
                    process: None,
                },
            }
        }

        fn receipt(&self, subject: &str, native_id: &str) {
            let input = |kind: &str, fields: Value| ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: Some(
                    if kind == "message.sent" {
                        "person/fixture"
                    } else {
                        "agent/eval.worker"
                    }
                    .into(),
                ),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some(format!("composition:{subject}:{kind}")),
            };
            self.state
                .store
                .append_claim(&input(
                    "message.sent",
                    json!({
                        "status":"sent", "from":"person/fixture", "to":"agent/eval.worker",
                        "session_id":self.route, "content":"Invented composition mail",
                    }),
                ))
                .unwrap();
            for status in ["staged", "delivered"] {
                self.state
                    .store
                    .append_mailbox_receipt_outcome(
                        &input(&format!("message.{status}"), json!({"status":status})),
                        &self.fence,
                    )
                    .unwrap();
            }
            self.state
                .store
                .append_native_mailbox_receipt_outcome(
                    &input("message.read", json!({"status":"read"})),
                    &self.fence,
                    Some(&crate::conversation_delivery::NativeReceipt {
                        session_id: self.source.id.clone(),
                        message_id: native_id.into(),
                    }),
                )
                .unwrap();
        }

        fn transcript(&self, texts: &[String]) {
            // Native sequence numbers include each record's byte offset. Keep those
            // offsets fixed when calibrating content, or longer IDs change the overhead.
            const RECORD_BYTES: usize = 16 * 1024;
            let lines = texts
                .iter()
                .enumerate()
                .map(|(index, text)| {
                    let mut line = json!({
                        "type":"user", "uuid":format!("native-{index:03}"),
                        "sessionId":self.source.native_id,
                        "timestamp":format!("2099-01-01T00:{:02}:{:02}Z", index / 60, index % 60),
                        "message":{"role":"user", "content":text},
                    })
                    .to_string();
                    assert!(line.len() < RECORD_BYTES);
                    line.extend(std::iter::repeat_n(' ', RECORD_BYTES - 1 - line.len()));
                    line.push('\n');
                    line
                })
                .collect::<String>();
            std::fs::write(&self.source.transcript, lines).unwrap();
        }

        fn raw(&self) -> Vec<Value> {
            conversation_blocks::read_slice(
                &self.source,
                &self.session,
                &self.route,
                crate::external_sessions::TimelineOrder::TimestampSequence,
                None,
                CLIENT_MAX_PAGE_ITEMS,
            )
            .unwrap()
            .items
        }
    }

    #[test]
    fn native_page_suppresses_a_receipted_echo_before_annotation_without_a_receipt_lookup() {
        let root = tempfile::tempdir().unwrap();
        let fixture = AnnotatedPage::new(root.path(), "delivery-suppression-order");
        fixture.receipt("message/suppressed-echo", "native-000");
        fixture.transcript(&["[st3-delivery:message/suppressed-echo]\nInvented mail".into()]);
        let snapshot = new_client_snapshot(&fixture.state);
        // Positive precondition: the unsuppressed native entry really has a qualified link.
        let mut raw = fixture.raw();
        assert!(
            raw.iter()
                .any(|item| item.get("_delivery_correlation").is_some())
        );
        annotate(
            &fixture.state,
            &snapshot,
            &fixture.session,
            &fixture.source.id,
            Some("agent/eval.worker"),
            raw.iter_mut(),
        )
        .unwrap();
        assert!(
            raw.iter()
                .any(|item| item["delivers"] == "message/suppressed-echo")
        );
        // Order sentinel, confined to this isolated fixture: any receipt SQL now fails.
        // A successful real page therefore proves the suppressed echo consumes no lookup slot.
        fixture
            .state
            .store
            .connection
            .write()
            .execute_batch("DROP TABLE local_native_delivery_receipts_v2;")
            .unwrap();
        let (page, _, suppressed) = native_slice_page_at_generation(
            &fixture.state,
            &snapshot,
            &fixture.session,
            &fixture.route,
            &ClientListQuery {
                limit: Some(10),
                ..Default::default()
            },
            &fixture.source,
        )
        .unwrap();
        let items = page.0["items"].as_array().unwrap().clone();
        assert!(suppressed > 0);
        assert!(!items.is_empty());
        assert!(
            items
                .iter()
                .any(|item| item["body"]["message_id"] == "message/suppressed-echo")
        );
        assert!(items.iter().all(|item| {
            item["body"]["message_id"] != "native-000"
                && item.get("delivers").is_none()
                && item.get("_delivery_correlation").is_none()
        }));
    }

    #[test]
    fn native_page_byte_bound_counts_annotations_and_continues_to_the_trimmed_link() {
        let root = tempfile::tempdir().unwrap();
        let fixture = AnnotatedPage::new(root.path(), "delivery-byte-bound-order");
        fixture.receipt("message/byte-oldest", "native-000");
        fixture.receipt("message/byte-newest", "native-099");
        let size = |items: &[Value]| {
            items
                .iter()
                .map(|item| serde_json::to_vec(item).unwrap().len())
                .sum::<usize>()
        };
        // One hundred native turns give exactly 200 entries, newer than the graph rows.
        // Calibrate ordinary text below its per-value cap so the unannotated page fits
        // by one byte: moving annotation after the real byte bound must fail this control.
        fixture.transcript(&vec!["x".into(); 100]);
        let target = TIMELINE_PAGE_BYTE_BUDGET - 1;
        let overhead = fixture.raw();
        assert_eq!(overhead.len(), CLIENT_MAX_PAGE_ITEMS);
        let padding = target.checked_sub(size(&overhead)).unwrap();
        let texts = (0..100)
            .map(|index| {
                let length = 1 + padding / 100 + usize::from(index < padding % 100);
                assert!(length < 8 * 1024);
                "x".repeat(length)
            })
            .collect::<Vec<_>>();
        fixture.transcript(&texts);
        let mut annotated = fixture.raw();
        assert_eq!(size(&annotated), target);
        let snapshot = new_client_snapshot(&fixture.state);
        annotate(
            &fixture.state,
            &snapshot,
            &fixture.session,
            &fixture.source.id,
            Some("agent/eval.worker"),
            annotated.iter_mut(),
        )
        .unwrap();
        assert_eq!(
            annotated
                .iter()
                .filter(|item| item.get("delivers").is_some())
                .count(),
            2
        );
        assert!(size(&annotated) > TIMELINE_PAGE_BYTE_BUDGET);
        let query = ClientListQuery {
            limit: Some(CLIENT_MAX_PAGE_ITEMS),
            ..Default::default()
        };
        let page = native_slice_page(
            &fixture.state,
            &snapshot,
            &fixture.session,
            &fixture.route,
            &query,
            &fixture.source,
        )
        .unwrap()
        .0;
        let items = page["items"].as_array().unwrap();
        assert!(size(items) <= TIMELINE_PAGE_BYTE_BUDGET);
        assert!(
            items.len() < annotated.len(),
            "annotation bytes must cause trimming"
        );
        assert!(
            items
                .iter()
                .any(|item| item["delivers"] == "message/byte-newest")
        );
        assert!(
            items
                .iter()
                .all(|item| item["body"]["message_id"] != "native-000"
                    && item["delivers"] != "message/byte-oldest")
        );
        assert_eq!(page["page"]["has_more"], true);
        let next = native_slice_page(
            &fixture.state,
            &snapshot,
            &fixture.session,
            &fixture.route,
            &ClientListQuery {
                cursor: Some(page["page"]["next_cursor"].as_str().unwrap().into()),
                ..query
            },
            &fixture.source,
        )
        .unwrap()
        .0;
        let continued = next["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["body"]["message_id"] == "native-000")
            .unwrap();
        assert_eq!(continued["delivers"], "message/byte-oldest");
        assert_eq!(next["page"]["has_more"], false);
    }

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
