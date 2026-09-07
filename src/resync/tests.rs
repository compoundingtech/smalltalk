use super::*;

fn refresh_for(sets: Vec<AgentWatchSet>) -> WatchRefresh {
    WatchRefresh {
        sets,
        malformed_declarations: BTreeSet::new(),
        live_task_ids: BTreeSet::new(),
    }
}

#[test]
fn resync_subject_uses_the_shared_three_fact_and_96_scalar_renderer() {
    let facts = vec![
        ResourceFact::transition("alpha", None::<String>, Some("declared")).unwrap(),
        ResourceFact::current("beta", "changed").unwrap(),
        ResourceFact::transition("charlie", Some("declared"), None::<String>).unwrap(),
        ResourceFact::current("delta", "omitted").unwrap(),
    ];
    let subject = crate::resource_profile_supervisor::resource_change_subject(
        "declaration",
        &facts,
        &["declaration".to_owned()],
        "content changed",
    );
    assert_eq!(
        subject,
        "declaration · alpha=+declared; beta=changed; charlie=-declared [declaration]"
    );
    assert!(subject.chars().count() <= 96);
    assert!(!subject.contains("delta"));
}

fn owner_incarnation(seed: u64) -> crate::event::StreamOwnerIncarnation {
    crate::event::StreamOwnerIncarnation::for_test(seed, seed + 1, 42, seed + 2)
}

fn discover(catalog: &Path) -> AgentSpec {
    let found = crate::discover_strict(catalog);
    eprintln!("discovery errors: {:?}", found.errors);
    found.specs.into_iter().next().unwrap()
}

fn resync_inbox_event(agent_dir: &Path) -> String {
    std::fs::read_dir(agent_dir.join("resources/inbox"))
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .find(|event| event.lines().any(|line| line == "stream: resync"))
        .expect("current resync inbox event")
}
fn resync_inbox_events(agent_dir: &Path) -> Vec<String> {
    std::fs::read_dir(agent_dir.join("resources/inbox"))
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter(|event| event.lines().any(|line| line == "stream: resync"))
        .collect()
}

fn event_body(event: &str) -> serde_json::Value {
    let body = event
        .lines()
        .rev()
        .find(|line| line.starts_with('{'))
        .expect("JSON resync body");
    serde_json::from_str(body).expect("valid JSON resync body")
}

fn event_field(event: &str, field: &str) -> String {
    if let Some(value) = event
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{field}: ")))
    {
        return value.to_owned();
    }
    let body = event_body(event);
    if matches!(field, "old" | "new") {
        let digest = body["facts"]
            .as_array()
            .and_then(|facts| facts.iter().find(|fact| fact["key"] == "digest"))
            .expect("digest transition fact");
        let value = if field == "old" {
            &digest["before"]
        } else {
            &digest["after"]
        };
        return value.as_str().expect("digest fact value").to_owned();
    }
    body[field]
        .as_str()
        .unwrap_or_else(|| panic!("missing {field} in event"))
        .to_owned()
}
#[test]
fn declaration_facts_are_ordered_added_removed_and_semantically_changed_labels() {
    let root = tempfile::tempdir().unwrap();
    let declaration = root.path().join("agent.kdl");
    std::fs::write(
        &declaration,
        r#"agent "worker" {
  host "host"
  command "true"
  resource "inactive" uri="file:///inactive" reason="kept"
  resource "reason" uri="file:///reason" reason="before"
  resource "removed" uri="file:///removed" reason="gone"
  resource "uri" uri="file:///before" reason="same"
}"#,
    )
    .unwrap();
    let before = declaration_summary(&discover(root.path()));
    std::fs::write(
        &declaration,
        r#"agent "worker" {
  host "host"
  command "true"
  resource "added" uri="file:///added" reason="new"
  resource "inactive" uri="file:///inactive" reason="kept" inactive-reason="paused"
  resource "reason" uri="file:///reason" reason="after"
  resource "uri" uri="file:///after" reason="same"
}"#,
    )
    .unwrap();
    let after = declaration_summary(&discover(root.path()));
    let facts = declaration_transition_facts(
        Some(&before),
        Some(&after),
        &CarrierState::Present("before-digest".to_owned()),
        &CarrierState::Present("after-digest".to_owned()),
    );
    assert_eq!(
        facts.iter().map(ResourceFact::key).collect::<Vec<_>>(),
        vec!["added", "inactive", "reason", "removed", "uri"]
    );
    assert_eq!(facts[0].before(), Some(None));
    assert_eq!(facts[0].after(), Some(Some("declared")));
    for index in [1, 2, 4] {
        assert_eq!(facts[index].before(), None);
        assert_eq!(facts[index].after(), Some(Some("changed")));
    }
    assert_eq!(facts[3].before(), Some(Some("declared")));
    assert_eq!(facts[3].after(), Some(None));
}

#[test]
fn declaration_parse_failure_retains_a_digest_fact_for_later_delivery() {
    let root = tempfile::tempdir().unwrap();
    let agent_dir = root.path().join("agents/host/worker");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let declaration = agent_dir.join("agent.kdl");
    let valid = r#"agent "worker" {
  host "host"
  command "true"
  resource "goal" uri="resources/goal.md" reason="Mission."
}"#;
    std::fs::write(&declaration, valid).unwrap();
    crate::event::publish_owner_binding_for_test(root.path(), "host").unwrap();
    let set = watch_set_for(&discover(root.path()), "host", &Default::default());
    let mut worker = Worker {
        root: root.path().to_path_buf(),
        this_host: "host".to_owned(),
        carriers: BTreeMap::new(),
        subscription_sequences: BTreeMap::new(),
        parked_transitions: BTreeMap::new(),
        deadlines: BTreeMap::new(),
        watched: BTreeMap::new(),
        watcher: None,
        emit: Arc::new(EmitQueue::default()),
    };
    worker.apply_watch_sets(refresh_for(vec![set]));
    std::fs::write(&declaration, "not an Agent Spec").unwrap();
    worker.flush_path_publishing(&declaration, None);
    let pending = worker.carriers[&declaration][0]
        .pending_transition
        .as_ref()
        .expect("malformed declaration keeps its digest fallback");
    assert_eq!(pending.facts.len(), 1);
    assert_eq!(pending.facts[0].key(), "digest");
    assert_eq!(pending.topics, ["declaration"]);
    assert_eq!(event_body(&pending.body)["facts"].as_array().unwrap().len(), 1);
    std::fs::write(&declaration, valid).unwrap();
    worker.flush_path_publishing(&declaration, None);
    let delivered = resync_inbox_event(&agent_dir);
    let body = event_body(&delivered);
    assert_eq!(body["binding"], "declaration");
    assert_eq!(body["topics"], serde_json::json!(["declaration"]));
    assert_eq!(body["facts"][0]["key"], "digest");
    assert!(!delivered.contains("file:///"));
    assert!(!delivered.contains("Mission."));
}


#[test]
fn watch_set_covers_declaration_and_local_bindings_only() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("agents/hetz/worker");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("agent.kdl"),
        r#"agent "worker" {
  host "hetz"
  command "true"
  resource "goal" uri="resources/goal.md" reason="Mission."
  resource "journal" uri="resources/context/journal.md" reason="Memory."
  resource "issue" uri="github-issue://org/repo/41" reason="Task."
  resource "old" uri="resources/old.md" reason="History." inactive-reason="No longer used."
}"#,
    )
    .unwrap();
    let spec = discover(tmp.path());
    let set = watch_set_for(&spec, "hetz", &Default::default());
    assert_eq!(set.bus_id, "hetz.worker");
    let mut labels: Vec<&str> = set.carriers.iter().map(|c| c.label.as_str()).collect();
    labels.sort();
    assert_eq!(labels, vec!["declaration", "goal"]);
    let goal = set.carriers.iter().find(|c| c.label == "goal").unwrap();
    assert_eq!(goal.class, CarrierClass::Immediate);
    assert_eq!(goal.path, dir.join("resources/goal.md"));
    let coverage = spec
        .resources
        .iter()
        .map(|resource| (resource.name(), resource_coverage(&dir, resource)))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(coverage["goal"], ResyncCoverage::Immediate);
    assert_eq!(coverage["journal"], ResyncCoverage::Silent);
    assert_eq!(coverage["issue"], ResyncCoverage::Unsupported);
    assert_eq!(coverage["old"], ResyncCoverage::Inactive);
}

#[test]
fn bus_id_uses_the_supervisor_host_not_the_os_hostname() {
    let tmp = tempfile::tempdir().unwrap();
    // A root-level declaration file supplies neither content nor path host: host stays
    // None, so the supervisor's logical alias must decide the recipient.
    std::fs::write(
        tmp.path().join("worker.kdl"),
        r#"agent "worker" {
  command "true"
}"#,
    )
    .unwrap();
    let spec = discover(tmp.path());
    assert_eq!(
        watch_set_for(&spec, "alias", &Default::default()).bus_id,
        "alias.worker"
    );
    assert_eq!(
        watch_set_for(&spec, "other", &Default::default()).bus_id,
        "other.worker"
    );
}

#[test]
fn lexical_paths_drive_store_classification_and_containment() {
    let agent_dir = Path::new("/catalog/agents/host/worker");

    let silent = resolve_local_path(
        agent_dir,
        "file:///catalog/agents/host/worker/resources/tmp/../context/./journal.md",
    )
    .unwrap();
    assert_eq!(
        silent,
        agent_dir.join("resources/context/journal.md"),
        "file URI dot segments are removed before classification"
    );
    assert_eq!(classify(agent_dir, "journal", &silent), None);

    let goal = resolve_local_path(agent_dir, "resources/context/.././goal.md").unwrap();
    assert_eq!(goal, agent_dir.join("resources/goal.md"));
    assert_eq!(
        classify(agent_dir, "notes", &goal),
        Some(CarrierClass::Immediate)
    );

    let escaped =
        resolve_local_path(agent_dir, "resources/context/../../outside/notes.md").unwrap();
    assert_eq!(escaped, agent_dir.join("outside/notes.md"));
    assert_eq!(
        classify(agent_dir, "notes", &escaped),
        Some(CarrierClass::Coalesced),
        "a lexical escape from an authored store is not silent"
    );
    assert_eq!(
        classify(
            agent_dir,
            "notes",
            Path::new("/catalog/agents/host/worker-copy/resources/context/notes.md"),
        ),
        Some(CarrierClass::Coalesced),
        "path-prefix siblings are not contained by the agent directory"
    );
}

