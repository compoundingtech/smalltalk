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
                None,
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
                None,
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

struct CompletionProvider {
    requests: tokio::sync::mpsc::UnboundedSender<(
        ObservationRequest,
        tokio::sync::oneshot::Sender<Result<crate::resource::ProviderObservation>>,
    )>,
}

impl ResourceProvider for CompletionProvider {
    fn observe(
        &self,
        request: ObservationRequest,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<crate::resource::ProviderObservation>> + Send + '_>> {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.requests.send((request, send)).unwrap();
        Box::pin(async move { receive.await? })
    }
}

struct CompletionWriterBarrier {
    checks: std::sync::atomic::AtomicUsize,
    armed: Arc<Mutex<std::collections::HashSet<String>>>,
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    finished: tokio::sync::mpsc::UnboundedSender<()>,
}

impl FaultInjection for CompletionWriterBarrier {
    fn fault(&self, scope: &str, subject: &str) -> Option<String> {
        if subject != "observer/ref" {
            return None;
        }
        if scope == "observer-completion-write"
            && self.checks.fetch_add(1, Ordering::SeqCst) == 1
        {
            // The first check precedes Store I/O; the second is inside the writer
            // transaction. Hold its ACK, not the scheduler mutex, until rearming ends.
            assert!(self.armed.try_lock().is_ok(), "writer transaction retained the observer scheduling mutex");
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
        } else if scope == "observer-completion-finished" {
            self.finished.send(()).unwrap();
        }
        None
    }
}

