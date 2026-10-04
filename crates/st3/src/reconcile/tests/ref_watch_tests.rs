use super::*;

#[derive(Default)]
struct RefProvider(Mutex<Vec<ObservationRequest>>);
impl ResourceProvider for RefProvider {
    fn observe(
        &self,
        request: ObservationRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<crate::resource::ProviderObservation>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            self.0.lock().unwrap().push(request);
            Ok(crate::resource::ProviderObservation {
                facts: serde_json::json!({"head":"a".repeat(40)}),
                cursor: Some("one".into()),
                next_check_unix_ms: now_ms() + 300_000,
            })
        })
    }
}

const REF_SOURCE: &str = r#"version 2
resource "ref" { kind "vcs.ref" }
observer "ref" { resource "resource/ref"; provider "github.ref"; locator "acme/garden@main"; field "head" }
subscription "apply" { observer "observer/ref"; on "head"; delivery "mission" { mission "review"; resource "source"; workspace "/tmp/example-ref-applies" } }
"#;

#[tokio::test(start_paused = true)]
async fn watched_ref_replaces_a_sleeping_slow_poll_and_unwatch_restores_the_default() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(
        &store,
        "version 2\nresource \"ref\" { kind \"vcs.ref\" }\nobserver \"ref\" { resource \"resource/ref\"; provider \"github.ref\"; locator \"acme/garden@main\"; field \"head\" }",
        "observer",
    );
    let provider = Arc::new(RefProvider::default());
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .with_resource_provider(provider.clone());
    let desired = store.desired_subjects().unwrap();
    reconciler
        .reconcile_resource_observers(&desired, &[])
        .unwrap();
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(provider.0.lock().unwrap().len(), 1);
    reconciler
        .reconcile_resource_observers(&desired, &[])
        .unwrap();
    apply_source(&store, &REF_SOURCE.replace("delivery \"mission\" { mission \"review\"; resource \"source\"; workspace \"/tmp/example-ref-applies\" }", "to \"agent/example\"; delivery \"message\""), "watch");
    let desired = store.desired_subjects().unwrap();
    reconciler
        .reconcile_resource_observers(&desired, &[])
        .unwrap();
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(31)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(provider.0.lock().unwrap().len(), 2);
    assert_eq!(
        provider.0.lock().unwrap()[1].every_ms,
        Some(crate::resource::GITHUB_REF_WATCH_MS)
    );
    apply_source(
        &store,
        "version 2\nsubscription \"apply\" { stop }",
        "unwatch",
    );
    let desired = store.desired_subjects().unwrap();
    reconciler
        .reconcile_resource_observers(&desired, &[])
        .unwrap();
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(31)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(provider.0.lock().unwrap().len(), 3);
    assert_eq!(provider.0.lock().unwrap()[2].every_ms, None);
}