#[cfg(unix)]
#[test]
fn classification_does_not_follow_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(agent_dir.join("resources/context")).unwrap();
    std::os::unix::fs::symlink(
        agent_dir.join("resources/context"),
        agent_dir.join("resources/linked"),
    )
    .unwrap();

    let linked = resolve_local_path(&agent_dir, "resources/linked/journal.md").unwrap();
    assert_eq!(
        classify(&agent_dir, "journal", &linked),
        Some(CarrierClass::Coalesced),
        "classification is lexical and must not canonicalize through the symlink"
    );
}

#[test]
fn shared_path_refresh_preserves_every_subscription_state() {
    let shared = PathBuf::from("/shared/resource.md");
    let previous = BTreeMap::from([(
        shared.clone(),
        vec![
            Entry {
                bus_id: "host.alpha".to_owned(),
                seat_id: None,
                label: "goal".to_owned(),
                class: CarrierClass::Immediate,
                containment_root: None,
                state: Some(CarrierState::Present("alpha-before".to_owned())),
                declaration_summary: None,
                occurrence_sequence: 4,
                pending_transition: None,
                in_flight: false,
                parked: false,
                dirty: true,
            },
            Entry {
                bus_id: "host.beta".to_owned(),
                seat_id: None,
                label: "spec".to_owned(),
                class: CarrierClass::Coalesced,
                containment_root: None,
                state: Some(CarrierState::Present("beta-before".to_owned())),
                declaration_summary: None,
                occurrence_sequence: 9,
                in_flight: false,
                parked: false,
                dirty: true,
                pending_transition: None,
            },
        ],
    )]);
    let sets = vec![
        AgentWatchSet {
            declaration_path: PathBuf::from("/catalog/alpha/agent.kdl"),
            bus_id: "host.alpha".to_owned(),
            seat_id: None,
            carriers: vec![WatchableCarrier {
                label: "goal".to_owned(),
                path: shared.clone(),
                class: CarrierClass::Immediate,
                containment_root: None,
            }],
            declaration_summary: None,
        },
        AgentWatchSet {
            declaration_path: PathBuf::from("/catalog/beta/agent.kdl"),
            bus_id: "host.beta".to_owned(),
            seat_id: None,
            carriers: vec![WatchableCarrier {
                label: "spec".to_owned(),
                path: shared.clone(),
                class: CarrierClass::Coalesced,
                containment_root: None,
            }],
            declaration_summary: None,
        },
    ];

    let rebuilt = rebuild_carriers(previous, refresh_for(sets), &BTreeMap::new(), &mut BTreeMap::new());
    let entries = rebuilt.get(&shared).expect("shared path remains watched");
    assert_eq!(entries.len(), 2);
    for (bus_id, digest) in [
        ("host.alpha", "alpha-before"),
        ("host.beta", "beta-before"),
    ] {
        let entry = entries
            .iter()
            .find(|entry| entry.bus_id == bus_id)
            .expect("subscriber remains present");
        assert_eq!(
            entry.state,
            Some(CarrierState::Present(digest.to_owned()))
        );
        assert!(entry.dirty, "pending mutation remains pending for {bus_id}");
    }
}

#[test]
fn retained_subscription_uses_current_seat_path_and_class() {
    let root = tempfile::tempdir().unwrap();
    let agent_dir = root.path().join("agents/alias/worker");
    std::fs::create_dir_all(agent_dir.join("resources")).unwrap();
    std::fs::write(
        agent_dir.join("agent.kdl"),
        r#"agent "worker" {
  host "alias"
  command "agent"
  resource "goal" uri="resources/goal.md" reason="Mission."
}"#,
    )
    .unwrap();
    let goal = agent_dir.join("resources/goal.md");
    std::fs::write(&goal, "current bytes").unwrap();
    crate::event::publish_owner_binding_for_test(root.path(), "alias").unwrap();

    let mut current = watch_set_for(
        &discover(root.path()),
        "alias",
        &ResourceProfileRegistry::empty(),
    );
    current.seat_id = Some("current-seat".to_owned());
    let old_path = agent_dir.join("resources/old-goal.md");
    let previous = BTreeMap::from([(
        old_path.clone(),
        vec![Entry {
            bus_id: "alias.worker".to_owned(),
            seat_id: Some("stale-seat".to_owned()),
            label: "goal".to_owned(),
            class: CarrierClass::Coalesced,
            containment_root: None,
            state: Some(CarrierState::Present("old-digest".to_owned())),
            declaration_summary: None,
            occurrence_sequence: 3,
            pending_transition: None,
            in_flight: false,
            parked: false,
            dirty: true,
        }],
    )]);

    let rebuilt = rebuild_carriers(previous, refresh_for(vec![current]), &BTreeMap::new(), &mut BTreeMap::new());
    assert!(!rebuilt.contains_key(&old_path));
    let entry = rebuilt[&goal]
        .iter()
        .find(|entry| entry.label == "goal")
        .expect("the goal subscription remains pending at its current path");
    assert_eq!(entry.bus_id, "alias.worker");
    assert_eq!(entry.seat_id.as_deref(), Some("current-seat"));
    assert_eq!(entry.class, CarrierClass::Immediate);
    assert_eq!(
        entry.state,
        Some(CarrierState::Present("old-digest".to_owned()))
    );
    assert!(entry.dirty);

    let mut worker = Worker {
        root: root.path().to_path_buf(),
        this_host: "alias".to_owned(),
        carriers: rebuilt,
        subscription_sequences: BTreeMap::new(),
        parked_transitions: BTreeMap::new(),
        deadlines: BTreeMap::new(),
        watched: BTreeMap::new(),
        watcher: None,
        emit: Arc::new(EmitQueue::default()),
    };
    worker.flush_path_publishing(&goal, Some(CarrierClass::Immediate));

    let entry = worker.carriers[&goal]
        .iter()
        .find(|entry| entry.label == "goal")
        .unwrap();
    assert!(!entry.dirty, "the current recipient accepted the transition");
    assert!(entry.pending_transition.is_none());
    let events = std::fs::read_dir(agent_dir.join("resources/inbox"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(events.len(), 1, "the event routes to the current bus id");
}

#[test]
fn pending_retry_keeps_its_original_snapshot_across_path_rebinding() {
    let root = tempfile::tempdir().unwrap();
    let agent_dir = root.path().join("agents/alias/worker");
    let resources = agent_dir.join("resources");
    std::fs::create_dir_all(&resources).unwrap();
    let old_path = resources.join("old-goal.md");
    let current_path = resources.join("current-goal.md");
    std::fs::write(&old_path, "pending bytes").unwrap();
    std::fs::write(&current_path, "current rebound bytes").unwrap();
    crate::event::publish_owner_binding_for_test(root.path(), "alias").unwrap();
    let declaration = agent_dir.join("agent.kdl");
    let mut worker = Worker {
        root: root.path().to_path_buf(),
        this_host: "alias".to_owned(),
        carriers: BTreeMap::from([(
            old_path.clone(),
            vec![Entry {
                bus_id: "alias.worker".to_owned(),
                seat_id: None,
                label: "goal".to_owned(),
                class: CarrierClass::Immediate,
                containment_root: None,
                state: Some(CarrierState::Present("old-digest".to_owned())),
                declaration_summary: None,
                occurrence_sequence: 0,
                pending_transition: None,
                in_flight: false,
                parked: false,
                dirty: true,
            }],
        )]),
        subscription_sequences: BTreeMap::new(),
        parked_transitions: BTreeMap::new(),
        deadlines: BTreeMap::new(),
        watched: BTreeMap::new(),
        watcher: None,
        emit: Arc::new(EmitQueue::default()),
    };

    worker.flush_path_publishing(&old_path, None);
    let pending = worker.carriers[&old_path][0]
        .pending_transition
        .clone()
        .expect("failed emit retains an immutable transition");
    std::fs::write(
        &declaration,
        r#"agent "worker" {
  host "alias"
  command "agent"
  resource "goal" uri="resources/current-goal.md" reason="Mission."
}"#,
    )
    .unwrap();
    let current =
        watch_set_for(&discover(root.path()), "alias", &ResourceProfileRegistry::empty());

    worker.apply_watch_sets(refresh_for(vec![current]));
    worker.flush_due_publishing(Instant::now() + IMMEDIATE_WINDOW + Duration::from_secs(1));

    let event = std::fs::read_dir(resources.join("inbox"))
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .find(|body| body.contains("stream: resync"))
        .expect("the retry routes through the refreshed recipient");
    assert!(event.contains(&format!("event-id: {}", pending.event_id)), "{event}");
    assert!(event.contains(&pending.body), "{event}");
    let pending_body: serde_json::Value = serde_json::from_str(&pending.body).unwrap();
    assert_eq!(pending_body["binding"], "goal");
    assert_eq!(pending_body["facts"][0]["before"], "old-digest");
    assert!(
        !pending.body.contains(&current_path.display().to_string()),
        "rebinding must not rewrite bytes reserved under the pending event identity"
    );
    let entry = &worker.carriers[&current_path][0];
    assert_eq!(entry.state.as_ref(), Some(&pending.new_state));
    assert!(entry.pending_transition.is_none());
    assert!(
        entry.dirty,
        "current rebound bytes are queued only after the pending snapshot completes"
    );
}

