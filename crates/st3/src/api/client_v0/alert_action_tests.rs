//! Shared dispatcher controls, using authored sessions rather than real device grants.
use super::*;

#[tokio::test]
async fn delegated_prompt_actions_share_one_source_contract_and_cannot_choose_the_actor() {
    for client in ["web", "tui", "ios"] {
        let root = tempfile::tempdir().unwrap();
        let state = tests::test_state_named(root.path(), "alder");
        let (denial, proof) =
            crate::store::alert_action_controls::seed_denial(&state.store, &["close-item"]);
        let snapshot = new_client_snapshot(&state);
        let session = ClientSession::for_tests_scoped(
            &format!("client/{client}-authored-fixture"),
            "agent/alder.helper",
            &["control.attention"],
        );
        let parameters = json!({"target_id":"agent/alder.worker","episode":denial.id,"answer":"dismiss","delegation":proof});
        let typed: st3_client::PromptRespondParameters =
            serde_json::from_value(parameters.clone()).unwrap();
        assert_eq!(serde_json::to_value(typed).unwrap(), parameters);
        let request = ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: format!("action/{client}-dismiss"),
            action_type: "prompt.respond".into(),
            idempotency_key: format!("authored-{client}-delegated-dismiss"),
            fence: Fence {
                snapshot_id: snapshot.id.clone(),
                ..Default::default()
            },
            parameters,
        };
        // Main's ordinary answers remain text; supplying proof narrows the
        // source action to dismiss rather than granting those ordinary answers.
        let before = state.store.index().unwrap();
        for answer in ["allow", "approve", "use-all", r#"{"0":[1]}"#] {
            let mut refused = request.clone();
            refused.idempotency_key = format!("authored-{client}-refused-{answer}");
            refused.parameters["answer"] = json!(answer);
            assert_eq!(
                action_inner(
                    State(state.clone()),
                    Extension(snapshot.clone()),
                    Extension(session.clone()),
                    Json(refused),
                    None
                )
                .await
                .unwrap_err()
                .code,
                "delegation-refused"
            );
            assert_eq!(state.store.index().unwrap(), before);
        }
        let mut forged = request.clone();
        forged.parameters["actor"] = json!("person/avery");
        assert_eq!(
            action_inner(
                State(state.clone()),
                Extension(snapshot.clone()),
                Extension(session.clone()),
                Json(forged),
                None
            )
            .await
            .unwrap_err()
            .code,
            "validation-failed"
        );
        let Json(first) = action_inner(
            State(state.clone()),
            Extension(snapshot.clone()),
            Extension(session.clone()),
            Json(request.clone()),
            None,
        )
        .await
        .unwrap();
        let Json(again) = action_inner(
            State(state.clone()),
            Extension(snapshot),
            Extension(session),
            Json(request),
            None,
        )
        .await
        .unwrap();
        assert_eq!(first["affected_ids"], again["affected_ids"]);
        assert_eq!(first["affected_ids"], json!(["agent/alder.worker"]));
        let answer = state
            .store
            .claims_for("agent/alder.worker", Some("harness.diagnostic"))
            .unwrap()
            .into_iter()
            .find(|claim| claim.body["fields"]["code"] == "native-prompt-answered")
            .unwrap();
        assert_eq!(answer.actor.as_deref(), Some("agent/alder.helper"));
        assert_eq!(answer.body["fields"]["acted_for"], "person/avery");
        assert_eq!(
            state
                .store
                .messages(Some("agent/alder.worker"), true)
                .unwrap()
                .len(),
            1
        );
    }
}
