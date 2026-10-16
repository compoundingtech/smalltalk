use super::tests::{json_request, state};
use super::*;

async fn owner_get(app: Router, path: &str, person: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .uri(path)
                .header(client_v0::LOCAL_PERSON_HEADER, person)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    (status, body.get("value").cloned().unwrap_or(body))
}

async fn retained_owner_item(
    app: Router,
    episode: &str,
    original: &str,
    attribution: &[&str],
) -> Value {
    let (status, history) = owner_get(
        app.clone(),
        "/v1/client/attention?history=true",
        "person/avery",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{history}");
    let sdk_page: st3_client::Page = serde_json::from_value(history.clone()).unwrap();
    assert_eq!(sdk_page.page.omitted_items, Some(0));
    let item = history["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["episode"] == episode)
        .unwrap()
        .clone();
    assert_eq!(item["state"], "resolved");
    assert_eq!(item["actions"], json!([]));
    assert_eq!(item["operational"]["actionable"], false);
    let detail = item["detail"].as_str().unwrap();
    assert!(detail.contains(original), "{detail}");
    for expected in attribution {
        assert!(detail.contains(expected), "missing {expected}: {detail}");
    }
    let path = format!(
        "/v1/client/attention/{}",
        urlencoding::encode(item["id"].as_str().unwrap())
    );
    let (status, selected) = owner_get(app.clone(), &path, "person/avery").await;
    assert_eq!(status, StatusCode::OK, "{selected}");
    assert_eq!(selected, item);
    assert_eq!(
        owner_get(app.clone(), &path, "person/other").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        owner_get(
            app.clone(),
            "/v1/client/attention?history=true&person=person%2Favery",
            "person/other"
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, open) = owner_get(app, "/v1/client/attention", "person/avery").await;
    assert_eq!(status, StatusCode::OK, "{open}");
    assert!(
        open["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["episode"] != episode)
    );
    item
}

#[tokio::test]
async fn alerts_resolve_dismisses_only_the_exact_granted_denial_source() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let (denial, proof) =
        crate::store::alert_action_controls::seed_denial(&state.store, &["close-item"]);
    let app = router(state.clone());
    let card = client_attention_id(&denial.subject, &proof.person, &denial.id).unwrap();
    let path = format!("/v1/attention/resolve/{}", urlencoding::encode(&card));
    let body = json!({"actor":"agent/alder.helper","outcome":"dismissed","delegation":proof,"idempotency_key":"api-dismiss-source"});
    let mut wrong = body.clone();
    wrong["outcome"] = json!("resolved");
    assert_ne!(
        json_request(app.clone(), &path, wrong).await.0,
        StatusCode::OK
    );
    assert_eq!(state.store.prompt_answer(&denial.id).unwrap().answer, None);
    let (status, result) = json_request(app.clone(), &path, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["outcome"], "dismissed");
    assert_eq!(result["actor"], "agent/alder.helper");
    assert_eq!(state.store.prompt_answer(&denial.id).unwrap().answer.as_deref(), Some("dismiss"));
    let answers = state.store.claims_for(&denial.subject, Some("harness.diagnostic")).unwrap()
        .into_iter().filter(|claim| claim.body["fields"]["code"] == "native-prompt-answered")
        .collect::<Vec<_>>();
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].body["_operation"]["id"], smallclaims::store::operation_id_for_key(
        &format!("native-prompt-answer:{}", denial.id)));
    let committed_index = state.store.index().unwrap();
    let (status, repeated) = json_request(app.clone(), &path, body).await;
    assert_eq!(status, StatusCode::OK, "{repeated}");
    assert_eq!(state.store.index().unwrap(), committed_index, "an identical retry appends nothing");
    assert_eq!(
        state
            .store
            .messages(Some("agent/alder.worker"), true)
            .unwrap()
            .len(),
        1
    );
    let index = state.store.index().unwrap();
    retained_owner_item(
        app.clone(),
        &denial.id,
        "invented denied call",
        &[
            "Dismissed by agent/alder.helper for person/avery",
            &proof.quote,
            &proof.policy,
            &denial.id,
        ],
    )
    .await;
    assert_eq!(
        state.store.index().unwrap(),
        index,
        "history reads must not write"
    );
    let source =
        "version 2\nagent \"worker\" { workspace \"/tmp/other\"; harness \"claude\" {} }\n";
    let intent = crate::graph::parse_internal_intent(source, "alder").unwrap();
    let preview = state
        .store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply_as(
            &intent,
            &preview.subject_tokens,
            "history-owner-change",
            Some("person/other"),
        )
        .unwrap();
    assert_eq!(
        state
            .store
            .agent_person(&denial.subject)
            .unwrap()
            .as_deref(),
        Some("person/other")
    );
    let (status, history) = owner_get(
        app.clone(),
        "/v1/client/attention?history=true",
        "person/avery",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert!(
        history["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["episode"] == denial.id)
    );
    let (status, foreign) =
        owner_get(app, "/v1/client/attention?history=true", "person/other").await;
    assert_eq!(status, StatusCode::OK, "{foreign}");
    assert!(
        foreign["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["episode"] != denial.id)
    );
}

#[tokio::test]
async fn alerts_withdraw_resolves_its_own_update_source_and_refuses_other_sources() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let source = "version 2\nagent \"alder.helper\" { workspace \"/tmp\"; command \"true\" }\n";
    let intent = crate::graph::parse_internal_intent(source, "node").unwrap();
    state
        .store
        .apply_internal(&intent, "declared-helper")
        .unwrap();
    state.store.append_claim(&ClaimInput {subject:"message/update-instruction".into(),kind:"message.sent".into(),actor:Some("person/avery".into()),
        fields:serde_json::from_value(json!({"from":"person/avery","to":"agent/alder.helper","content":"Report the result","status":"sent"})).unwrap(),
        evidence:vec![],expected_subject:None,idempotency_key:Some("update-instruction".into())}).unwrap();
    let app = router(state.clone());
    let (status, update) = json_request(
        app.clone(),
        "/v1/work/ask",
        json!({"person":"person/avery","actor":"agent/alder.helper",
        "title":"Invented status","reason":"The report is obsolete","idempotency_key":"own-update",
        "request":{"version":1,"type":"update","about":"message/update-instruction"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{update}");
    let subject = update["subject"].as_str().unwrap();
    let episode = state
        .store
        .claims_for(subject, Some("work.person-asked"))
        .unwrap()[0]
        .id
        .clone();
    let card = client_attention_id(subject, "person/avery", &episode).unwrap();
    let path = format!("/v1/attention/withdraw/{}", urlencoding::encode(&card));
    let mut body = json!({"actor":"agent/alder.stranger","episode":episode,"reason":"Withdrawn","idempotency_key":"withdraw-own"});
    assert_ne!(
        json_request(app.clone(), &path, body.clone()).await.0,
        StatusCode::OK
    );
    body["actor"] = json!("agent/alder.helper");
    let (status, result) = json_request(app.clone(), &path, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["status"], "cancelled");
    assert_eq!(
        json_request(app.clone(), &path, body).await.0,
        StatusCode::OK
    );
    assert!(
        state
            .store
            .attention_items(Some("person/avery"))
            .unwrap()
            .iter()
            .all(|item| item.subject != subject)
    );
    let index = state.store.index().unwrap();
    let item = retained_owner_item(
        app,
        &episode,
        "The report is obsolete",
        &["Withdrawn by the sender agent/alder.helper", "Withdrawn"],
    )
    .await;
    assert_eq!(item["update"]["type"], "update");
    assert_eq!(
        state.store.index().unwrap(),
        index,
        "history reads must not write"
    );
}

#[tokio::test]
async fn attention_history_sparse_pages_keep_the_cut_and_reject_cursor_tampering() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let (denial, proof) =
        crate::store::alert_action_controls::seed_denial(&state.store, &["close-item"]);
    state
        .store
        .respond_native_prompt(
            &denial.subject,
            &denial.id,
            "dismiss",
            "agent/alder.helper",
            Some(&proof),
        )
        .unwrap();
    for number in 0..4 {
        state.store.append_claim(&ClaimInput {subject:denial.subject.clone(),kind:"harness.diagnostic".into(),actor:Some(denial.subject.clone()),
            fields:serde_json::from_value(json!({"code":"history-fixture","reason":format!("irrelevant {number}"),"severity":"warning"})).unwrap(),
            evidence:vec![],expected_subject:None,idempotency_key:Some(format!("irrelevant-{number}"))}).unwrap();
    }
    let app = router(state.clone());
    let (status, mut page) = owner_get(
        app.clone(),
        "/v1/client/attention?history=true&limit=1",
        "person/avery",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"], json!([]));
    assert_eq!(page["page"]["has_more"], true);
    let cursor = page["page"]["next_cursor"].as_str().unwrap().to_owned();
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(
            cursor
                .strip_prefix(attention_history::CURSOR_PREFIX)
                .unwrap(),
        )
        .unwrap();
    let mut forged: Value = serde_json::from_slice(&bytes).unwrap();
    forged["position"]["before_index"] = json!(0);
    let forged = format!(
        "{}{}",
        attention_history::CURSOR_PREFIX,
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&forged).unwrap())
    );
    assert_eq!(
        owner_get(
            app.clone(),
            &format!(
                "/v1/client/attention?history=true&cursor={}",
                urlencoding::encode(&forged)
            ),
            "person/avery"
        )
        .await
        .0,
        StatusCode::GONE
    );
    assert_eq!(
        owner_get(
            app.clone(),
            &format!(
                "/v1/client/attention?history=true&cursor={}",
                urlencoding::encode(&cursor)
            ),
            "person/other"
        )
        .await
        .0,
        StatusCode::GONE
    );
    let mut next_proof = proof.clone();
    let later=state.store.append_claim(&ClaimInput {subject:denial.subject.clone(),kind:"harness.diagnostic".into(),actor:Some(denial.subject.clone()),
        fields:serde_json::from_value(json!({"code":"permission-denied","severity":"warning","status":"Bash","driver":"claude","reason":"a later denied call"})).unwrap(),
        evidence:vec![],expected_subject:None,idempotency_key:Some("later-denial".into())}).unwrap();
    next_proof.episode = later.id.clone();
    state
        .store
        .respond_native_prompt(
            &later.subject,
            &later.id,
            "dismiss",
            "agent/alder.helper",
            Some(&next_proof),
        )
        .unwrap();
    let mut seen = Vec::new();
    for _ in 0..10 {
        seen.extend(page["items"].as_array().unwrap().iter().cloned());
        if !page["page"]["has_more"].as_bool().unwrap() {
            break;
        }
        let next = page["page"]["next_cursor"].as_str().unwrap();
        let (status, next_page) = owner_get(
            app.clone(),
            &format!(
                "/v1/client/attention?history=true&cursor={}",
                urlencoding::encode(next)
            ),
            "person/avery",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{next_page}");
        assert_eq!(next_page["page"]["limit"], 1);
        page = next_page;
    }
    assert_eq!(page["page"]["has_more"], false);
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0]["episode"], denial.id);
    let (status, refresh) =
        owner_get(app, "/v1/client/attention?history=true", "person/avery").await;
    assert_eq!(status, StatusCode::OK, "{refresh}");
    assert!(
        refresh["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["episode"] == later.id)
    );
}

#[tokio::test]
async fn attention_history_filters_foreign_missing_sources_before_reporting_omissions() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let (denial, proof) =
        crate::store::alert_action_controls::seed_denial(&state.store, &["close-item"]);
    let answer = state
        .store
        .respond_native_prompt(
            &denial.subject,
            &denial.id,
            "dismiss",
            "agent/alder.helper",
            Some(&proof),
        )
        .unwrap();
    let app = router(state.clone());
    let (_, original) = owner_get(
        app.clone(),
        "/v1/client/attention?history=true",
        "person/avery",
    )
    .await;
    assert_eq!(original["items"].as_array().unwrap().len(), 1);
    // An authored partial-replica image, not an admitted signed claim: the answer
    // identifies another person but its cited original is absent at this cut.
    let mut body = answer.body.clone();
    body["fields"]["acted_for"] = json!("person/other");
    body["fields"]["delegation"]["person"] = json!("person/other");
    body["fields"]["delegation"]["episode"] = json!("claim/history-missing-original");
    state
        .store
        .connection
        .batched(|tx| -> anyhow::Result<()> {
            tx.execute(
                "UPDATE claims SET body=?1,predecessors=?2 WHERE id=?3",
                rusqlite::params![
                    body.to_string(),
                    json!(["claim/history-missing-original"]).to_string(),
                    answer.id
                ],
            )?;
            Ok(())
        })
        .unwrap()
        .unwrap();
    let index = state.store.index().unwrap();
    let (status, page) = owner_get(
        app.clone(),
        "/v1/client/attention?history=true",
        "person/avery",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["page"]["omitted_items"], 0);
    assert_eq!(page["items"], json!([]));
    let (status, own_missing) = owner_get(
        app.clone(),
        "/v1/client/attention?history=true",
        "person/other",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{own_missing}");
    assert_eq!(own_missing["page"]["omitted_items"], 1);
    let path = format!(
        "/v1/client/attention/{}",
        urlencoding::encode(&format!("attention/closed/{}", answer.id))
    );
    assert_eq!(
        owner_get(app.clone(), &path, "person/avery").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        owner_get(app, &path, "person/other").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(state.store.index().unwrap(), index);
}

#[tokio::test]
async fn attention_history_omits_wide_and_malformed_rows_and_advances_to_retained_items() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let (denial, proof) =
        crate::store::alert_action_controls::seed_denial(&state.store, &["close-item"]);
    state
        .store
        .respond_native_prompt(
            &denial.subject,
            &denial.id,
            "dismiss",
            "agent/alder.helper",
            Some(&proof),
        )
        .unwrap();
    let filler = state
        .store
        .append_claim(&ClaimInput {
            subject: denial.subject.clone(),
            kind: "harness.diagnostic".into(),
            actor: Some(denial.subject.clone()),
            fields: serde_json::from_value(json!({"code":"history-fixture","severity":"warning"}))
                .unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some("omitted-history-candidate".into()),
        })
        .unwrap();
    let app = router(state.clone());
    let before_refusal = state.store.index().unwrap();
    state
        .store
        .connection
        .batched(|tx| -> anyhow::Result<()> {
            let row = || -> rusqlite::Result<(String, String, i64)> {
                tx.query_row(
                    "SELECT body,predecessors,store_index FROM claims WHERE id=?1",
                    [&filler.id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
            };
            let indexed = || -> rusqlite::Result<String> {
                tx.query_row(
                    "SELECT id FROM claims INDEXED BY claims_operation_index
                     WHERE json_extract(body, '$._operation.id') IS NOT NULL
                       AND json_extract(body, '$._operation.id')=?1",
                    [filler.body["_operation"]["id"].as_str().unwrap()],
                    |row| row.get(0),
                )
            };
            let original = row()?;
            assert_eq!(indexed()?, filler.id);
            // The native expression index rejects this state. Never drop it to
            // claim malformed-BODY admission or malformed-JSON history coverage.
            let error = tx
                .execute("UPDATE claims SET body='{malformed' WHERE id=?1", [&filler.id])
                .unwrap_err();
            assert!(error.to_string().contains("malformed JSON"), "{error}");
            assert_eq!(row()?, original);
            assert_eq!(indexed()?, filler.id);
            Ok(())
        })
        .unwrap()
        .unwrap();
    assert_eq!(state.store.index().unwrap(), before_refusal);
    let (status, baseline) = owner_get(
        app.clone(),
        "/v1/client/attention?history=true&limit=32",
        "person/avery",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{baseline}");
    assert_eq!(baseline["page"]["has_more"], false);
    let expected = baseline["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].clone())
        .collect::<Vec<_>>();
    assert!(!expected.is_empty());
    for (body, predecessors) in [
        (json!({"fields":{"reason":"x".repeat(1_048_577)}}).to_string(), "[]".to_owned()),
        // Valid JSON reaches storage, but predecessors must be an array. This
        // exercises malformed-row omission, not malformed-BODY admission.
        (json!({"fields":{"code":"native-prompt-answered","status":"dismiss",
            "acted_for":"person/avery","driver":"claude"}}).to_string(), "{}".to_owned()),
    ] {
        // Private SQL corruption fixture, never a source claim or a live database.
        state
            .store
            .connection
            .batched(|tx| -> anyhow::Result<()> {
                tx.execute(
                    "UPDATE claims SET body=?1,predecessors=?2 WHERE id=?3",
                    rusqlite::params![body, predecessors, filler.id],
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        let index = state.store.index().unwrap();
        let (status, mut page) = owner_get(
            app.clone(),
            "/v1/client/attention?history=true&limit=1",
            "person/avery",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["page"]["omitted_items"], 1);
        let sdk_page: st3_client::Page = serde_json::from_value(page.clone()).unwrap();
        assert_eq!(sdk_page.page.omitted_items, Some(1));
        assert_eq!(page["items"], json!([]));
        assert_eq!(page["page"]["has_more"], true);
        let mut found = false;
        let mut retained = Vec::new();
        for _ in 0..10 {
            if !page["page"]["has_more"].as_bool().unwrap() {
                break;
            }
            let cursor = page["page"]["next_cursor"].as_str().unwrap();
            let (status, next) = owner_get(
                app.clone(),
                &format!(
                    "/v1/client/attention?history=true&cursor={}",
                    urlencoding::encode(cursor)
                ),
                "person/avery",
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{next}");
            assert_eq!(next["page"]["omitted_items"], 0);
            found |= next["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["episode"] == denial.id);
            retained.extend(
                next["items"].as_array().unwrap().iter().map(|item| item["id"].clone()),
            );
            page = next;
        }
        assert!(
            found,
            "readable retained history remains reachable after the omitted row"
        );
        assert_eq!(page["page"]["has_more"], false);
        assert_eq!(retained, expected, "retained neighbours keep their original positions");
        assert_eq!(state.store.index().unwrap(), index);
    }
}