#[test]
fn same_directory_path_rebinding_diffs_the_new_path_without_a_filesystem_event() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("resources");
    std::fs::create_dir_all(&parent).unwrap();
    let old_path = parent.join("old.md");
    let new_path = parent.join("new.md");
    std::fs::write(&old_path, "old bytes").unwrap();
    std::fs::write(&new_path, "new bytes").unwrap();
    let declaration = root.path().join("agent.kdl");
    let mut worker = Worker {
        root: root.path().to_path_buf(),
        this_host: "host".to_owned(),
        carriers: BTreeMap::from([(
            old_path.clone(),
            vec![Entry {
                bus_id: "host.worker".to_owned(),
                seat_id: None,
                label: "goal".to_owned(),
                class: CarrierClass::Immediate,
                containment_root: None,
                state: read_state(&old_path, None).ok(),
                declaration_summary: None,
                occurrence_sequence: 0,
                pending_transition: None,
                in_flight: false,
                parked: false,
                dirty: false,
            }],
        )]),
        subscription_sequences: BTreeMap::new(),
        parked_transitions: BTreeMap::new(),
        deadlines: BTreeMap::new(),
        watched: BTreeMap::from([(parent.clone(), dir_identity(&parent))]),
        watcher: None,
        emit: Arc::new(EmitQueue::default()),
    };

    worker.apply_watch_sets(refresh_for(vec![AgentWatchSet {
        declaration_path: declaration,
        bus_id: "host.worker".to_owned(),
        seat_id: None,
        carriers: vec![WatchableCarrier {
            label: "goal".to_owned(),
            path: new_path.clone(),
            class: CarrierClass::Immediate,
            containment_root: None,
        }],
        declaration_summary: None,
    }]));

    assert!(!worker.carriers.contains_key(&old_path));
    assert!(worker.carriers[&new_path][0].dirty);
    assert!(
        worker.deadlines.contains_key(&CarrierClass::Immediate),
        "metadata refresh must enqueue the rebound digest even though its parent stayed watched"
    );
}

#[test]
fn dirty_entry_deadline_migrates_when_refresh_changes_notification_class() {
    let root = tempfile::tempdir().unwrap();
    let carrier = root.path().join("carrier.md");
    std::fs::write(&carrier, "same bytes").unwrap();
    let declaration = root.path().join("agent.kdl");
    let old_deadline = Instant::now();
    let mut worker = Worker {
        root: root.path().to_path_buf(),
        this_host: "host".to_owned(),
        carriers: BTreeMap::from([(
            carrier.clone(),
            vec![Entry {
                bus_id: "host.worker".to_owned(),
                seat_id: None,
                label: "spec".to_owned(),
                class: CarrierClass::Immediate,
                containment_root: None,
                state: read_state(&carrier, None).ok(),
                declaration_summary: None,
                occurrence_sequence: 0,
                pending_transition: None,
                in_flight: false,
                parked: false,
                dirty: true,
            }],
        )]),
        subscription_sequences: BTreeMap::new(),
        parked_transitions: BTreeMap::new(),
        deadlines: BTreeMap::from([(CarrierClass::Immediate, old_deadline)]),
        watched: BTreeMap::new(),
        watcher: None,
        emit: Arc::new(EmitQueue::default()),
    };
    let refresh = |class| {
        refresh_for(vec![AgentWatchSet {
            declaration_path: declaration.clone(),
            bus_id: "host.worker".to_owned(),
            seat_id: None,
            carriers: vec![WatchableCarrier {
                label: "spec".to_owned(),
                path: carrier.clone(),
                class,
                containment_root: None,
            }],
            declaration_summary: None,
        }])
    };

    worker.apply_watch_sets(refresh(CarrierClass::Coalesced));
    assert!(!worker.deadlines.contains_key(&CarrierClass::Immediate));
    assert!(
        worker.deadlines[&CarrierClass::Coalesced] >= old_deadline + COALESCED_WINDOW,
        "immediate-to-coalesced migration receives the new class window"
    );

    let coalesced_deadline = worker.deadlines[&CarrierClass::Coalesced];
    worker.apply_watch_sets(refresh(CarrierClass::Immediate));
    assert!(!worker.deadlines.contains_key(&CarrierClass::Coalesced));
    assert!(
        worker.deadlines[&CarrierClass::Immediate] < coalesced_deadline,
        "coalesced-to-immediate migration is rescheduled under the shorter window"
    );
    assert!(worker.carriers[&carrier][0].dirty);
}

#[test]
fn malformed_declaration_retains_only_an_observed_live_seat_subscription() {
    let declaration = PathBuf::from("/catalog/agents/hetz/worker/agent.kdl");
    let previous = || {
        BTreeMap::from([(
            declaration.clone(),
            vec![Entry {
                bus_id: "hetz.worker".to_owned(),
                seat_id: Some("custom-worker-seat".to_owned()),
                label: "declaration".to_owned(),
                class: CarrierClass::Immediate,
                containment_root: None,
                state: Some(CarrierState::Present("before".to_owned())),
                declaration_summary: None,
                occurrence_sequence: 1,
                pending_transition: Some(PendingTransition::new(
                    "declaration",
                    &declaration,
                    &CarrierState::Present("before".to_owned()),
                    &CarrierState::Present("corrected".to_owned()),
                    owner_incarnation(1),
                    1,
                )),
                in_flight: false,
                parked: false,
                dirty: true,
            }],
        )])
    };
    let malformed_declarations = BTreeSet::from([declaration.clone()]);

    let retained = rebuild_carriers(
        previous(),
        WatchRefresh {
            sets: Vec::new(),
            malformed_declarations: malformed_declarations.clone(),
            live_task_ids: BTreeSet::from(["custom-worker-seat".to_owned()]),
        },
        &BTreeMap::new(),
        &mut BTreeMap::new(),
    );
    let entry = &retained[&declaration][0];
    assert_eq!(
        entry.state,
        Some(CarrierState::Present("before".to_owned()))
    );
    assert_eq!(
        entry
            .pending_transition
            .as_ref()
            .map(|pending| pending.new_state.clone()),
        Some(CarrierState::Present("corrected".to_owned()))
    );

    let dropped = rebuild_carriers(
        previous(),
        WatchRefresh {
            sets: Vec::new(),
            malformed_declarations,
            live_task_ids: BTreeSet::new(),
        },
        &BTreeMap::new(),
        &mut BTreeMap::new(),
    );
    assert!(
        dropped.is_empty(),
        "a malformed declaration must not retain a watch after its exact seat is no longer live"
    );
}

#[test]
fn degraded_poll_replays_a_pending_transition_before_newer_bytes() {
    let root = tempfile::tempdir().unwrap();
    let agent_dir = root.path().join("agents/hetz/worker");
    let resources = agent_dir.join("resources");
    std::fs::create_dir_all(&resources).unwrap();
    std::fs::write(
        agent_dir.join("agent.kdl"),
        r#"agent "worker" {
  host "hetz"
  command "agent"
  resource "goal" uri="resources/goal.md" reason="Mission."
}"#,
    )
    .unwrap();
    let carrier = resources.join("goal.md");
    std::fs::write(&carrier, "newer live bytes").unwrap();
    crate::event::publish_owner_binding_for_test(root.path(), "hetz").unwrap();

    let set = watch_set_for(&discover(root.path()), "hetz", &Default::default());
    let mut worker = Worker {
        root: root.path().to_path_buf(),
        this_host: "hetz".to_owned(),
        carriers: BTreeMap::from([(
            carrier.clone(),
            vec![Entry {
                bus_id: "hetz.worker".to_owned(),
                seat_id: set.seat_id.clone(),
                label: "goal".to_owned(),
                class: CarrierClass::Immediate,
                containment_root: None,
                state: Some(CarrierState::Present("old-digest".to_owned())),
                declaration_summary: None,
                occurrence_sequence: 1,
                pending_transition: Some(PendingTransition::new(
                    "goal",
                    &carrier,
                    &CarrierState::Present("old-digest".to_owned()),
                    &CarrierState::Present("pending-target".to_owned()),
                    owner_incarnation(1),
                    1,
                )),
                in_flight: false,
                parked: false,
                dirty: false,
            }],
        )]),
        subscription_sequences: BTreeMap::new(),
        parked_transitions: BTreeMap::new(),
        deadlines: BTreeMap::new(),
        watched: BTreeMap::new(),
        watcher: None,
        emit: Arc::new(EmitQueue::default()),
    };
    let now = Instant::now();
    worker.apply_watch_sets(refresh_for(vec![set]));
    assert!(
        !resources.join("inbox").exists(),
        "degraded polling must schedule rather than emit during refresh"
    );
    worker.flush_due_publishing(now + IMMEDIATE_WINDOW + Duration::from_secs(1));

    let event = std::fs::read_dir(resources.join("inbox"))
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .find(|body| body.contains("stream: resync"))
        .expect("pending transition is replayed");
    assert_eq!(event_field(&event, "old"), "old-digest");
    assert_eq!(event_field(&event, "new"), "pending-targ");
    let entry = &worker.carriers[&carrier][0];
    assert_eq!(
        entry.state,
        Some(CarrierState::Present("pending-target".to_owned()))
    );
    assert!(entry.pending_transition.is_none());
    assert!(
        entry.dirty,
        "newer live bytes are scheduled only after the pending transition completes"
    );
}

#[test]
fn fallback_polling_preserves_the_coalesced_window() {
    let root = tempfile::tempdir().unwrap();
    crate::event::publish_owner_binding_for_test(root.path(), "host").unwrap();
    let carrier = root.path().join("carrier.md");
    std::fs::write(&carrier, "before").unwrap();
    let baseline = read_state(&carrier, None).ok();
    let mut worker = Worker {
        root: root.path().to_path_buf(),
        this_host: "host".to_owned(),
        carriers: BTreeMap::from([(
            carrier.clone(),
            vec![Entry {
                bus_id: "host.missing".to_owned(),
                seat_id: None,
                label: "spec".to_owned(),
                class: CarrierClass::Coalesced,
                containment_root: None,
                state: baseline.clone(),
                declaration_summary: None,
                occurrence_sequence: 0,
                pending_transition: None,
                in_flight: false,
                parked: false,
                dirty: false,
            }],
        )]),
        subscription_sequences: BTreeMap::new(),
        parked_transitions: BTreeMap::new(),
        deadlines: BTreeMap::new(),
        watched: BTreeMap::new(),
        watcher: None,
        emit: Arc::new(EmitQueue::default()),
    };
    std::fs::write(&carrier, "after").unwrap();
    let now = Instant::now();
    worker.apply_watch_sets(refresh_for(vec![AgentWatchSet {
        declaration_path: PathBuf::from("/catalog/missing/agent.kdl"),
        bus_id: "host.missing".to_owned(),
        seat_id: None,
        carriers: vec![WatchableCarrier {
            label: "spec".to_owned(),
            path: carrier.clone(),
            class: CarrierClass::Coalesced,
            containment_root: None,
        }],
        declaration_summary: None,
    }]));

    worker.flush_due_publishing(now + IMMEDIATE_WINDOW + Duration::from_secs(1));
    let entry = &worker.carriers[&carrier][0];
    assert_eq!(entry.state, baseline);
    assert!(entry.pending_transition.is_none(), "coalesced emit ran too early");

    worker.flush_due_publishing(now + COALESCED_WINDOW + Duration::from_secs(1));
    assert!(
        worker.carriers[&carrier][0].pending_transition.is_some(),
        "the coalesced transition must be attempted after its full window"
    );
}

