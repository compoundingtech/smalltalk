//! Atomic, durable workflow-failure baseline and message delivery within resource observations.
use super::*;
use crate::resource::github_workflows::PerformanceFailure;

#[allow(clippy::too_many_arguments)]
pub(super) fn deliver_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    batch: &str,
    observer: &str,
    resource: &str,
    subscription: &str,
    recipient: &str,
    repository_id: &Value,
    failures: &[Value],
    baseline: bool,
) -> Result<Vec<String>, St3Error> {
    let mut messages = Vec::new();
    // Each recipient establishes its own baseline, including when the first listing is
    // empty. Registering later cannot turn another recipient's baseline into a history flood.
    let baseline_identity = repository_id
        .as_u64()
        .map(Value::from)
        .unwrap_or_else(|| Value::String(resource.to_owned()));
    let baseline_key = canonical_hash(&(
        "main-performance-recipient-baseline",
        recipient,
        baseline_identity,
    ))
    .map_err(internal)?;
    let baseline_subject = format!("resource/github-workflow-baseline/{baseline_key}");
    let has_baseline = latest_actual(transaction, &baseline_subject)
        .map_err(internal)?
        .is_some();
    let baseline = baseline || !has_baseline;
    if !has_baseline {
        append_claim_tx(transaction, origin, &baseline_subject, "resource.observed", None,
            &json!({"fields": {"kind": "custom.github.workflow-failure-baseline", "observer": observer,
                "facts": {"recipient": recipient, "resource": resource, "repository_id": repository_id}}}),
            &[], Some(batch)).map_err(claim_append_error)?;
    }
    for facts in failures {
        let Some(failure) = PerformanceFailure::from_facts(facts) else {
            continue;
        };
        // The subscription and observer names are intentionally absent: replacement
        // registrations to the same recipient cannot redeliver a known attempt/head.
        let repository_identity = repository_id
            .as_u64()
            .map(Value::from)
            .unwrap_or_else(|| Value::String(failure.repository.clone()));
        let key = canonical_hash(&(
            "main-performance-failure",
            recipient,
            repository_identity,
            failure.run_id,
            failure.run_attempt,
            &failure.head_sha,
        ))
        .map_err(internal)?;
        let receipt = format!("resource/github-workflow-failure/{key}");
        if latest_actual(transaction, &receipt)
            .map_err(internal)?
            .is_some()
        {
            continue;
        }
        let record = append_claim_tx(
            transaction,
            origin,
            &receipt,
            "resource.observed",
            None,
            &json!({"fields": {
                "kind": "custom.github.workflow-failure",
                "observer": observer,
                "facts": {"failure": failure, "recipient": recipient, "baseline": baseline},
            }}),
            &[],
            Some(batch),
        )
        .map_err(claim_append_error)?;
        if baseline {
            continue;
        }
        let subject = format!("message/workflow-failure-{}", &key[..20]);
        let mut content = serde_json::to_value(&failure).map_err(internal)?;
        content["priority"] = Value::String("P0".into());
        append_claim_tx(transaction, origin, &subject, "message.sent", None,
            &json!({"fields": {
                "from": format!("daemon/{origin}"),
                "to": recipient,
                "title": format!("P0: main Performance failed: {} run {}/{}", failure.repository, failure.run_id, failure.run_attempt),
                "content": serde_json::to_string(&content).map_err(internal)?,
                "status": "sent",
                "tags": ["P0", "github-workflow-failure", subscription, resource],
            }, "evidence": [record.id]}), &[], Some(batch)).map_err(claim_append_error)?;
        messages.push(subject);
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::parse_test_intent as parse_intent;
    use crate::resource::github_workflows::PERFORMANCE_FAILURES_FIELD;

    const SOURCE: &str = r#"version 2
resource "repo" { kind "vcs.repository" }
observer "repo" { resource "resource/repo"; provider "github.repository"; locator "acme/garden"; field "issues" }
agent "target" { workspace "."; command "true" }
subscription "performance" { observer "observer/repo"; on "main_performance_failures"; to "agent/node.target"; delivery "message" }
"#;

    fn publish(store: &Store, source: &str, key: &str) {
        let intent = parse_intent(source, "node").unwrap();
        let plan = store
            .mission(
                &intent,
                IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        assert!(plan.blockers.is_empty());
        let recipient = intent
            .subjects
            .values()
            .filter_map(|desired| crate::graph::subscription_spec(&desired.desired))
            .find(|spec| !spec.stopped)
            .map(|spec| spec.to)
            .unwrap_or_else(|| "agent/node.target".into());
        store
            .apply_as(&intent, &plan.subject_tokens, key, Some(&recipient))
            .unwrap();
    }

    fn failure(run_id: u64, run_attempt: u64, head: &str) -> Value {
        json!({"repository": "acme/garden", "run_id": run_id, "run_attempt": run_attempt,
            "head_sha": head.repeat(40), "workflow": "Performance", "workflow_path": ".github/workflows/perf.yml",
            "event": "push", "head_branch": "main", "status": "completed", "conclusion": "failure",
            "url": format!("https://github.com/acme/garden/actions/runs/{run_id}")})
    }

    fn observe(store: &Store, failures: Value, cursor: &str) -> ResourceObservationOutcome {
        let revision = store
            .selected_desired_revision("observer/repo")
            .unwrap()
            .unwrap();
        let subscriptions = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .filter(|item| item.kind == "subscription")
            .map(|item| {
                (
                    item.subject,
                    crate::graph::subscription_spec(&item.desired).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        store
            .record_resource_observation(
                "observer/repo",
                &revision,
                None,
                "resource/repo",
                Some(cursor),
                &json!({"repository_id": 7, PERFORMANCE_FAILURES_FIELD: failures}),
                100,
                &subscriptions,
                None,
            )
            .unwrap()
    }

    #[test]
    fn main_performance_failures_baseline_and_dedupe_across_restart_and_registration() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claims.sqlite3");
        let store = Store::open(&path, "node").unwrap();
        publish(&store, SOURCE, "register");
        assert!(
            observe(&store, json!([failure(1, 1, "a")]), "baseline")
                .message_subjects
                .is_empty()
        );
        let newly_failed = observe(
            &store,
            json!([failure(1, 1, "a"), failure(2, 1, "b")]),
            "new",
        );
        assert_eq!(newly_failed.message_subjects.len(), 1);
        let message = store
            .claims_for(&newly_failed.message_subjects[0], Some("message.sent"))
            .unwrap()
            .remove(0);
        assert!(
            message.body["fields"]["title"]
                .as_str()
                .unwrap()
                .starts_with("P0:")
        );
        assert_eq!(message.body["fields"]["to"], "agent/node.target");
        let content: Value =
            serde_json::from_str(message.body["fields"]["content"].as_str().unwrap()).unwrap();
        assert_eq!(content["priority"], "P0");
        assert_eq!(content["repository"], "acme/garden");
        assert_eq!(content["run_id"], 2);
        assert_eq!(content["run_attempt"], 1);
        assert_eq!(content["head_sha"], "b".repeat(40));
        assert_eq!(content["workflow_path"], ".github/workflows/perf.yml");
        assert_eq!(
            content["url"],
            "https://github.com/acme/garden/actions/runs/2"
        );
        let duplicate = observe(
            &store,
            json!([failure(2, 1, "b")]),
            "same-failure-changed-list",
        );
        assert!(duplicate.message_subjects.is_empty());
        let mut renamed = failure(2, 1, "b");
        renamed["repository"] = json!("acme/renamed");
        renamed["url"] = json!("https://github.com/acme/renamed/actions/runs/2");
        assert!(
            observe(&store, json!([renamed]), "renamed-same-repository-id")
                .message_subjects
                .is_empty()
        );
        assert_eq!(store.messages(None, true).unwrap().len(), 1);
        drop(store);
        let store = Store::open(&path, "node").unwrap();
        observe(&store, json!([]), "disappeared");
        let replacement = SOURCE.replace(
            "subscription \"performance\"",
            "subscription \"replacement\"",
        );
        publish(
            &store,
            &format!("{replacement}\nsubscription \"performance\" {{ stop }}"),
            "replace-subscription",
        );
        assert!(
            observe(
                &store,
                json!([failure(1, 1, "a"), failure(2, 1, "b")]),
                "reappeared"
            )
            .message_subjects
            .is_empty()
        );
        let changed = observe(
            &store,
            json!([failure(2, 2, "b"), failure(2, 2, "c"), failure(3, 1, "d")]),
            "rerun-and-new-head",
        );
        assert_eq!(changed.message_subjects.len(), 3);
        assert_eq!(store.messages(None, true).unwrap().len(), 4);
    }

    #[test]
    fn main_performance_failures_a_second_recipient_baselines_existing_history_and_an_empty_first_snapshot()
     {
        let store = Store::open_memory("node").unwrap();
        publish(&store, SOURCE, "first-recipient");
        observe(&store, json!([failure(1, 1, "a")]), "first-baseline");
        assert_eq!(
            observe(
                &store,
                json!([failure(1, 1, "a"), failure(2, 1, "b")]),
                "first-new"
            )
            .message_subjects
            .len(),
            1
        );
        publish(
            &store,
            r#"version 2
agent "second" { workspace "."; command "true" }
subscription "second-performance" { observer "observer/repo"; on "main_performance_failures"; to "agent/node.second"; delivery "message" }
"#,
            "second-recipient",
        );
        assert!(
            observe(
                &store,
                json!([failure(1, 1, "a"), failure(2, 1, "b")]),
                "second-baseline"
            )
            .message_subjects
            .is_empty()
        );
        assert_eq!(
            observe(
                &store,
                json!([failure(1, 1, "a"), failure(2, 1, "b"), failure(3, 1, "c")]),
                "both-new"
            )
            .message_subjects
            .len(),
            2
        );
        publish(
            &store,
            r#"version 2
agent "third" { workspace "."; command "true" }
subscription "third-performance" { observer "observer/repo"; on "main_performance_failures"; to "agent/node.third"; delivery "message" }
"#,
            "third-recipient",
        );
        assert!(
            observe(&store, json!([]), "empty-baseline")
                .message_subjects
                .is_empty()
        );
        assert_eq!(
            observe(&store, json!([failure(4, 1, "d")]), "all-new")
                .message_subjects
                .len(),
            3
        );
    }

    #[test]
    fn main_performance_failures_baseline_when_added_to_an_existing_observer() {
        let store = Store::open_memory("node").unwrap();
        publish(&store, SOURCE, "register");
        let revision = store
            .selected_desired_revision("observer/repo")
            .unwrap()
            .unwrap();
        store
            .record_resource_observation(
                "observer/repo",
                &revision,
                None,
                "resource/repo",
                None,
                &json!({"repository_id": 7}),
                100,
                &[],
                None,
            )
            .unwrap();
        assert!(
            observe(&store, json!([failure(1, 1, "a")]), "field-baseline")
                .message_subjects
                .is_empty()
        );
        let mut wrong = failure(99, 1, "a");
        wrong["event"] = json!("pull_request");
        let next = observe(
            &store,
            json!([failure(1, 1, "a"), failure(2, 1, "b"), wrong]),
            "next",
        );
        assert_eq!(next.message_subjects.len(), 1);
    }

    #[test]
    fn main_performance_failures_roll_back_the_receipt_and_message_together() {
        let store = Store::open_memory("node").unwrap();
        publish(&store, SOURCE, "register");
        observe(&store, json!([]), "baseline");
        let revision = store
            .selected_desired_revision("observer/repo")
            .unwrap()
            .unwrap();
        let subscriptions = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .filter(|item| item.kind == "subscription")
            .map(|item| {
                (
                    item.subject,
                    crate::graph::subscription_spec(&item.desired).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let current = || calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
        let failed = store.record_resource_observation(
            "observer/repo",
            &revision,
            None,
            "resource/repo",
            Some("cancelled"),
            &json!({"repository_id": 7, PERFORMANCE_FAILURES_FIELD: [failure(2, 1, "b")]}),
            100,
            &subscriptions,
            Some(&current),
        );
        assert!(failed.is_err());
        assert!(store.messages(None, true).unwrap().is_empty());
        assert_eq!(
            observe(&store, json!([failure(2, 1, "b")]), "retry")
                .message_subjects
                .len(),
            1
        );
        assert_eq!(store.messages(None, true).unwrap().len(), 1);
    }
}