async fn completion_during_writer_ack_does_not_block_rearming_or_publish_stale(failed: bool) {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&root.path().join("completion.sqlite3"), "node").unwrap());
    apply_source(
        &store,
        "version 2\nresource \"ref\" { kind \"vcs.ref\" }\nobserver \"ref\" { resource \"resource/ref\"; provider \"github.ref\"; locator \"acme/garden@main\"; field \"head\" }",
        "observer",
    );
    let default_desired = store.desired_subjects().unwrap();
    apply_source(
        &store,
        "version 2\nsubscription \"watch\" { observer \"observer/ref\"; on \"head\"; to \"agent/example\"; delivery \"message\" }",
        "watch",
    );
    let watched_desired = store.desired_subjects().unwrap();
    apply_source(&store, "version 2\nsubscription \"watch\" { stop }", "unwatch");
    // Cadence snapshots have the same durable observer revision. This isolates
    // cancel/rearm's UUID fence from the independent selected-revision fence.
    let (requests, mut pending) = tokio::sync::mpsc::unbounded_channel();
    let (entered, mut writer_entered) = tokio::sync::mpsc::unbounded_channel();
    let (release, released) = std::sync::mpsc::channel();
    let (finished, mut completions) = tokio::sync::mpsc::unbounded_channel();
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .with_resource_provider(Arc::new(CompletionProvider { requests }));
    let armed = reconciler.armed_observers.clone();
    let reconciler = Arc::new(reconciler.with_fault_injection(Arc::new(CompletionWriterBarrier {
        checks: std::sync::atomic::AtomicUsize::new(0),
        armed,
        entered,
        release: Mutex::new(released),
        finished,
    })));
    reconciler.reconcile_resource_observers(&default_desired, &[]).unwrap();
    let (initial, complete) = tokio::time::timeout(Duration::from_secs(5), pending.recv()).await.unwrap().unwrap();
    assert_eq!(initial.every_ms, None);
    let observation = |head: &str| crate::resource::ProviderObservation {
        facts: serde_json::json!({"head":head}),
        cursor: Some(head.into()),
        next_check_unix_ms: now_ms() + 300_000,
    };
    complete.send(if failed {
        Err(anyhow::anyhow!("retired provider request failed"))
    } else {
        Ok(observation("stale"))
    }).unwrap();
    tokio::time::timeout(Duration::from_secs(5), writer_entered.recv()).await.unwrap().unwrap();

    // A full pass may itself need the same writer for other stages. Exercise
    // the actual observer scheduling stage while this completion owns the writer.
    let schedule = reconciler.clone();
    let rearm = tokio::task::spawn_blocking(move || {
        schedule.reconcile_resource_observers(&watched_desired, &[]).unwrap();
        schedule.reconcile_resource_observers(&default_desired, &[]).unwrap();
    });
    let scheduled = tokio::time::timeout(Duration::from_secs(1), rearm).await;
    // Always release the writer before reporting a bounded-progress failure.
    release.send(()).unwrap();
    scheduled.expect("observer scheduling waited for completion's writer ACK").unwrap();
    tokio::time::timeout(Duration::from_secs(5), completions.recv()).await.unwrap().unwrap();

    assert!(store.latest_actual_value("resource/ref").unwrap().is_none());
    assert!(store.latest_actual_value("observer/ref").unwrap().is_none());
    assert!(store.claims_for("resource/ref", Some("resource.observed")).unwrap().is_empty());
    assert!(store.claims_for("observer/ref", Some("observer.state")).unwrap().is_empty());
    assert!(reconciler.observer_deadlines.lock().unwrap().is_empty());
    assert!(reconciler.observer_cursors.lock().unwrap().is_empty());
    let (newest, complete) = tokio::time::timeout(Duration::from_secs(5), pending.recv()).await.unwrap().unwrap();
    // The watched arm was retired before its task could enter the provider;
    // if it did enter, drain it and select the rearmed default-cadence request.
    let (newest, complete) = if newest.every_ms.is_some() {
        drop(complete);
        tokio::time::timeout(Duration::from_secs(5), pending.recv()).await.unwrap().unwrap()
    } else {
        (newest, complete)
    };
    assert_eq!(newest.every_ms, None);
    assert_eq!(newest.cursor, None);
    complete.send(Ok(observation("fresh"))).unwrap();
    tokio::time::timeout(Duration::from_secs(5), completions.recv()).await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if store.latest_actual_value("resource/ref").unwrap()
                .is_some_and(|actual| actual["facts"]["head"] == "fresh") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }).await.expect("the current default-cadence completion did not publish");
    assert_eq!(store.latest_actual_value("resource/ref").unwrap().unwrap()["facts"]["head"], "fresh");
    assert_eq!(store.latest_actual_value("observer/ref").unwrap().unwrap()["state"], "healthy");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn observer_success_waiting_for_writer_ack_allows_rearm_and_rejects_stale_completion() {
    completion_during_writer_ack_does_not_block_rearming_or_publish_stale(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn observer_failure_waiting_for_writer_ack_allows_rearm_and_rejects_stale_state() {
    completion_during_writer_ack_does_not_block_rearming_or_publish_stale(true).await;
}

#[derive(Default)]
struct CountingFileProvider(std::sync::atomic::AtomicUsize);

impl ResourceProvider for CountingFileProvider {
    fn observe(
        &self,
        request: ObservationRequest,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<crate::resource::ProviderObservation>> + Send + '_>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            crate::resource::RegisteredResourceProvider.observe(request).await
        })
    }
}

async fn settle_observer_tasks() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn unchanged_file_observers_keep_polling_without_reconcile_wakes() {
    const OBSERVERS: usize = 4;
    const INTERVALS: usize = 3;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("watched");
    std::fs::write(&path, "unchanged").unwrap();
    let store = Arc::new(Store::open_memory("node").unwrap());
    let mut source = String::from("version 2\n");
    for index in 0..OBSERVERS {
        source.push_str(&format!(
            "resource \"file-{index}\" {{ kind \"filesystem.file\" }}\nobserver \"file-{index}\" {{ resource \"resource/file-{index}\"; provider \"local.file\"; locator \"{}\"; field \"content_hash\"; every \"2s\" }}\n",
            path.display(),
        ));
    }
    apply_source(&store, &source, "files");
    let provider = Arc::new(CountingFileProvider::default());
    let reconciler = Reconciler::new(
        store.clone(), Arc::new(FakeRuntime::default()), "node".into(), Arc::new(Notify::new()),
    ).with_resource_provider(provider.clone());
    let desired = store.desired_subjects().unwrap();
    reconciler.reconcile_resource_observers(&desired, &[]).unwrap();
    settle_observer_tasks().await;
    assert_eq!(provider.0.load(Ordering::SeqCst), OBSERVERS);
    // The initial baseline writes and wakes; the following pass arms idle polling.
    reconciler.reconcile_resource_observers(&desired, &[]).unwrap();
    settle_observer_tasks().await;
    let generation = *reconciler.event_notify.borrow();
    let index = store.index().unwrap();
    for interval in 1..=INTERVALS {
        tokio::time::advance(Duration::from_millis(2001)).await;
        settle_observer_tasks().await;
        assert_eq!(provider.0.load(Ordering::SeqCst), OBSERVERS * (interval + 1));
        assert_eq!(*reconciler.event_notify.borrow(), generation, "unchanged polls woke reconciliation");
        assert_eq!(store.index().unwrap(), index);
    }
    // A full pass must find the existing arms rather than duplicate them.
    reconciler.reconcile_resource_observers(&desired, &[]).unwrap();
    settle_observer_tasks().await;
    assert_eq!(reconciler.armed_observers.lock().unwrap_or_else(PoisonError::into_inner).len(), OBSERVERS);
    tokio::time::advance(Duration::from_millis(2001)).await;
    settle_observer_tasks().await;
    assert_eq!(provider.0.load(Ordering::SeqCst), OBSERVERS * (INTERVALS + 2));
    apply_source(&store, "version 2\nobserver \"file-0\" { stop }\n", "stop");
    reconciler.reconcile_resource_observers(&store.desired_subjects().unwrap(), &[]).unwrap();
    tokio::time::advance(Duration::from_millis(2001)).await;
    settle_observer_tasks().await;
    assert_eq!(provider.0.load(Ordering::SeqCst), OBSERVERS * (INTERVALS + 3) - 1);
}