#[test]
fn notify_backend_error_rescans_every_carrier_digest() {
    let root = tempfile::tempdir().unwrap();
    let agent_dir = root.path().join("agents/hetz/worker");
    let resources = agent_dir.join("resources");
    std::fs::create_dir_all(&resources).unwrap();
    std::fs::write(
        agent_dir.join("agent.kdl"),
        r#"agent "worker" {
  host "hetz"
  command "agent"
  resource "goal" uri="resources/goal.md" reason="Mission."
}"#,
    )
    .unwrap();
    let goal = resources.join("goal.md");
    std::fs::write(&goal, "before\n").unwrap();
    crate::event::publish_owner_binding_for_test(root.path(), "hetz").unwrap();

    let mut worker = Worker {
        root: root.path().to_path_buf(),
        this_host: "hetz".to_owned(),
        carriers: BTreeMap::new(),
        subscription_sequences: BTreeMap::new(),
        parked_transitions: BTreeMap::new(),
        deadlines: BTreeMap::new(),
        watched: BTreeMap::new(),
        watcher: None,
        emit: Arc::new(EmitQueue::default()),
    };
    worker.apply_watch_sets(refresh_for(vec![watch_set_for(
        &discover(root.path()),
        "hetz",
        &Default::default(),
    )]));
    std::fs::write(&goal, "after\n").unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    forward_watch_result(&tx, Err(notify::Error::generic("backend dropped events")));
    match rx.recv().unwrap() {
        Msg::Rescan => worker.rescan_all(),
        _ => panic!("a notify backend error must request a full digest rescan"),
    }
    worker.flush_due_publishing(Instant::now() + IMMEDIATE_WINDOW);

    let inbox = resources.join("inbox");
    let events = std::fs::read_dir(inbox)
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1);
    assert!(events[0].contains("stream: resync"));
    assert!(events[0].contains(r#""binding":"goal""#));
}

#[test]
fn registering_another_directory_rediffs_the_untouched_watch_set() {
    // A registration change is not free of the subscriptions it does not name: notify's
    // macOS FSEvents backend stops the one shared stream on every `watch`, purges the
    // device's pending events, and restarts at "since now", so mutations already queued for
    // a directory that stayed in the set are destroyed. The state that leaves behind is a
    // registered watch that will never report a change that already happened, and the model
    // here is exact — the live parents are recorded as covered but never handed to the
    // backend, so no event about them can exist. Only re-diffing the whole set recovers it.
    let root = tempfile::tempdir().unwrap();
    let write_agent = |identity: &str| {
        let agent_dir = root.path().join("agents/alias").join(identity);
        let resources = agent_dir.join("resources");
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::write(
            agent_dir.join("agent.kdl"),
            format!(
                r#"agent "{identity}" {{
  host "alias"
  command "agent"
  resource "goal" uri="resources/goal.md" reason="Mission."
}}"#
            ),
        )
        .unwrap();
        let goal = resources.join("goal.md");
        std::fs::write(&goal, "before\n").unwrap();
        (agent_dir, goal)
    };
    let (live_dir, live_goal) = write_agent("live");
    write_agent("joining");
    crate::event::publish_owner_binding_for_test(root.path(), "alias").unwrap();

    let specs = crate::discover_strict(root.path()).specs;
    let set_for = |identity: &str| {
        let spec = specs
            .iter()
            .find(|spec| spec.path.starts_with(root.path().join("agents/alias").join(identity)))
            .expect("both declarations are valid");
        watch_set_for(spec, "alias", &ResourceProfileRegistry::empty())
    };
    let live_set = set_for("live");
    let joining_set = set_for("joining");

    let (tx, _rx) = channel::<Msg>();
    let mut worker = Worker {
        root: root.path().to_path_buf(),
        this_host: "alias".to_owned(),
        carriers: rebuild_carriers(
            BTreeMap::new(),
            refresh_for(vec![live_set.clone()]),
            &BTreeMap::new(),
            &mut BTreeMap::new(),
        ),
        subscription_sequences: BTreeMap::new(),
        parked_transitions: BTreeMap::new(),
        deadlines: BTreeMap::new(),
        watched: BTreeMap::new(),
        watcher: make_watcher(tx),
        emit: Arc::new(EmitQueue::default()),
    };
    worker.watched = worker
        .carriers
        .keys()
        .filter_map(|path| path.parent())
        .map(|dir| (dir.to_path_buf(), dir_identity(dir)))
        .collect();

    std::fs::write(&live_goal, "changed with no watch able to report it\n").unwrap();

    // The joining seat contributes directories the backend has not seen, so this refresh
    // changes the registration set without touching the live subscription's own paths.
    worker.apply_watch_sets(refresh_for(vec![live_set, joining_set]));
    worker.flush_due_publishing(Instant::now() + IMMEDIATE_WINDOW + Duration::from_secs(1));

    let event = resync_inbox_event(&live_dir);
    assert_eq!(event_field(&event, "binding"), "goal");
    // The joining seat has no inbox at all: its baseline seeded silently, as a new
    // subscription must, so the rescan is not simply emitting for everything it re-reads.
    assert!(
        !root
            .path()
            .join("agents/alias/joining/resources/inbox")
            .exists(),
        "the joining seat seeds its baseline silently"
    );
}

#[cfg(unix)]
#[test]
fn digesting_a_fifo_fails_without_blocking_the_worker() {
    use std::os::unix::ffi::OsStrExt as _;

    let tmp = tempfile::tempdir().unwrap();
    let fifo = tmp.path().join("carrier.fifo");
    let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: the path is NUL-terminated and points into the live temp directory.
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
    assert_eq!(read_state(&fifo, None).unwrap(), CarrierState::Missing);
    assert_eq!(
        read_state(&fifo, Some(tmp.path())).unwrap(),
        CarrierState::Missing,
        "confined carrier reads must reject a FIFO without blocking too"
    );
}

#[test]
fn due_flush_only_clears_subscribers_of_the_due_class() {
    let root = tempfile::tempdir().unwrap();
    let carrier = root.path().join("carrier.md");
    std::fs::write(&carrier, "same bytes").unwrap();
    let state = read_state(&carrier, None).ok();
    let entries = [CarrierClass::Immediate, CarrierClass::Coalesced]
        .into_iter()
        .map(|class| Entry {
            bus_id: "host.worker".to_owned(),
            seat_id: None,
            label: format!("{class:?}"),
            class,
            containment_root: None,
            state: state.clone(),
            declaration_summary: None,
            occurrence_sequence: 0,
            pending_transition: None,
            in_flight: false,
            parked: false,
            dirty: true,
        })
        .collect();
    let now = Instant::now();
    let mut worker = Worker {
        root: root.path().to_path_buf(),
        this_host: "host".to_owned(),
        carriers: BTreeMap::from([(carrier, entries)]),
        subscription_sequences: BTreeMap::new(),
        parked_transitions: BTreeMap::new(),
        deadlines: BTreeMap::from([(CarrierClass::Immediate, now)]),
        watched: BTreeMap::new(),
        watcher: None,
        emit: Arc::new(EmitQueue::default()),
    };

    worker.flush_due_publishing(now);
    let entries = worker.carriers.values().next().unwrap();
    assert!(!entries[0].dirty);
    assert!(entries[1].dirty, "coalesced subscriber must wait for its own deadline");
}

#[cfg(unix)]
#[test]
fn confined_read_refuses_a_symlink_created_after_resolution() {
    let agent_dir = tempfile::tempdir().expect("agent directory");
    let resources = agent_dir.path().join("resources");
    std::fs::create_dir(&resources).expect("resources directory");
    let carrier = resources.join("goal.md");
    std::fs::write(&carrier, "inside").expect("inside carrier");
    assert!(
        matches!(
            read_state(&carrier, Some(agent_dir.path())),
            Ok(CarrierState::Present(_))
        ),
        "ordinary files beneath the admitted root remain readable"
    );

    std::fs::remove_file(&carrier).expect("remove inside carrier");
    let outside = tempfile::NamedTempFile::new().expect("outside carrier");
    std::fs::write(outside.path(), "external").expect("outside bytes");
    std::os::unix::fs::symlink(outside.path(), &carrier)
        .expect("replace absent carrier with an external symlink");
    assert_eq!(
        read_state(&carrier, Some(agent_dir.path())).unwrap(),
        CarrierState::Missing
    );

    std::fs::remove_file(&carrier).expect("remove final symlink");
    std::fs::remove_dir(&resources).expect("remove resources directory");
    let outside_dir = tempfile::tempdir().expect("outside directory");
    std::fs::write(outside_dir.path().join("goal.md"), "external").expect("outside carrier");
    std::os::unix::fs::symlink(outside_dir.path(), &resources)
        .expect("replace absent ancestor with an external symlink");
    assert_eq!(
        read_state(&carrier, Some(agent_dir.path())).unwrap(),
        CarrierState::Missing
    );
}