#[test]
fn watched_ref_queue_collapses_after_restart_without_repinning_the_active_run() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("queue.db");
    let store = Arc::new(Store::open(&path, "node").unwrap());
    apply_source(&store, QUEUED_REVIEW_SOURCE, "mission");
    apply_source(&store, REF_SOURCE, "ref");
    let desired = store.desired_subjects().unwrap();
    let item = desired
        .iter()
        .find(|d| d.subject == "subscription/apply")
        .unwrap();
    let subscriptions = vec![(
        item.subject.clone(),
        crate::graph::subscription_spec(&item.desired).unwrap(),
    )];
    let revision = store
        .selected_desired_revision("observer/ref")
        .unwrap()
        .unwrap();
    let observe = |head: &str| {
        store
            .record_resource_observation(
                "observer/ref",
                &revision,
                None,
                "resource/ref",
                None,
                &serde_json::json!({"head":head.repeat(40)}),
                now_ms() + 30_000,
                &subscriptions,
            )
            .unwrap()
    };
    observe("0"); // The first observation establishes the subscription baseline.
    observe("a");
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    reconciler
        .reconcile_subscription_missions(&desired)
        .unwrap();
    let running = store
        .active_mission_runs_for_mission("review")
        .unwrap()
        .remove(0);
    let pinned = running.inputs["source"].value.clone();
    for head in ["b", "c", "d", "c", "d"] {
        observe(head);
    }
    drop(reconciler);
    drop(store);
    let store = Arc::new(Store::open(&path, "node").unwrap());
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    let desired = store.desired_subjects().unwrap();
    reconciler
        .reconcile_subscription_missions(&desired)
        .unwrap();
    assert_eq!(
        store
            .claims_for(
                "subscription/apply",
                Some("subscription.mission-request-cancelled")
            )
            .unwrap()
            .len(),
        4
    );
    let queued = store
        .pending_subscription_mission_requests("subscription/apply")
        .unwrap();
    assert_eq!(queued.len(), 1);
    let latest = store
        .claim_by_id(queued[0].body["fields"]["discovery"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(latest.body["fields"]["facts"]["head"], "d".repeat(40));
    assert_eq!(
        store.mission_run(&running.id).unwrap().unwrap().inputs["source"].value,
        pinned
    );
    for step in &running.steps {
        store
            .set_step_state(&step.subject, "completed", None)
            .unwrap();
    }
    for _ in 0..5 {
        reconciler.evaluate_mission_runs().unwrap();
    }
    store
        .record_subscription_mission_deferral("subscription/apply", &queued[0].id, 0, 1)
        .unwrap();
    reconciler
        .reconcile_subscription_missions(&desired)
        .unwrap();
    let newest = store
        .active_mission_runs_for_mission("review")
        .unwrap()
        .remove(0);
    assert_eq!(
        newest.inputs["source"].value,
        format!("resource/ref@{}", latest.id)
    );
    assert!(
        store
            .pending_subscription_mission_requests("subscription/apply")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn watched_ref_start_rechecks_the_head_inside_the_run_transaction_and_replays_started_runs() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(&store, QUEUED_REVIEW_SOURCE, "mission");
    apply_source(&store, REF_SOURCE, "ref");
    let revision = store
        .selected_desired_revision("observer/ref")
        .unwrap()
        .unwrap();
    let observe = |head: &str| {
        store
            .record_resource_observation(
                "observer/ref",
                &revision,
                None,
                "resource/ref",
                None,
                &serde_json::json!({"head":head.repeat(40)}),
                now_ms() + 30_000,
                &[],
            )
            .unwrap();
        store
            .claims_for("resource/ref", Some("resource.observed"))
            .unwrap()
            .pop()
            .unwrap()
    };
    let first = observe("a");
    let request = |key: &str| MissionRunRequest {
        mission: "review".into(),
        revision: None,
        workspace: "/tmp/example-ref-applies".into(),
        requester: Some("person/operator".into()),
        mode: None,
        inputs: BTreeMap::from([("source".into(), format!("resource/ref@{}", first.id))]),
        idempotency_key: key.into(),
    };
    let original = request("original");
    let started = store
        .create_subscription_mission_run(
            &original,
            None,
            "subscription/apply",
            "resource/ref",
            &first.id,
        )
        .unwrap();
    observe("b");
    let before = store.index().unwrap();
    let stale = store
        .create_subscription_mission_run(
            &request("stale"),
            None,
            "subscription/apply",
            "resource/ref",
            &first.id,
        )
        .unwrap_err();
    assert_eq!(stale.code, "stale-ref-head");
    assert_eq!(store.index().unwrap(), before);
    let replayed = store
        .create_subscription_mission_run(
            &original,
            None,
            "subscription/apply",
            "resource/ref",
            &first.id,
        )
        .unwrap();
    assert_eq!(replayed.id, started.id);
    assert_eq!(
        replayed.inputs["source"].value,
        format!("resource/ref@{}", first.id)
    );
}

#[derive(Default)]
struct DelayedRefProvider(
    Mutex<Vec<tokio::sync::oneshot::Sender<crate::resource::ProviderObservation>>>,
);
impl ResourceProvider for DelayedRefProvider {
    fn observe(
        &self,
        _: ObservationRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<crate::resource::ProviderObservation>>
                + Send
                + '_,
        >,
    > {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.0.lock().unwrap().push(send);
        Box::pin(async move { Ok(receive.await?) })
    }
}

#[tokio::test(start_paused = true)]
async fn watched_ref_discards_inflight_results_even_after_cadence_returns_to_default() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(
        &store,
        "version 2\nresource \"ref\" { kind \"vcs.ref\" }\nobserver \"ref\" { resource \"resource/ref\"; provider \"github.ref\"; locator \"acme/garden@main\"; field \"head\" }",
        "observer",
    );
    let provider = Arc::new(DelayedRefProvider::default());
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .with_resource_provider(provider.clone());
    for (index, source) in [
        None,
        Some(
            "version 2\nsubscription \"watch\" { observer \"observer/ref\"; on \"head\"; to \"agent/example\"; delivery \"message\" }",
        ),
        Some("version 2\nsubscription \"watch\" { stop }"),
    ].into_iter().enumerate() {
        if let Some(source) = source {
            apply_source(&store, source, &format!("watch-{index}"));
        }
        reconciler
            .reconcile_resource_observers(&store.desired_subjects().unwrap(), &[])
            .unwrap();
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(31)).await;
        for _ in 0..10 { tokio::task::yield_now().await; }
    }
    let mut requests = std::mem::take(&mut *provider.0.lock().unwrap());
    assert_eq!(requests.len(), 3);
    let observation = |head: &str| crate::resource::ProviderObservation {
        facts: serde_json::json!({"head":head}),
        cursor: Some(head.into()),
        next_check_unix_ms: now_ms() + 300_000,
    };
    requests.pop().unwrap().send(observation("newest")).unwrap();
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    for request in requests {
        request.send(observation("stale")).unwrap();
    }
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        store.latest_actual_value("resource/ref").unwrap().unwrap()["facts"]["head"],
        "newest"
    );
    assert_eq!(
        store
            .claims_for("resource/ref", Some("resource.observed"))
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn watched_ref_does_not_shorten_a_rate_limit_retry_deadline() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(
        &store,
        "version 2\nresource \"ref\" { kind \"vcs.ref\" }\nobserver \"ref\" { resource \"resource/ref\"; provider \"github.ref\"; locator \"acme/garden@main\"; field \"head\" }\nsubscription \"watch\" { observer \"observer/ref\"; on \"head\"; to \"agent/example\"; delivery \"message\" }",
        "watch",
    );
    store
        .append_claim(&ClaimInput {
            subject: "observer/ref".into(),
            kind: "observer.state".into(),
            actor: None,
            fields: BTreeMap::from([
                ("state".into(), serde_json::json!("unreachable")),
                ("error_code".into(), serde_json::json!("rate-limited")),
                (
                    "next_check_unix_ms".into(),
                    serde_json::json!((now_ms() + 300_000).to_string()),
                ),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let provider = Arc::new(RefProvider::default());
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .with_resource_provider(provider.clone());
    reconciler
        .reconcile_resource_observers(&store.desired_subjects().unwrap(), &[])
        .unwrap();
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(31)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(provider.0.lock().unwrap().is_empty());
    tokio::time::advance(Duration::from_secs(270)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(provider.0.lock().unwrap().len(), 1);
}