#[tokio::test(start_paused = true)]
async fn changed_file_observation_wakes_reconciliation_and_delivers_once() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("watched");
    std::fs::write(&path, "before").unwrap();
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(&store, &format!(
        "version 2\nagent \"example\" {{ workspace \"/tmp\"; command \"true\"; restart \"never\" }}\nresource \"file\" {{ kind \"filesystem.file\" }}\nobserver \"file\" {{ resource \"resource/file\"; provider \"local.file\"; locator \"{}\"; field \"content_hash\"; every \"2s\" }}\nsubscription \"watch\" {{ observer \"observer/file\"; on \"content_hash\"; to \"agent/node.example\"; delivery \"message\" }}",
        path.display(),
    ), "file");
    let provider = Arc::new(CountingFileProvider::default());
    let reconciler = Reconciler::new(
        store.clone(), Arc::new(FakeRuntime::default()), "node".into(), Arc::new(Notify::new()),
    ).with_resource_provider(provider.clone());
    let desired = store.desired_subjects().unwrap();
    reconciler.reconcile_resource_observers(&desired, &[]).unwrap();
    settle_observer_tasks().await;
    reconciler.reconcile_resource_observers(&desired, &[]).unwrap();
    settle_observer_tasks().await;
    let generation = *reconciler.event_notify.borrow();
    std::fs::write(&path, "after").unwrap();
    tokio::time::advance(Duration::from_millis(2001)).await;
    settle_observer_tasks().await;
    assert_eq!(*reconciler.event_notify.borrow(), generation + 1);
    assert_eq!(store.claims_for("resource/file", Some("resource.observed")).unwrap().len(), 2);
    assert_eq!(store.claims_for("observer/file", Some("observer.observed")).unwrap().len(), 2);
    assert_eq!(store.latest_actual_value("subscription/watch").unwrap().unwrap()["state"], "active");
    assert_eq!(store.messages(Some("agent/node.example"), false).unwrap().len(), 1);
    reconciler.reconcile_resource_observers(&desired, &[]).unwrap();
    settle_observer_tasks().await;
    tokio::time::advance(Duration::from_millis(2001)).await;
    settle_observer_tasks().await;
    assert_eq!(*reconciler.event_notify.borrow(), generation + 1);
    assert_eq!(store.messages(Some("agent/node.example"), false).unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn unchanged_observer_failures_keep_backoff_without_reconcile_wakes() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(&store, "version 2\nresource \"ref\" { kind \"vcs.ref\" }\nobserver \"ref\" { resource \"resource/ref\"; provider \"github.ref\"; locator \"acme/garden@main\"; field \"head\" }", "ref");
    let (send, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let reconciler = Reconciler::new(
        store.clone(), Arc::new(FakeRuntime::default()), "node".into(), Arc::new(Notify::new()),
    ).with_resource_provider(Arc::new(CompletionProvider { requests: send }));
    let desired = store.desired_subjects().unwrap();
    reconciler.reconcile_resource_observers(&desired, &[]).unwrap();
    settle_observer_tasks().await;
    requests.try_recv().unwrap().1.send(Err(anyhow::anyhow!("temporary failure"))).unwrap();
    settle_observer_tasks().await;
    assert_eq!(*reconciler.event_notify.borrow(), 1);
    reconciler.reconcile_resource_observers(&desired, &[]).unwrap();
    settle_observer_tasks().await;
    for _ in 0..3 {
        tokio::time::advance(Duration::from_secs(59)).await;
        settle_observer_tasks().await;
        assert!(requests.try_recv().is_err());
        tokio::time::advance(Duration::from_secs(2)).await;
        settle_observer_tasks().await;
        requests.try_recv().unwrap().1.send(Err(anyhow::anyhow!("temporary failure"))).unwrap();
        settle_observer_tasks().await;
        assert_eq!(*reconciler.event_notify.borrow(), 1);
        assert_eq!(store.claims_for("observer/ref", Some("observer.state")).unwrap().len(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn restarted_file_observer_rearms_once_without_new_claims_or_wakes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("watched");
    std::fs::write(&path, "unchanged").unwrap();
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(&store, &format!(
        "version 2\nresource \"file\" {{ kind \"filesystem.file\" }}\nobserver \"file\" {{ resource \"resource/file\"; provider \"local.file\"; locator \"{}\"; field \"content_hash\"; every \"2s\" }}",
        path.display(),
    ), "file");
    let provider = Arc::new(CountingFileProvider::default());
    let desired = store.desired_subjects().unwrap();
    let first = Reconciler::new(
        store.clone(), Arc::new(FakeRuntime::default()), "node".into(), Arc::new(Notify::new()),
    ).with_resource_provider(provider.clone());
    first.reconcile_resource_observers(&desired, &[]).unwrap();
    settle_observer_tasks().await;
    let index = store.index().unwrap();
    assert_eq!(provider.0.load(Ordering::SeqCst), 1);
    let restarted = Reconciler::new(
        store.clone(), Arc::new(FakeRuntime::default()), "node".into(), Arc::new(Notify::new()),
    ).with_resource_provider(provider.clone());
    restarted.reconcile_resource_observers(&desired, &[]).unwrap();
    settle_observer_tasks().await;
    assert_eq!(provider.0.load(Ordering::SeqCst), 2);
    assert_eq!(*restarted.event_notify.borrow(), 0);
    restarted.reconcile_resource_observers(&desired, &[]).unwrap();
    settle_observer_tasks().await;
    tokio::time::advance(Duration::from_millis(2001)).await;
    settle_observer_tasks().await;
    assert_eq!(provider.0.load(Ordering::SeqCst), 3);
    assert_eq!(store.index().unwrap(), index);
    assert_eq!(*restarted.event_notify.borrow(), 0);
}

#[tokio::test(start_paused = true)]
async fn idle_observer_replaces_its_arm_when_subscriptions_change() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(&store, "version 2\nagent \"example\" { workspace \"/tmp\"; command \"true\"; restart \"never\" }\nresource \"ref\" { kind \"vcs.ref\" }\nobserver \"ref\" { resource \"resource/ref\"; provider \"github.ref\"; locator \"acme/garden@main\"; field \"head\"; every \"2s\" }", "ref");
    let (send, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let reconciler = Reconciler::new(
        store.clone(), Arc::new(FakeRuntime::default()), "node".into(), Arc::new(Notify::new()),
    ).with_resource_provider(Arc::new(CompletionProvider { requests: send }));
    let observation = |head: &str| Ok(crate::resource::ProviderObservation {
        facts: serde_json::json!({"head": head}),
        cursor: Some(head.into()),
        next_check_unix_ms: now_ms() + 300_000,
    });
    let desired = store.desired_subjects().unwrap();
    reconciler.reconcile_resource_observers(&desired, &[]).unwrap();
    settle_observer_tasks().await;
    requests.try_recv().unwrap().1.send(observation("a")).unwrap();
    settle_observer_tasks().await;
    reconciler.reconcile_resource_observers(&desired, &[]).unwrap();
    settle_observer_tasks().await;
    tokio::time::advance(Duration::from_millis(2001)).await;
    settle_observer_tasks().await;
    requests.try_recv().unwrap().1.send(observation("a")).unwrap();
    settle_observer_tasks().await;
    let generation = *reconciler.event_notify.borrow();
    apply_source(&store, "version 2\nsubscription \"watch\" { observer \"observer/ref\"; on \"head\"; to \"agent/node.example\"; delivery \"message\" }", "watch");
    reconciler.reconcile_resource_observers(&store.desired_subjects().unwrap(), &[]).unwrap();
    settle_observer_tasks().await;
    tokio::time::advance(Duration::from_millis(2001)).await;
    settle_observer_tasks().await;
    requests.try_recv().unwrap().1.send(observation("b")).unwrap();
    assert!(requests.try_recv().is_err(), "a full pass double-armed the observer");
    settle_observer_tasks().await;
    assert_eq!(*reconciler.event_notify.borrow(), generation + 1);
    assert_eq!(store.latest_actual_value("subscription/watch").unwrap().unwrap()["state"], "active");
    assert_eq!(store.messages(Some("agent/node.example"), false).unwrap().len(), 1);
    reconciler.reconcile_resource_observers(&store.desired_subjects().unwrap(), &[]).unwrap();
    settle_observer_tasks().await;
    apply_source(&store, "version 2\nobserver \"ref\" { stop }", "stop");
    // Removal must fence the sleeping poll even before another pass settles it.
    tokio::time::advance(Duration::from_millis(2001)).await;
    settle_observer_tasks().await;
    assert!(requests.try_recv().is_err());
    assert_eq!(*reconciler.event_notify.borrow(), generation + 1);
}

#[tokio::test(start_paused = true)]
async fn observer_refresh_replaces_the_idle_arm_and_wakes_for_its_receipt() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("watched");
    std::fs::write(&path, "unchanged").unwrap();
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(&store, &format!(
        "version 2\nresource \"file\" {{ kind \"filesystem.file\" }}\nobserver \"file\" {{ resource \"resource/file\"; provider \"local.file\"; locator \"{}\"; field \"content_hash\"; every \"2s\" }}",
        path.display(),
    ), "file");
    let provider = Arc::new(CountingFileProvider::default());
    let desired = store.desired_subjects().unwrap();
    let reconciler = Reconciler::new(
        store.clone(), Arc::new(FakeRuntime::default()), "node".into(), Arc::new(Notify::new()),
    ).with_resource_provider(provider.clone());
    reconciler.reconcile_resource_observers(&desired, &[]).unwrap();
    settle_observer_tasks().await;
    reconciler.reconcile_resource_observers(&desired, &[]).unwrap();
    settle_observer_tasks().await;
    let generation = *reconciler.event_notify.borrow();
    store.append_claim(&ClaimInput {
        subject: "observer/file".into(),
        kind: "observer.refresh-requested".into(),
        actor: None,
        fields: BTreeMap::from([
            ("attempt".into(), Value::String("manual".into())),
            ("revision".into(), Value::String(store.selected_desired_revision("observer/file").unwrap().unwrap())),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: None,
    }).unwrap();
    reconciler.reconcile_resource_observers(&desired, &[]).unwrap();
    assert_eq!(reconciler.armed_observers.lock().unwrap_or_else(PoisonError::into_inner).len(), 1);
    settle_observer_tasks().await;
    assert_eq!(provider.0.load(Ordering::SeqCst), 2);
    assert_eq!(*reconciler.event_notify.borrow(), generation + 1);
    assert!(store.pending_observer_refresh_attempt("observer/file").unwrap().is_none());
    assert_eq!(store.claims_for("resource/file", Some("resource.observed")).unwrap().len(), 1);
    assert_eq!(store.claims_for("observer/file", Some("observer.observed")).unwrap().len(), 2);
    reconciler.reconcile_resource_observers(&desired, &[]).unwrap();
    settle_observer_tasks().await;
    tokio::time::advance(Duration::from_millis(2001)).await;
    settle_observer_tasks().await;
    assert_eq!(provider.0.load(Ordering::SeqCst), 3);
    assert_eq!(*reconciler.event_notify.borrow(), generation + 1);
}