#[cfg(unix)]
#[test]
fn confined_read_refuses_a_symlinked_confinement_root_ancestor() {
    let temp = tempfile::tempdir().expect("outer directory");
    let real_root = temp.path().join("real/agent");
    std::fs::create_dir_all(&real_root).expect("real agent directory");
    std::fs::write(real_root.join("goal.md"), "outside admitted ancestry")
        .expect("carrier bytes");
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(temp.path().join("real"), &alias)
        .expect("symlinked root ancestor");
    let admitted_root = alias.join("agent");
    assert_eq!(
        read_state(&admitted_root.join("goal.md"), Some(&admitted_root)).unwrap(),
        CarrierState::Missing,
        "every component of the confinement root must be opened without following symlinks"
    );
}
#[test]
fn deletion_and_same_byte_recreation_are_distinct_carrier_transitions() {
    let root = tempfile::tempdir().unwrap();
    let agent_dir = root.path().join("agents/host/worker");
    let resources = agent_dir.join("resources");
    std::fs::create_dir_all(&resources).unwrap();
    std::fs::write(
        agent_dir.join("agent.kdl"),
        r#"agent "worker" {
  host "host"
  command "agent"
  resource "goal" uri="resources/goal.md" reason="Mission."
}"#,
    )
    .unwrap();
    let carrier = resources.join("goal.md");
    std::fs::write(&carrier, "same bytes").unwrap();
    let CarrierState::Present(original_digest) = read_state(&carrier, None).unwrap() else {
        panic!("regular carrier has a digest");
    };
    crate::event::publish_owner_binding_for_test(root.path(), "host").unwrap();
    let set = watch_set_for(&discover(root.path()), "host", &Default::default());
    let mut worker = Worker {
        root: root.path().to_path_buf(),
        this_host: "host".to_owned(),
        carriers: BTreeMap::new(),
        subscription_sequences: BTreeMap::new(),
        parked_transitions: BTreeMap::new(),
        deadlines: BTreeMap::new(),
        watched: BTreeMap::new(),
        watcher: None,
        emit: Arc::new(EmitQueue::default()),
    };
    worker.apply_watch_sets(refresh_for(vec![set]));

    std::fs::remove_file(&carrier).unwrap();
    worker.flush_path_publishing(&carrier, None);
    let deletion = resync_inbox_events(&agent_dir);
    assert_eq!(deletion.len(), 1);
    assert_eq!(
        event_field(&deletion[0], "old"),
        original_digest.chars().take(12).collect::<String>()
    );
    assert_eq!(event_field(&deletion[0], "new"), "missing");
    assert!(event_field(&deletion[0], "occurrence").ends_with(":1"));
    assert_eq!(
        worker.carriers[&carrier][0].state,
        Some(CarrierState::Missing)
    );

    worker.flush_path_publishing(&carrier, None);
    assert_eq!(
        resync_inbox_events(&agent_dir).len(),
        1,
        "repeated missing observations are silent"
    );

    std::fs::write(&carrier, "same bytes").unwrap();
    worker.flush_path_publishing(&carrier, None);
    let events = resync_inbox_events(&agent_dir);
    assert_eq!(
        events.len(),
        1,
        "creation supersedes the tombstone under the binding key"
    );
    let creation = &events[0];
    assert_eq!(event_field(creation, "old"), "missing");
    assert!(event_field(creation, "occurrence").ends_with(":2"));
}

#[test]
#[cfg(unix)]
fn transient_permission_error_retries_without_emitting_a_tombstone() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    let agent_dir = root.path().join("agents/host/worker");
    let resources = agent_dir.join("resources");
    std::fs::create_dir_all(&resources).unwrap();
    std::fs::write(
        agent_dir.join("agent.kdl"),
        r#"agent "worker" {
  host "host"
  command "agent"
  resource "goal" uri="resources/goal.md" reason="Mission."
}"#,
    )
    .unwrap();
    let carrier = resources.join("goal.md");
    std::fs::write(&carrier, "before").unwrap();
    crate::event::publish_owner_binding_for_test(root.path(), "host").unwrap();
    let set = watch_set_for(&discover(root.path()), "host", &Default::default());
    let mut worker = Worker {
        root: root.path().to_path_buf(),
        this_host: "host".to_owned(),
        carriers: BTreeMap::new(),
        subscription_sequences: BTreeMap::new(),
        parked_transitions: BTreeMap::new(),
        deadlines: BTreeMap::new(),
        watched: BTreeMap::new(),
        watcher: None,
        emit: Arc::new(EmitQueue::default()),
    };
    worker.apply_watch_sets(refresh_for(vec![set]));
    let baseline = worker.carriers[&carrier][0].state.clone();

    let original_permissions = std::fs::metadata(&carrier).unwrap().permissions();
    std::fs::set_permissions(&carrier, std::fs::Permissions::from_mode(0)).unwrap();
    worker.flush_path_publishing(&carrier, None);
    let entry = &worker.carriers[&carrier][0];
    assert_eq!(entry.state, baseline);
    assert!(entry.pending_transition.is_none());
    assert!(entry.dirty);
    assert!(!resources.join("inbox").exists());

    std::fs::set_permissions(&carrier, original_permissions).unwrap();
    std::fs::write(&carrier, "after").unwrap();
    worker.flush_due_publishing(Instant::now() + IMMEDIATE_WINDOW + Duration::from_secs(1));
    let event = resync_inbox_event(&agent_dir);
    assert_ne!(event_field(&event, "new"), "missing");
}

#[test]
#[cfg(unix)]
fn initial_transient_read_failure_schedules_a_baseline_retry() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    let agent_dir = root.path().join("agents/host/worker");
    let resources = agent_dir.join("resources");
    std::fs::create_dir_all(&resources).unwrap();
    std::fs::write(
        agent_dir.join("agent.kdl"),
        r#"agent "worker" {
  host "host"
  command "agent"
  resource "goal" uri="resources/goal.md" reason="Mission."
}"#,
    )
    .unwrap();
    let carrier = resources.join("goal.md");
    std::fs::write(&carrier, "baseline").unwrap();
    let original_permissions = std::fs::metadata(&carrier).unwrap().permissions();
    std::fs::set_permissions(&carrier, std::fs::Permissions::from_mode(0)).unwrap();
    let set = watch_set_for(&discover(root.path()), "host", &Default::default());
    let mut worker = Worker {
        root: root.path().to_path_buf(),
        this_host: "host".to_owned(),
        carriers: BTreeMap::new(),
        subscription_sequences: BTreeMap::new(),
        parked_transitions: BTreeMap::new(),
        deadlines: BTreeMap::new(),
        watched: BTreeMap::from([(resources.clone(), dir_identity(&resources))]),
        watcher: None,
        emit: Arc::new(EmitQueue::default()),
    };

    worker.apply_watch_sets(refresh_for(vec![set]));
    let entry = worker.carriers[&carrier]
        .iter()
        .find(|entry| entry.label == "goal")
        .unwrap();
    assert_eq!(entry.state, None);
    assert!(entry.dirty);
    assert!(worker.deadlines.contains_key(&CarrierClass::Immediate));

    std::fs::set_permissions(&carrier, original_permissions).unwrap();
    worker.flush_due_publishing(Instant::now() + IMMEDIATE_WINDOW + Duration::from_secs(1));
    let entry = worker.carriers[&carrier]
        .iter()
        .find(|entry| entry.label == "goal")
        .unwrap();
    assert!(matches!(entry.state, Some(CarrierState::Present(_))));
    assert!(!entry.dirty);
    assert!(!resources.join("inbox").exists());
}

#[test]
fn reinstalled_subscription_keeps_occurrence_identity_without_an_active_watch() {
    let root = tempfile::tempdir().unwrap();
    let agent_dir = root.path().join("agents/host/worker");
    let resources = agent_dir.join("resources");
    std::fs::create_dir_all(&resources).unwrap();
    std::fs::write(
        agent_dir.join("agent.kdl"),
        r#"agent "worker" {
  host "host"
  command "agent"
  resource "goal" uri="resources/goal.md" reason="Mission."
}"#,
    )
    .unwrap();
    let carrier = resources.join("goal.md");
    std::fs::write(&carrier, "A").unwrap();
    crate::event::publish_owner_binding_for_test(root.path(), "host").unwrap();
    let set = watch_set_for(&discover(root.path()), "host", &Default::default());
    let seen_subscription_count = set.carriers.len();
    let mut worker = Worker {
        root: root.path().to_path_buf(),
        this_host: "host".to_owned(),
        carriers: BTreeMap::new(),
        subscription_sequences: BTreeMap::new(),
        parked_transitions: BTreeMap::new(),
        deadlines: BTreeMap::new(),
        watched: BTreeMap::new(),
        watcher: None,
        emit: Arc::new(EmitQueue::default()),
    };
    worker.apply_watch_sets(refresh_for(vec![set.clone()]));

    std::fs::write(&carrier, "B").unwrap();
    worker.flush_path_publishing(&carrier, None);
    let before_suspend = resync_inbox_event(&agent_dir);

    // Suspension removes every carrier and watch, while retaining one scalar sequence floor
    // for each declaration/binding identity seen during this supervisor incarnation.
    worker
        .watched
        .insert(resources.clone(), dir_identity(&resources));
    worker.apply_watch_sets(refresh_for(Vec::new()));
    assert!(worker.carriers.is_empty());
    assert!(worker.watched.is_empty());
    assert_eq!(
        worker.subscription_sequences.len(),
        seen_subscription_count
    );

    std::fs::write(&carrier, "A").unwrap();
    worker.apply_watch_sets(refresh_for(vec![set]));
    let resumed = worker.carriers[&carrier]
        .iter()
        .find(|entry| entry.label == "goal")
        .unwrap();
    assert_eq!(resumed.occurrence_sequence, 1);
    std::fs::write(&carrier, "B").unwrap();
    worker.flush_path_publishing(&carrier, None);
    let after_resume = resync_inbox_event(&agent_dir);

    assert_eq!(
        event_field(&before_suspend, "old"),
        event_field(&after_resume, "old")
    );
    assert_eq!(
        event_field(&before_suspend, "new"),
        event_field(&after_resume, "new")
    );
    assert_ne!(
        event_field(&before_suspend, "event-id"),
        event_field(&after_resume, "event-id"),
        "the post-resume A→B occurrence must not deduplicate against the pre-suspend one"
    );
    assert!(event_field(&before_suspend, "occurrence").ends_with(":1"));
    assert!(event_field(&after_resume, "occurrence").ends_with(":2"));
}

#[test]
fn relocated_subscription_keeps_occurrence_sequence_in_the_recipient_namespace() {
    let root = tempfile::tempdir().unwrap();
    let agent_dir = root.path().join("agents/host/worker");
    let resources = agent_dir.join("resources");
    std::fs::create_dir_all(&resources).unwrap();
    std::fs::write(
        agent_dir.join("agent.kdl"),
        r#"agent "worker" {
  host "host"
  command "agent"
  resource "goal" uri="resources/goal.md" reason="Mission."
}"#,
    )
    .unwrap();
    let original_carrier = resources.join("goal.md");
    let relocated_carrier = resources.join("relocated-goal.md");
    std::fs::write(&original_carrier, "A").unwrap();
    crate::event::publish_owner_binding_for_test(root.path(), "host").unwrap();
    let set =
        watch_set_for(&discover(root.path()), "host", &ResourceProfileRegistry::empty());
    let mut worker = Worker {
        root: root.path().to_path_buf(),
        this_host: "host".to_owned(),
        carriers: BTreeMap::new(),
        subscription_sequences: BTreeMap::new(),
        parked_transitions: BTreeMap::new(),
        deadlines: BTreeMap::new(),
        watched: BTreeMap::new(),
        watcher: None,
        emit: Arc::new(EmitQueue::default()),
    };
    worker.apply_watch_sets(refresh_for(vec![set.clone()]));

    std::fs::write(&original_carrier, "B").unwrap();
    worker.flush_path_publishing(&original_carrier, None);
    let first_a_to_b = resync_inbox_event(&agent_dir);

    std::fs::write(&relocated_carrier, "A").unwrap();
    let mut relocated = set;
    relocated.declaration_path = agent_dir.join("relocated/agent.kdl");
    for carrier in &mut relocated.carriers {
        if carrier.label == "declaration" {
            carrier.path = relocated.declaration_path.clone();
        } else if carrier.label == "goal" {
            carrier.path = relocated_carrier.clone();
        }
    }
    worker.apply_watch_sets(refresh_for(vec![relocated.clone()]));
    let rebound = worker.carriers[&relocated_carrier]
        .iter()
        .find(|entry| entry.label == "goal")
        .unwrap();
    assert_eq!(rebound.occurrence_sequence, 1);
    assert_eq!(
        rebound.state,
        read_state(&original_carrier, None).ok()
    );

    worker.flush_path_publishing(&relocated_carrier, None);
    let back_to_a = resync_inbox_event(&agent_dir);
    assert_eq!(event_field(&back_to_a, "old"), event_field(&first_a_to_b, "new"));
    assert_eq!(event_field(&back_to_a, "new"), event_field(&first_a_to_b, "old"));
    assert!(event_field(&back_to_a, "occurrence").ends_with(":2"));

    std::fs::write(&relocated_carrier, "B").unwrap();
    worker.flush_path_publishing(&relocated_carrier, None);
    let second_a_to_b = resync_inbox_event(&agent_dir);
    assert_eq!(
        event_field(&first_a_to_b, "old"),
        event_field(&second_a_to_b, "old")
    );
    assert_eq!(
        event_field(&first_a_to_b, "new"),
        event_field(&second_a_to_b, "new")
    );
    assert_ne!(
        event_field(&first_a_to_b, "event-id"),
        event_field(&second_a_to_b, "event-id")
    );
    assert!(event_field(&first_a_to_b, "occurrence").ends_with(":1"));
    assert!(event_field(&second_a_to_b, "occurrence").ends_with(":3"));

    relocated.bus_id = "host.replacement".to_owned();
    worker.apply_watch_sets(refresh_for(vec![relocated]));
    assert_eq!(
        worker.carriers[&relocated_carrier]
            .iter()
            .find(|entry| entry.label == "goal")
            .unwrap()
            .occurrence_sequence,
        0,
        "a different recipient starts a distinct deduplication namespace"
    );
}

#[test]
fn subscribers_advance_occurrence_sequences_independently() {
    let root = tempfile::tempdir().unwrap();
    let carrier = root.path().join("shared.md");
    std::fs::write(&carrier, "new bytes").unwrap();
    crate::event::publish_owner_binding_for_test(root.path(), "host").unwrap();
    let entries = ["host.alpha", "host.beta"]
        .into_iter()
        .map(|bus_id| Entry {
            bus_id: bus_id.to_owned(),
            seat_id: None,
            label: "goal".to_owned(),
            class: CarrierClass::Immediate,
            containment_root: None,
            state: Some(CarrierState::Present("old-digest".to_owned())),
            declaration_summary: None,
            occurrence_sequence: 0,
            pending_transition: None,
            in_flight: false,
            parked: false,
            dirty: true,
        })
        .collect();
    let mut worker = Worker {
        root: root.path().to_path_buf(),
        this_host: "host".to_owned(),
        carriers: BTreeMap::from([(carrier.clone(), entries)]),
        subscription_sequences: BTreeMap::new(),
        parked_transitions: BTreeMap::new(),
        deadlines: BTreeMap::new(),
        watched: BTreeMap::new(),
        watcher: None,
        emit: Arc::new(EmitQueue::default()),
    };

    worker.flush_path_publishing(&carrier, None);

    let entries = &worker.carriers[&carrier];
    assert_eq!(entries[0].occurrence_sequence, 1);
    assert_eq!(entries[1].occurrence_sequence, 1);
    assert_eq!(
        event_field(&entries[0].pending_transition.as_ref().unwrap().body, "occurrence"),
        event_field(&entries[1].pending_transition.as_ref().unwrap().body, "occurrence"),
        "one subscriber must not consume sequence numbers from another"
    );
}

#[test]
fn supervisor_restart_incarnation_changes_the_occurrence_namespace() {
    let first = PendingTransition::new(
        "goal",
        Path::new("/agent/goal.md"),
        &CarrierState::Present("old".to_owned()),
        &CarrierState::Present("new".to_owned()),
        owner_incarnation(1),
        1,
    );
    let restarted = PendingTransition::new(
        "goal",
        Path::new("/agent/goal.md"),
        &CarrierState::Present("old".to_owned()),
        &CarrierState::Present("new".to_owned()),
        owner_incarnation(2),
        1,
    );

    assert_ne!(first.body, restarted.body);
    assert_ne!(first.event_id, restarted.event_id);
}

#[test]
fn failed_tombstone_emit_retains_present_state_and_immutable_retry_snapshot() {
    let root = tempfile::tempdir().unwrap();
    crate::event::publish_owner_binding_for_test(root.path(), "host").unwrap();
    let carrier = root.path().join("carrier.md");
    std::fs::write(&carrier, "old bytes").unwrap();
    let mut worker = Worker {
        root: root.path().to_path_buf(),
        this_host: "host".to_owned(),
        carriers: BTreeMap::from([(
            carrier.clone(),
            vec![Entry {
                bus_id: "host.missing".to_owned(),
                seat_id: None,
                label: "goal".to_owned(),
                class: CarrierClass::Immediate,
                containment_root: None,
                state: Some(CarrierState::Present("old-digest".to_owned())),
                declaration_summary: None,
                occurrence_sequence: 0,
                pending_transition: None,
                in_flight: false,
                parked: false,
                dirty: true,
            }],
        )]),
        subscription_sequences: BTreeMap::new(),
        parked_transitions: BTreeMap::new(),
        deadlines: BTreeMap::new(),
        watched: BTreeMap::new(),
        watcher: None,
        emit: Arc::new(EmitQueue::default()),
    };
    std::fs::remove_file(&carrier).unwrap();

    worker.flush_path_publishing(&carrier, None);
    let pending_transition = worker.carriers[&carrier][0]
        .pending_transition
        .clone()
        .expect("failed transition snapshot is retained");
    assert_eq!(pending_transition.new_state, CarrierState::Missing);
    assert_eq!(event_field(&pending_transition.body, "old"), "old-digest");
    assert_eq!(event_field(&pending_transition.body, "new"), "missing");
    assert_eq!(worker.carriers[&carrier][0].occurrence_sequence, 1);
    std::fs::write(&carrier, "old bytes").unwrap();
    worker.flush_path_publishing(&carrier, None);
    let entry = &worker.carriers[&carrier][0];
    assert_eq!(entry.occurrence_sequence, 1);
    assert_eq!(
        entry.state,
        Some(CarrierState::Present("old-digest".to_owned()))
    );
    assert_eq!(
        entry.pending_transition.as_ref(),
        Some(&pending_transition),
        "a retry must replay the tombstone snapshot even after the carrier is recreated"
    );
    assert!(entry.dirty);
    assert!(worker.deadlines.contains_key(&CarrierClass::Immediate));
}

fn handoff_worker(root: &Path, carrier: &Path, recipients: &[&str]) -> Worker {
    crate::event::publish_owner_binding_for_test(root, "host").unwrap();
    std::fs::write(carrier, "current bytes").unwrap();
    Worker {
        root: root.to_path_buf(),
        this_host: "host".to_owned(),
        carriers: BTreeMap::from([(
            carrier.to_path_buf(),
            recipients
                .iter()
                .map(|bus_id| Entry {
                    bus_id: (*bus_id).to_owned(),
                    seat_id: None,
                    label: "goal".to_owned(),
                    class: CarrierClass::Immediate,
                    containment_root: None,
                    state: Some(CarrierState::Present("stale-digest".to_owned())),
                    declaration_summary: None,
                    occurrence_sequence: 0,
                    pending_transition: None,
                    in_flight: false,
                    parked: false,
                    dirty: true,
                })
                .collect(),
        )]),
        subscription_sequences: BTreeMap::new(),
        parked_transitions: BTreeMap::new(),
        deadlines: BTreeMap::new(),
        watched: BTreeMap::new(),
        watcher: None,
        emit: Arc::new(EmitQueue::default()),
    }
}

/// One agent declaration plus its goal carrier, and a subscription to that carrier whose
/// baseline is stale, so the next flush captures exactly one transition for it.
///
/// A subscription exists only for a canonical seat a pass proved alive, so a subscription
/// addressed to a declaration that says `suspended` IS the suspended-and-running state — the
/// one that produced ~2010 refusals per seat in #431. A seat whose task had already exited
/// produces no subscription at all, which is why a fixture built on one proves nothing.
fn declared_recipient_worker(root: &Path, identity: &str, declaration_extra: &str) -> Worker {
    crate::event::publish_owner_binding_for_test(root, "host").unwrap();
    let agent_dir = root.join("agents/host").join(identity);
    std::fs::create_dir_all(agent_dir.join("resources")).unwrap();
    std::fs::write(
        agent_dir.join("agent.kdl"),
        format!(
            "agent \"{identity}\" {{\n  host \"host\"\n  command \"agent\"\n{declaration_extra}  resource \"goal\" uri=\"resources/goal.md\" reason=\"Mission.\"\n}}"
        ),
    )
    .unwrap();
    let goal = agent_dir.join("resources/goal.md");
    std::fs::write(&goal, "current bytes").unwrap();
    Worker {
        root: root.to_path_buf(),
        this_host: "host".to_owned(),
        carriers: BTreeMap::from([(
            goal,
            vec![Entry {
                bus_id: format!("host.{identity}"),
                seat_id: None,
                label: "goal".to_owned(),
                class: CarrierClass::Immediate,
                containment_root: None,
                state: Some(CarrierState::Present("stale-digest".to_owned())),
                declaration_summary: None,
                occurrence_sequence: 0,
                pending_transition: None,
                in_flight: false,
                parked: false,
                dirty: true,
            }],
        )]),
        subscription_sequences: BTreeMap::new(),
        parked_transitions: BTreeMap::new(),
        deadlines: BTreeMap::new(),
        watched: BTreeMap::new(),
        watcher: None,
        emit: Arc::new(EmitQueue::default()),
    }
}

/// A recipient that refuses because it is not running keeps its reservation and is attempted
/// exactly once. Retrying it cannot make it admissible: eligibility was resolved under the
/// catalog-authoring lock, and each attempt re-resolves the whole catalog to reach the same
/// answer. That retry is the CPU burn and the shared-lock coverage of #431.
#[test]
fn a_not_running_recipient_parks_its_reservation_and_is_attempted_once() {
    let root = tempfile::tempdir().unwrap();
    let mut worker = declared_recipient_worker(
        root.path(),
        "worker",
        "  desired-state \"suspended\" reason=\"Waiting for capacity\"\n",
    );
    let goal = worker.carriers.keys().next().unwrap().clone();

    worker.flush_path_publishing(&goal, None);

    let entry = &worker.carriers[&goal][0];
    assert!(entry.parked, "the subscription must be parked");
    assert!(
        entry.pending_transition.is_none(),
        "the reservation moves out of the subscription, which a refresh will drop"
    );
    assert!(
        worker
            .parked_transitions
            .contains_key(&("host.worker".to_owned(), "goal".to_owned())),
        "the reservation must be retained, not dropped: {:?}",
        worker.parked_transitions.keys().collect::<Vec<_>>()
    );
    assert!(
        worker.deadlines.is_empty(),
        "a not-running recipient must schedule no retry deadline: {:?}",
        worker.deadlines
    );

    // Each source that would ordinarily re-arm this carrier, checked on its own: a parked
    // subscription must schedule nothing, not merely publish nothing.
    std::fs::write(&goal, "changed while the recipient is suspended\n").unwrap();
    worker.mark_mutated(vec![goal.clone()]);
    assert!(
        worker.deadlines.is_empty() && !worker.carriers[&goal][0].dirty,
        "a mutation wakeup must not schedule a parked subscription: {:?}",
        worker.deadlines
    );
    worker.rescan_all();
    assert!(
        worker.deadlines.is_empty() && !worker.carriers[&goal][0].dirty,
        "a rescan must not schedule a parked subscription: {:?}",
        worker.deadlines
    );
    worker.flush_path_publishing(&goal, None);
    worker.flush_due_publishing(Instant::now() + COALESCED_WINDOW + Duration::from_secs(1));
    assert!(worker.deadlines.is_empty(), "{:?}", worker.deadlines);
    assert_eq!(
        worker.emit.handed_off(),
        1,
        "a parked reservation must not be attempted again"
    );
}

/// The reservation re-arms when its recipient is running again, and replays the exact bytes
/// it reserved. Dropping it at the refusal would lose a resync the agent should see on
/// resume; a refresh drops the suspended subscription itself, so the reservation has to
/// outlive it.
#[test]
fn a_parked_reservation_re_arms_and_replays_when_its_recipient_runs_again() {
    let root = tempfile::tempdir().unwrap();
    let mut worker = declared_recipient_worker(
        root.path(),
        "worker",
        "  desired-state \"suspended\" reason=\"Waiting for capacity\"\n",
    );
    let goal = worker.carriers.keys().next().unwrap().clone();
    let agent_dir = root.path().join("agents/host/worker");

    worker.flush_path_publishing(&goal, None);
    let reserved = worker
        .parked_transitions
        .values()
        .next()
        .expect("the refusal retains its reservation")
        .clone();

    std::fs::write(
        agent_dir.join("agent.kdl"),
        "agent \"worker\" {\n  host \"host\"\n  command \"agent\"\n  resource \"goal\" uri=\"resources/goal.md\" reason=\"Mission.\"\n}",
    )
    .unwrap();
    let resumed = watch_set_for(
        &discover(root.path()),
        "host",
        &ResourceProfileRegistry::empty(),
    );
    worker.apply_watch_sets(refresh_for(vec![resumed]));

    let entry = &worker.carriers[&goal][0];
    assert!(!entry.parked, "a carried recipient is running");
    assert_eq!(
        entry.pending_transition.as_ref().map(|held| &held.event_id),
        Some(&reserved.event_id),
        "the restored reservation must be the reserved one"
    );
    assert!(
        worker.deadlines.contains_key(&CarrierClass::Immediate),
        "re-arming must schedule the carrier's class: {:?}",
        worker.deadlines
    );
    assert!(worker.parked_transitions.is_empty());

    worker.flush_due_publishing(Instant::now() + IMMEDIATE_WINDOW + Duration::from_secs(1));
    let event = resync_inbox_event(&agent_dir);
    assert!(
        event.contains(&format!("event-id: {}", reserved.event_id)),
        "{event}"
    );
    assert!(event.contains(&reserved.body), "{event}");
}

/// A refusal no retry can admit drops its reservation and advances the baseline, so the same
/// carrier transition is not captured again on the next observation. Without advancing it,
/// "drop" would only mean "re-capture and refuse again".
#[test]
fn a_permanently_refused_reservation_is_dropped_and_not_recaptured() {
    let root = tempfile::tempdir().unwrap();
    let mut worker = declared_recipient_worker(root.path(), "worker", "");
    // A second declaration of the same identity makes the recipient ambiguous. No retry can
    // resolve that from the publisher's side.
    let twin = root.path().join("agents/host/worker-copy");
    std::fs::create_dir_all(&twin).unwrap();
    std::fs::write(
        twin.join("agent.kdl"),
        "agent \"worker\" {\n  host \"host\"\n  command \"agent\"\n}",
    )
    .unwrap();
    let goal = worker.carriers.keys().next().unwrap().clone();

    worker.flush_path_publishing(&goal, None);

    let entry = &worker.carriers[&goal][0];
    assert!(
        entry.pending_transition.is_none(),
        "a permanent refusal drops its reservation"
    );
    assert!(!entry.parked, "a permanent refusal is not parked");
    assert!(worker.parked_transitions.is_empty());
    assert!(
        worker.deadlines.is_empty(),
        "a permanent refusal must schedule no retry: {:?}",
        worker.deadlines
    );

    worker.rescan_all();
    worker.flush_due_publishing(Instant::now() + COALESCED_WINDOW + Duration::from_secs(1));
    assert_eq!(
        worker.emit.handed_off(),
        1,
        "the dropped transition must not be recaptured"
    );
}

/// Every refusal the catalog can return, classified. `RecipientNotRunning` is the only one a
/// resume can admit; the rest never become admissible. An unknown recipient and a catalog
/// mid-edit stay retryable on purpose: a declaration being replaced by rename is briefly
/// absent, and dropping the reservation there would lose a resync nothing was wrong with.
#[test]
fn refusals_are_classified_by_what_could_admit_them_later() {
    use crate::event::{RefusalKind, refusal_kind};

    let root = tempfile::tempdir().unwrap();
    crate::event::publish_owner_binding_for_test(root.path(), "host").unwrap();
    let write = |identity: &str, body: &str| {
        let dir = root.path().join("agents/host").join(identity);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agent.kdl"), body).unwrap();
    };
    let emit = |recipient: &str| {
        crate::event::emit_builtin_resync(
            root.path(),
            "host",
            recipient,
            "eventid",
            Some("goal"),
            Some("subject"),
            "{}",
            true,
        )
        .expect_err("every recipient in this fixture refuses")
    };

    write(
        "suspended",
        "agent \"suspended\" {\n  host \"host\"\n  command \"agent\"\n  desired-state \"suspended\" reason=\"Waiting\"\n}",
    );
    write(
        "foreign",
        "agent \"foreign\" {\n  host \"elsewhere\"\n  command \"agent\"\n}",
    );
    write("plain", "agent \"plain\" {\n  host \"host\"\n  command \"agent\"\n}");
    write(
        "twin",
        "agent \"plain\" {\n  host \"host\"\n  command \"agent\"\n}",
    );
    write("solo", "agent \"solo\" {\n  host \"host\"\n  command \"agent\"\n}");

    let suspended = emit("host.suspended");
    assert!(
        suspended.to_string().contains("eyes are closed"),
        "{suspended:#}"
    );
    assert_eq!(
        refusal_kind(&suspended),
        Some(RefusalKind::RecipientNotRunning)
    );

    // A foreign spec is only reachable under its own host-qualified bus id; by local identity
    // it is simply not in this host's namespace.
    let foreign = emit("elsewhere.foreign");
    assert!(
        foreign
            .to_string()
            .contains("event publication must run on that host"),
        "{foreign:#}"
    );
    assert_eq!(refusal_kind(&foreign), Some(RefusalKind::Permanent));

    let ambiguous = emit("host.plain");
    assert!(ambiguous.to_string().contains("is ambiguous"), "{ambiguous:#}");
    assert_eq!(refusal_kind(&ambiguous), Some(RefusalKind::Permanent));

    let undeclared = crate::event::emit(
        root.path(),
        "host",
        "host.solo",
        "other",
        "eventid",
        None,
        None,
        "{}",
        true,
    )
    .expect_err("an undeclared stream is refused");
    assert_eq!(refusal_kind(&undeclared), Some(RefusalKind::Permanent));

    let unknown = emit("host.absent");
    assert!(unknown.to_string().contains("no agent"), "{unknown:#}");
    assert_eq!(
        refusal_kind(&unknown),
        None,
        "an absent declaration may be a rename in flight, so it stays retryable"
    );
}

/// One outstanding publication per subscription. Flushing again while a publication is still
/// in flight must not hand off the same subscription twice: whether its reserved event
/// identity is spent is decided by an outcome that has not returned yet.
#[test]
fn a_flush_never_hands_off_a_subscription_whose_publication_is_outstanding() {
    let root = tempfile::tempdir().unwrap();
    let carrier = root.path().join("carrier.md");
    let mut worker = handoff_worker(root.path(), &carrier, &["host.missing"]);

    worker.flush_path(&carrier, None);
    assert_eq!(worker.emit.queued_recipients(), ["host.missing"]);
    assert!(worker.carriers[&carrier][0].in_flight);

    std::fs::write(&carrier, "newer bytes while the publication is outstanding").unwrap();
    worker.mark_mutated(vec![carrier.clone()]);
    worker.flush_path(&carrier, None);
    assert_eq!(
        worker.emit.queued_recipients(),
        ["host.missing"],
        "an outstanding publication must not be handed off a second time"
    );
}

/// A deactivation acknowledgement is the reconcile pass's guarantee that nothing starts
/// publishing to that recipient afterwards. A publication still queued when the seat is
/// deactivated is dropped, and only that recipient's.
#[test]
fn deactivation_drops_only_that_recipients_queued_publication() {
    let root = tempfile::tempdir().unwrap();
    let carrier = root.path().join("carrier.md");
    let mut worker = handoff_worker(root.path(), &carrier, &["host.leaving", "host.staying"]);

    worker.flush_path(&carrier, None);
    assert_eq!(
        worker.emit.queued_recipients(),
        ["host.leaving", "host.staying"]
    );

    worker.deactivate_watch_set("host.leaving");
    assert_eq!(worker.emit.queued_recipients(), ["host.staying"]);
}

/// A refresh that drops a subscription — the suspended recipient of #431 — must drop its
/// queued publication too. The pass has already decided that seat receives no events, and a
/// queued refusal published afterwards is work no outcome can apply.
#[test]
fn a_refresh_drops_a_queued_publication_for_a_subscription_it_removed() {
    let root = tempfile::tempdir().unwrap();
    let carrier = root.path().join("carrier.md");
    let mut worker = handoff_worker(root.path(), &carrier, &["host.missing"]);

    worker.flush_path(&carrier, None);
    assert_eq!(worker.emit.queued_recipients(), ["host.missing"]);

    worker.apply_watch_sets(refresh_for(Vec::new()));
    assert!(worker.carriers.is_empty());
    assert!(
        worker.emit.queued_recipients().is_empty(),
        "a dropped subscription's queued publication must not survive the refresh"
    );
}

#[test]
fn transition_identity_covers_every_rendered_transition_dimension() {
    let topics = vec!["content".to_owned()];
    let facts =
        vec![ResourceFact::transition("digest", Some("old"), Some("new")).unwrap()];
    let baseline = render_body("goal", &topics, &facts, "v1:1:2:42:3:1");
    assert_eq!(
        transition_identity(&baseline),
        transition_identity(&baseline),
        "replaying one canonical body must reproduce its identity"
    );

    let changed_facts =
        vec![ResourceFact::transition("digest", Some("old"), Some("other")).unwrap()];
    for (dimension, changed) in [
        (
            "binding",
            render_body("spec", &topics, &facts, "v1:1:2:42:3:1"),
        ),
        (
            "topic",
            render_body("goal", &["other".to_owned()], &facts, "v1:1:2:42:3:1"),
        ),
        (
            "fact",
            render_body("goal", &topics, &changed_facts, "v1:1:2:42:3:1"),
        ),
        (
            "occurrence",
            render_body("goal", &topics, &facts, "v1:1:2:42:3:2"),
        ),
    ] {
        assert_ne!(
            transition_identity(&baseline),
            transition_identity(&changed),
            "changing {dimension} must change the event identity"
        );
    }
}

#[test]
fn local_path_resolution_parses_supported_file_uris_without_uri_metadata_bytes() {
    let agent_dir = Path::new("/cat/agents/hetz/w");
    assert_eq!(
        resolve_local_path(agent_dir, "file:///etc/demo.kdl"),
        Some(PathBuf::from("/etc/demo.kdl"))
    );
    for scheme in ["file", "FILE", "FiLe"] {
        assert_eq!(
            resolve_local_path(agent_dir, &format!("{scheme}:///etc/demo.kdl")),
            Some(PathBuf::from("/etc/demo.kdl"))
        );
    }
    assert_eq!(
        resolve_local_path(agent_dir, "file:///tmp/with%20space/%E2%82%AC.md"),
        Some(PathBuf::from("/tmp/with space/€.md"))
    );
    assert_eq!(
        resolve_local_path(agent_dir, "file:///tmp/literal%3Fmark"),
        Some(PathBuf::from("/tmp/literal?mark"))
    );
    assert_eq!(
        resolve_local_path(agent_dir, "file:///"),
        Some(PathBuf::from("/"))
    );
    assert_eq!(
        resolve_local_path(agent_dir, "resources/journal.md"),
        Some(agent_dir.join("resources/journal.md"))
    );
    assert_eq!(
        resolve_local_path(
            agent_dir,
            "resources/with%20space/%E2%82%AC-journal.md"
        ),
        Some(agent_dir.join("resources/with space/€-journal.md"))
    );

    for unsupported in [
        "file://authority/etc/demo.kdl",
        "file:////authority/etc/demo.kdl",
        "file:///etc/demo.kdl?revision=2",
        "file:///etc/demo.kdl#section",
        "file:///tmp/encoded%2Fseparator",
        "file:///tmp/encoded%2fseparator",
        "file:///tmp/encoded%5Cseparator",
        "file:///tmp/bad%escape",
        "file:///tmp/%2E%2E/escape",
        "file:///tmp/a%00b",
        "file:///tmp/%FF.md",
        "resources/encoded%2Fseparator",
        "resources/encoded%5Cseparator",
        "resources/%2E%2E/outside.md",
        "resources/bad%escape",
        "resources/a%00b",
        "resources/%FF.md",
        "http://x/y",
        "worktree://repo/main",
        "GitHub-Issue://org/repo/41",
        "ProFiLe:opaque",
    ] {
        assert_eq!(
            resolve_local_path(agent_dir, unsupported),
            None,
            "{unsupported} must not become filesystem bytes"
        );
    }
}

#[test]
fn classification_is_goal_immediate_stores_silent_other_coalesced() {
    let agent_dir = Path::new("/cat/agents/hetz/w");
    assert_eq!(
        classify(agent_dir, "mission", &agent_dir.join("resources/goal.md")),
        Some(CarrierClass::Immediate),
        "basename goal.md is immediate regardless of binding name"
    );
    assert_eq!(
        classify(
            agent_dir,
            "journal",
            &agent_dir.join("resources/context/journal.md")
        ),
        None,
        "agent-authored stores are silent and excluded from the watch set"
    );
    assert_eq!(
        classify(
            agent_dir,
            "decision-log",
            &agent_dir.join("resources/decisions/x.md")
        ),
        None
    );
    assert_eq!(
        classify(agent_dir, "spec", Path::new("/etc/demo/spec.md")),
        Some(CarrierClass::Coalesced)
    );
}

#[test]
fn profile_classes_map_onto_carrier_notification() {
    assert_eq!(carrier_class(ProfileClass::Immediate), Some(CarrierClass::Immediate));
    assert_eq!(carrier_class(ProfileClass::Coalesced), Some(CarrierClass::Coalesced));
    assert_eq!(
        carrier_class(ProfileClass::Silent),
        None,
        "silent profiles are excluded from the watch set like sniffed authored stores"
    );
}

#[test]
fn registered_profile_failures_are_reported_while_other_bindings_survive() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("agents/hetz/worker");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("agent.kdl"),
        r#"agent "worker" {
  host "hetz"
  command "true"
  resource "goal" uri="dev.schickling.agent-goal://hetz/worker" reason="Mission."
  resource "issue" uri="worktree://repo/main" reason="Opaque scheme."
}"#,
    )
    .unwrap();

    let broken = tmp.path().join("broken.wasm");
    std::fs::write(&broken, b"not a module").unwrap();
    let profiles = ResourceProfileRegistry::empty().with_profile(
        agent_spec::ResourceProfile::wasm(
            "dev.schickling.agent-goal",
            &broken,
            ProfileClass::Coalesced,
        ),
    );
    let refresh = profiles.begin_refresh();
    let spec = discover(tmp.path());
    let (set, diagnostics) =
        resolve_watch_set(&spec, std::slice::from_ref(&spec), "hetz", &refresh);
    assert!(!set.carriers.iter().any(|c| c.label == "goal"));
    assert!(set.carriers.iter().any(|c| c.label == "declaration"));
    assert!(!set.carriers.iter().any(|c| c.label == "issue"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].contains("resource 'goal'"));
    assert!(diagnostics[0].contains("unwatchable"));
    assert_eq!(
        resource_coverage_with_profiles(&dir, &spec.resources[0], &refresh),
        ResyncCoverage::Unsupported
    );
}

#[test]
fn silent_profile_skips_its_resolver_entirely() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("agents/hetz/worker");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("agent.kdl"),
        r#"agent "worker" {
  host "hetz"
  command "true"
  resource "goal" uri="dev.schickling.agent-goal://hetz/worker" reason="Mission."
}"#,
    )
    .unwrap();
    let missing = tmp.path().join("must-not-load.wasm");
    let profiles = ResourceProfileRegistry::empty().with_profile(
        agent_spec::ResourceProfile::wasm(
            "dev.schickling.agent-goal",
            missing,
            ProfileClass::Silent,
        ),
    );
    let refresh = profiles.begin_refresh();
    let spec = discover(tmp.path());
    let (set, diagnostics) =
        resolve_watch_set(&spec, std::slice::from_ref(&spec), "hetz", &refresh);
    assert!(!set.carriers.iter().any(|carrier| carrier.label == "goal"));
    assert!(
        diagnostics.is_empty(),
        "a silent profile must not execute its missing resolver: {diagnostics:?}"
    );
    assert_eq!(
        resource_coverage_with_profiles(&dir, &spec.resources[0], &refresh),
        ResyncCoverage::Silent
    );
}
