use super::owned_sets::{Options, Source};
use super::*;
use crate::parse_intent;

fn bundle(command: &str, meadow: bool) -> NormalizedIntent {
    parse_intent(
        &format!(
            "version 2\nagent \"garden/orchard\" {{ command {command:?} }}\n{}",
            if meadow {
                "agent \"garden/meadow\" { command \"true\" }"
            } else {
                ""
            }
        ),
        "amber",
    )
    .unwrap()
}
fn options(store: &Store, sequence: u64) -> Options {
    let current = store
        .owned_sets()
        .unwrap()
        .into_iter()
        .find(|s| s.id == "owned-set/garden");
    Options {
        set: "garden".into(),
        source: Source {
            repository: "acme/garden".into(),
            r#ref: "refs/heads/main".into(),
            sha: format!("{sequence:040x}"),
            sequence,
        },
        expected_set: current.map_or("absent".into(), |s| s.revision),
        rollout: None,
        adopt: Default::default(),
        allow_empty: false,
        confirm_retire: None,
        expected_subjects: Default::default(),
    }
}
fn apply(store: &Store, input: &NormalizedIntent, sequence: u64) -> ApplyResponse {
    let mut opts = options(store, sequence);
    opts.expected_subjects = store
        .owned_set_preview(input, &opts)
        .unwrap()
        .expected_subjects;
    store
        .apply_owned_set(input, &opts, &format!("set-{sequence}"), "person/operator")
        .unwrap()
}
fn direct(store: &Store, input: &NormalizedIntent, key: &str) -> Result<ApplyResponse, St3Error> {
    let preview = store
        .mission(
            input,
            IntentInput {
                kdl: "".into(),
                source_name: None,
            },
        )
        .unwrap();
    store.apply_as(input, &preview.subject_tokens, key, Some("person/operator"))
}
fn share(from: &Store, to: &Store) {
    to.import_replication(&from.origin, &from.export_replication(0).unwrap())
        .unwrap();
}

#[test]
fn stale_render_is_refused_even_with_fresh_set_and_member_heads() {
    let store = Store::open_memory("amber").unwrap();
    apply(&store, &bundle("new", false), 30);
    let input = bundle("old", false);
    let mut opts = options(&store, 20);
    opts.expected_subjects = store
        .owned_set_preview(&input, &opts)
        .unwrap()
        .expected_subjects;
    let before = store.index().unwrap();
    let error = store
        .apply_owned_set(&input, &opts, "old", "person/operator")
        .unwrap_err();
    assert_eq!(error.code, "owned-set-refused");
    assert_eq!(store.index().unwrap(), before);
    assert_eq!(store.owned_sets().unwrap()[0].receipt.source.sequence, 30);
}

#[test]
fn partition_heal_selects_highest_source_and_blocks_stale_member_writes() {
    let amber = Store::open_memory("amber").unwrap();
    let cobalt = Store::open_memory("cobalt").unwrap();
    let ivory = Store::open_memory("ivory").unwrap();
    apply(&amber, &bundle("initial", true), 10);
    share(&amber, &cobalt);
    share(&amber, &ivory);
    apply(&amber, &bundle("new", true), 30);
    apply(&cobalt, &bundle("old", true), 20);
    share(&amber, &ivory);
    share(&cobalt, &ivory);
    share(&cobalt, &amber);
    share(&amber, &cobalt);
    let expected = amber
        .selected_desired_token("agent/garden/orchard")
        .unwrap();
    assert!(expected.is_some());
    for store in [&amber, &cobalt, &ivory] {
        assert_eq!(store.owned_sets().unwrap()[0].receipt.source.sequence, 30);
        assert_eq!(
            store
                .selected_desired_token("agent/garden/orchard")
                .unwrap(),
            expected
        );
        let error = direct(store, &bundle("stale", true), "direct-stale").unwrap_err();
        assert_eq!(error.code, "set-managed-subject");
        store
            .connection
            .batched(replay_graph_from_nothing_tx)
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .selected_desired_token("agent/garden/orchard")
                .unwrap(),
            expected
        );
    }
}

#[test]
fn pruning_requires_exact_preview_confirmation_and_retains_ownership() {
    let store = Store::open_memory("amber").unwrap();
    apply(&store, &bundle("true", true), 10);
    let input = bundle("true", false);
    let mut opts = options(&store, 20);
    let preview = store.owned_set_preview(&input, &opts).unwrap();
    assert!(preview.mass_retirement);
    opts.expected_subjects = preview.expected_subjects.clone();
    let before = store.index().unwrap();
    assert_eq!(
        store
            .apply_owned_set(&input, &opts, "retire", "person/operator")
            .unwrap_err()
            .code,
        "mass-retirement-refused"
    );
    assert_eq!(store.index().unwrap(), before);
    opts.confirm_retire = Some(preview.digest);
    store
        .apply_owned_set(&input, &opts, "retire", "person/operator")
        .unwrap();
    let stopped = store
        .desired_subjects()
        .unwrap()
        .into_iter()
        .find(|s| s.subject == "agent/garden/meadow")
        .unwrap();
    assert_eq!(stopped.kind, "stop");
    assert!(
        store.owned_sets().unwrap()[0]
            .receipt
            .retired
            .contains_key("agent/garden/meadow")
    );
    assert_eq!(
        direct(&store, &bundle("resurrect", true), "resurrect")
            .unwrap_err()
            .code,
        "set-managed-subject"
    );
    assert!(
        store
            .claims_for("agent/garden/meadow", Some("intent.desired"))
            .unwrap()
            .len()
            >= 2
    );
}

#[test]
fn confirmation_is_bound_to_content_and_source() {
    let store = Store::open_memory("amber").unwrap();
    apply(&store, &bundle("initial", true), 10);
    let input = bundle("new", false);
    let mut opts = options(&store, 20);
    let p = store.owned_set_preview(&input, &opts).unwrap();
    opts.confirm_retire = Some(p.digest);
    let different = bundle("different", false);
    opts.expected_subjects = store
        .owned_set_preview(&different, &opts)
        .unwrap()
        .expected_subjects;
    assert_eq!(
        store
            .apply_owned_set(&different, &opts, "different", "person/operator")
            .unwrap_err()
            .code,
        "mass-retirement-refused"
    );
}

#[test]
fn explicit_adoption_keeps_unrelated_declarations_and_atomic_failure_changes_nothing() {
    let store = Store::open_memory("amber").unwrap();
    let input = bundle("initial", true);
    direct(&store, &input, "manual").unwrap();
    let input = bundle("initial", false);
    let mut opts = options(&store, 10);
    let p = store.owned_set_preview(&input, &opts).unwrap();
    assert!(p.blockers.iter().any(|b| b.contains("adoption")));
    opts.adopt.insert("agent/garden/orchard".into());
    opts.expected_subjects = store
        .owned_set_preview(&input, &opts)
        .unwrap()
        .expected_subjects;
    store
        .apply_owned_set(&input, &opts, "adopt", "person/operator")
        .unwrap();
    assert!(
        store.owned_sets().unwrap()[0]
            .receipt
            .adoptions
            .contains_key("agent/garden/orchard")
    );
    let meadow = store.selected_desired_token("agent/garden/meadow").unwrap();
    let mut invalid = bundle("new", true);
    invalid
        .subjects
        .get_mut("agent/garden/meadow")
        .unwrap()
        .kind = "unsupported".into();
    let before = store.index().unwrap();
    assert_eq!(
        store
            .apply_owned_set(&invalid, &options(&store, 20), "invalid", "person/operator")
            .unwrap_err()
            .code,
        "unsupported-set-member"
    );
    assert_eq!(store.index().unwrap(), before);
    assert_eq!(
        store.selected_desired_token("agent/garden/meadow").unwrap(),
        meadow
    );
}

#[test]
fn same_source_retry_is_noop_and_new_source_does_not_relaunch_unchanged_members() {
    let store = Store::open_memory("amber").unwrap();
    let input = bundle("initial", true);
    apply(&store, &input, 10);
    let token = store
        .selected_desired_token("agent/garden/orchard")
        .unwrap();
    let before = store.index().unwrap();
    let opts = options(&store, 10);
    let p = store.owned_set_preview(&input, &opts).unwrap();
    assert!(p.noop);
    assert!(
        !store
            .apply_owned_set(&input, &opts, "repeat", "person/operator")
            .unwrap()
            .changed
    );
    assert_eq!(store.index().unwrap(), before);
    apply(&store, &input, 20);
    assert_eq!(
        store
            .selected_desired_token("agent/garden/orchard")
            .unwrap(),
        token
    );
    assert_eq!(store.owned_sets().unwrap()[0].receipt.source.sequence, 20);
}

#[test]
fn empty_set_needs_both_explicit_empty_and_preview_bound_retirement() {
    let store = Store::open_memory("amber").unwrap();
    apply(&store, &bundle("initial", true), 10);
    let input = crate::graph::parse_owned_set_intent("version 2", "amber").unwrap();
    let mut opts = options(&store, 20);
    opts.expected_subjects = store
        .owned_set_preview(&input, &opts)
        .unwrap()
        .expected_subjects;
    assert_eq!(
        store
            .apply_owned_set(&input, &opts, "empty", "person/operator")
            .unwrap_err()
            .code,
        "empty-owned-set"
    );
    opts.allow_empty = true;
    let p = store.owned_set_preview(&input, &opts).unwrap();
    opts.expected_subjects = p.expected_subjects;
    opts.confirm_retire = Some(p.digest);
    store
        .apply_owned_set(&input, &opts, "empty", "person/operator")
        .unwrap();
    assert!(store.owned_sets().unwrap()[0].receipt.members.is_empty());
}

#[test]
fn mission_omission_retains_active_runs_and_blocks_new_runs_even_on_old_pin() {
    let store = Store::open_memory("amber").unwrap();
    let kdl = "version 2\nmission \"harvest\" state=\"ready\" { goal \"Harvest\"; step \"work\" { assigned-to \"agent/garden/orchard\" } }\nagent \"garden/orchard\" { command \"true\" }";
    let input = parse_intent(kdl, "amber").unwrap();
    apply(&store, &input, 10);
    let revision = input.missions["harvest"].revision.clone();
    let request = crate::model::MissionRunRequest {
        mission: "harvest".into(),
        revision: Some(revision.clone()),
        workspace: "/tmp".into(),
        requester: Some("person/operator".into()),
        mode: Some("run".into()),
        inputs: Default::default(),
        idempotency_key: "active".into(),
    };
    let run = store.create_mission_run(&request).unwrap();
    let input = bundle("true", false);
    let mut opts = options(&store, 20);
    let p = store.owned_set_preview(&input, &opts).unwrap();
    opts.expected_subjects = p.expected_subjects;
    opts.confirm_retire = Some(p.digest);
    store
        .apply_owned_set(&input, &opts, "prune-mission", "person/operator")
        .unwrap();
    assert_eq!(
        store.mission_spec("harvest", None).unwrap().unwrap().state,
        MissionState::Retired
    );
    assert_eq!(
        store
            .mission_spec("harvest", Some(&revision))
            .unwrap()
            .unwrap()
            .state,
        MissionState::Ready
    );
    assert_eq!(
        store.mission_run(&run.id).unwrap().unwrap().status,
        "running"
    );
    let mut request = request;
    request.idempotency_key = "new".into();
    assert_eq!(
        store.create_mission_run(&request).unwrap_err().code,
        "mission-retired"
    );
}

fn signed_fleet() -> Vec<Store> {
    let fleet = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
    let keys = (0..3)
        .map(|_| Arc::new(crate::fleet::MemberKey::generate().unwrap().0))
        .collect::<Vec<_>>();
    let stores = ["amber", "cobalt", "ivory"]
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let store = Store::open_memory(*name).unwrap();
            store.bind_fleet(fleet).unwrap();
            store.pin_fleet_anchor(keys[0].public()).unwrap();
            store.set_member_key(Some(keys[i].clone())).unwrap();
            store
        })
        .collect::<Vec<_>>();
    for (i, store) in stores.iter().enumerate() {
        let mut fields = json!({"fleet_id":fleet,"member_key":keys[i].public(),"via":if i==0{"anchor"}else{"invite"},"mode":"listening"});
        if i != 0 {
            fields["sponsor"] = json!("host/amber");
        }
        stores[0]
            .append_claim(&ClaimInput {
                subject: format!("host/{}", store.origin),
                kind: "fleet.member-admitted".into(),
                actor: None,
                fields: serde_json::from_value(fields).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        store
            .append_claim(&ClaimInput {
                subject: format!("daemon/{}", store.origin),
                kind: "daemon.started".into(),
                actor: None,
                fields: serde_json::from_value(
                    json!({"status":"running","features":{"owned_sets":1}}),
                )
                .unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    for from in &stores {
        for to in &stores {
            if from.origin != to.origin {
                signed_share(from, to);
            }
        }
    }
    stores
}

#[test]
fn rollout_policy_preserves_legacy_hashes_and_requires_every_active_daemon() {
    let stores = signed_fleet();
    let input = bundle("unchanged", false);
    apply(&stores[0], &input, 10);
    let legacy = serde_json::to_value(&stores[0].owned_sets().unwrap()[0].receipt).unwrap();
    assert!(legacy.get("rollout").is_none());
    let restored: owned_sets::Revision = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(
        canonical_hash(&restored).unwrap(),
        canonical_hash(&legacy).unwrap()
    );
    let mut opts = options(&stores[0], 20);
    opts.rollout = Some(crate::rollout::Policy::when_idle(1_800_000, false));
    let blocked = stores[0].owned_set_preview(&input, &opts).unwrap();
    assert_eq!(
        blocked
            .blockers
            .iter()
            .filter(|reason| reason.contains("seat-rollout support"))
            .count(),
        3
    );
    for from in &stores {
        from.append_claim(&ClaimInput {
            subject: format!("daemon/{}", from.origin),
            kind: "daemon.started".into(),
            actor: None,
            fields: serde_json::from_value(
                json!({"status":"running","features":{"owned_sets":1,"seat_rollout":1}}),
            )
            .unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
        for to in &stores {
            signed_share(from, to);
        }
    }
    let ready = stores[0].owned_set_preview(&input, &opts).unwrap();
    assert!(ready.blockers.is_empty(), "{:?}", ready.blockers);
    opts.expected_subjects = ready.expected_subjects;
    stores[0]
        .apply_owned_set(&input, &opts, "rollout-policy", "person/operator")
        .unwrap();
    let same_source = options(&stores[0], 20);
    let preview = stores[0].owned_set_preview(&input, &same_source).unwrap();
    assert!(
        !preview.noop,
        "changing policy is not an equal-source retry"
    );
    assert!(
        preview
            .blockers
            .iter()
            .any(|b| b.contains("source sequence"))
    );
}

#[test]
fn rollout_preview_refuses_host_harness_and_authored_session_transitions() {
    let store = Store::open_memory("amber").unwrap();
    let native = |host: &str, harness: &str, args: &str| {
        crate::graph::parse_owned_set_intent(&format!(
        "version 2\nagent \"garden/orchard\" {{ host {host:?}; workspace \".\"; harness {harness:?} {{ {args} }} }}"),"amber").unwrap()
    };
    apply(&store, &native("amber", "claude", "model \"first\";"), 10);
    for (host, harness, args) in [
        ("cobalt", "claude", "model \"second\";"),
        ("amber", "codex", "model \"second\";"),
        ("amber", "claude", "account \"cloud\"; model \"second\";"),
        ("amber", "claude", "args \"--resume\" \"authored-session\";"),
    ] {
        let mut opts = options(&store, 20);
        opts.rollout = Some(crate::rollout::Policy::when_idle(1_800_000, false));
        let preview = store
            .owned_set_preview(&native(host, harness, args), &opts)
            .unwrap();
        assert!(!preview.blockers.is_empty(), "{host} {harness} {args}");
        assert!(
            store
                .apply_owned_set(
                    &native(host, harness, args),
                    &opts,
                    "unsupported",
                    "person/operator"
                )
                .is_err()
        );
        assert_eq!(store.owned_sets().unwrap()[0].receipt.source.sequence, 10);
    }
}
fn signed_share(from: &Store, to: &Store) {
    let fleet = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
    let exchange = from
        .export_replication_exchange_answering(
            fleet,
            &to.replication_inventory().unwrap(),
            &to.replication_signature_requests().unwrap(),
        )
        .unwrap();
    to.receive_replication_exchange(&from.origin, fleet, &exchange)
        .unwrap();
    let admission = to.validate_replication_backlog().unwrap();
    assert_eq!(admission.invalid, 0);
    assert_eq!(admission.unknown, 0);
    to.project_replication_backlog().unwrap();
}

#[test]
fn signed_replication_heal_replay_and_checkpoint_keep_source_winner() {
    let stores = signed_fleet();
    let (amber, cobalt, ivory) = (&stores[0], &stores[1], &stores[2]);
    apply(amber, &bundle("initial", true), 10);
    signed_share(amber, cobalt);
    signed_share(amber, ivory);
    apply(amber, &bundle("new", true), 30);
    apply(cobalt, &bundle("old", true), 20);
    // The older render arrives last, after all of the newer one's records were admitted.
    signed_share(amber, ivory);
    signed_share(cobalt, ivory);
    signed_share(cobalt, amber);
    signed_share(amber, cobalt);
    let token = amber
        .selected_desired_token("agent/garden/orchard")
        .unwrap();
    assert!(token.is_some());
    for store in &stores {
        assert_eq!(store.owned_sets().unwrap()[0].receipt.source.sequence, 30);
        assert_eq!(
            store
                .selected_desired_token("agent/garden/orchard")
                .unwrap(),
            token
        );
        store
            .connection
            .batched(replay_graph_from_nothing_tx)
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .selected_desired_token("agent/garden/orchard")
                .unwrap(),
            token
        );
        assert!(
            store.status(Some("agent/garden/orchard")).unwrap().subjects[0]
                .conflicts
                .is_empty()
        );
        let scratch = tempfile::tempdir().unwrap();
        let (_, _, proof) = store
            .plan_checkpoint_through(now_ms() + 1_000, None, scratch.path())
            .unwrap();
        assert!(proof.passed, "{proof:?}");
        let claims = store
            .claims_for("owned-set/garden", Some("owned-set.revised"))
            .unwrap();
        assert_eq!(claims.len(), 3);
        for c in claims {
            assert!(checkpoint_rules::slot_of(&c).is_none());
        }
        for c in store
            .claims_for("agent/garden/orchard", Some("intent.desired"))
            .unwrap()
        {
            assert!(checkpoint_rules::slot_of(&c).is_none());
        }
    }
}

#[test]
fn a_peer_without_set_support_blocks_activation_without_blocking_unmanaged_publication() {
    let stores = signed_fleet();
    let amber = &stores[0];
    let cobalt = &stores[1];
    cobalt
        .append_claim(&ClaimInput {
            subject: "daemon/cobalt".into(),
            kind: "daemon.started".into(),
            actor: None,
            fields: serde_json::from_value(json!({"status":"running","version":"old"})).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    signed_share(cobalt, amber);
    let input = bundle("initial", false);
    let opts = options(amber, 10);
    let p = amber.owned_set_preview(&input, &opts).unwrap();
    assert!(p.blockers.iter().any(|b| b.contains("host/cobalt")));
    assert_eq!(
        amber
            .apply_owned_set(&input, &opts, "unsupported", "person/operator")
            .unwrap_err()
            .code,
        "owned-set-refused"
    );
    direct(amber, &input, "manual").unwrap();
}

#[test]
fn stale_independent_claim_written_before_adoption_cannot_override_set_on_heal() {
    let amber = Store::open_memory("amber").unwrap();
    let cobalt = Store::open_memory("cobalt").unwrap();
    direct(&amber, &bundle("initial", false), "initial").unwrap();
    share(&amber, &cobalt);
    let input = bundle("managed", false);
    let mut opts = options(&amber, 10);
    opts.adopt.insert("agent/garden/orchard".into());
    opts.expected_subjects = amber
        .owned_set_preview(&input, &opts)
        .unwrap()
        .expected_subjects;
    amber
        .apply_owned_set(&input, &opts, "adopt", "person/operator")
        .unwrap();
    let token = amber
        .selected_desired_token("agent/garden/orchard")
        .unwrap();
    direct(&cobalt, &bundle("stale", false), "stale").unwrap();
    share(&cobalt, &amber);
    assert_eq!(
        amber
            .selected_desired_token("agent/garden/orchard")
            .unwrap(),
        token
    );
    share(&amber, &cobalt);
    assert_eq!(
        cobalt
            .selected_desired_token("agent/garden/orchard")
            .unwrap(),
        token
    );
}

#[test]
fn incomplete_highest_set_holds_member_effects_until_dependencies_arrive() {
    let amber = Store::open_memory("amber").unwrap();
    let cobalt = Store::open_memory("cobalt").unwrap();
    apply(&amber, &bundle("initial", false), 10);
    let old = amber.owned_sets().unwrap()[0].clone();
    let token = amber
        .selected_desired_token("agent/garden/orchard")
        .unwrap();
    direct(&cobalt, &bundle("new", false), "future-member").unwrap();
    let future = cobalt
        .claims_for("agent/garden/orchard", Some("intent.desired"))
        .unwrap()
        .pop()
        .unwrap();
    let mut receipt = old.receipt.clone();
    receipt.previous = Some(format!("{}@{}", old.id, old.revision));
    receipt.source = options(&amber, 30).source;
    let desired: DesiredSubject = serde_json::from_value(future.body.clone()).unwrap();
    receipt
        .members
        .get_mut("agent/garden/orchard")
        .unwrap()
        .claim = future.id.clone();
    receipt
        .members
        .get_mut("agent/garden/orchard")
        .unwrap()
        .revision = desired_revision(&desired);
    amber
        .append_claim(&ClaimInput {
            subject: old.id,
            kind: "owned-set.revised".into(),
            actor: None,
            fields: serde_json::from_value(
                json!({"revision":canonical_hash(&receipt).unwrap(),"body":receipt}),
            )
            .unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    assert!(!amber.owned_sets().unwrap()[0].blockers.is_empty());
    assert_eq!(
        amber
            .owned_member_guard("agent/garden/orchard")
            .unwrap_err()
            .code,
        "owned-set-pending"
    );
    assert_eq!(
        amber
            .selected_desired_token("agent/garden/orchard")
            .unwrap(),
        token
    );
    share(&cobalt, &amber);
    assert!(amber.owned_sets().unwrap()[0].blockers.is_empty());
    assert_eq!(
        amber
            .selected_desired_token("agent/garden/orchard")
            .unwrap(),
        Some(future.id)
    );
}

#[test]
fn reused_idempotency_key_with_changed_input_is_refused() {
    let store = Store::open_memory("amber").unwrap();
    let input = bundle("initial", false);
    let mut opts = options(&store, 10);
    opts.expected_subjects = store
        .owned_set_preview(&input, &opts)
        .unwrap()
        .expected_subjects;
    store
        .apply_owned_set(&input, &opts, "same-key", "person/operator")
        .unwrap();
    assert_eq!(
        store
            .apply_owned_set(
                &bundle("different", false),
                &opts,
                "same-key",
                "person/operator"
            )
            .unwrap_err()
            .code,
        "idempotency-mismatch"
    );
}

#[test]
fn a_render_prepared_from_a_superseded_set_cannot_overwrite_selected_files() {
    let store = Store::open_memory("amber").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let source = |content: &str| {
        parse_intent(&format!(
        "version 2\nagent \"garden/orchard\" {{\n workspace {:?}\n command \"true\"\n render {{ file \"configuration.txt\" {content:?} }}\n}}\n",
        workspace.path().display().to_string()), "amber").unwrap()
    };
    let old = source("old");
    apply(&store, &old, 10);
    let new = source("new");
    apply(&store, &new, 20);
    let subject = "agent/garden/orchard";
    let selected = &new.subjects[subject];
    assert!(crate::render::apply_all(&store, &[selected], "amber")[subject].is_ok());
    let stale = &old.subjects[subject];
    assert!(crate::render::apply_all(&store, &[stale], "amber")[subject].is_err());
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("configuration.txt")).unwrap(),
        "new"
    );
}

#[test]
fn a_member_added_only_on_a_losing_partition_retires_through_the_winning_set() {
    let stores = signed_fleet();
    let (amber, cobalt, ivory) = (&stores[0], &stores[1], &stores[2]);
    apply(amber, &bundle("initial", false), 10);
    signed_share(amber, cobalt);
    signed_share(amber, ivory);
    apply(cobalt, &bundle("initial", true), 20);
    apply(amber, &bundle("newer", false), 30);
    signed_share(cobalt, ivory);
    signed_share(amber, ivory);
    signed_share(cobalt, amber);
    signed_share(amber, cobalt);
    for store in &stores {
        let stopped = store
            .desired_subject_with_writer("agent/garden/meadow")
            .unwrap()
            .unwrap()
            .0;
        assert_eq!(stopped.kind, "stop");
        assert!(store.owned_desired_guard(&stopped).is_ok());
        assert_eq!(
            direct(store, &bundle("revive", true), "bypass")
                .unwrap_err()
                .code,
            "set-managed-subject"
        );
        store
            .connection
            .batched(replay_graph_from_nothing_tx)
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .desired_subject_with_writer("agent/garden/meadow")
                .unwrap()
                .unwrap()
                .0
                .kind,
            "stop"
        );
        let scratch = tempfile::tempdir().unwrap();
        assert!(
            store
                .plan_checkpoint_through(now_ms() + 1_000, None, scratch.path())
                .unwrap()
                .2
                .passed
        );
    }
    // A subsequent apply makes the learned retirement an explicit immutable stop reference.
    let input = bundle("newer", false);
    let mut opts = options(amber, 40);
    let preview = amber.owned_set_preview(&input, &opts).unwrap();
    opts.expected_subjects = preview.expected_subjects;
    opts.confirm_retire = Some(preview.digest);
    amber
        .apply_owned_set(&input, &opts, "learned-retirement", "person/operator")
        .unwrap();
    let view = amber.owned_sets().unwrap().remove(0);
    assert!(view.blockers.is_empty(), "{:?}", view.blockers);
    let retired = &view.receipt.retired["agent/garden/meadow"];
    assert_eq!(
        amber.claim_by_id(&retired.claim).unwrap().unwrap().kind,
        "intent.desired"
    );
}

#[test]
fn equal_source_content_conflicts_hold_effects_until_a_higher_source_resolves_them() {
    let stores = signed_fleet();
    let (amber, cobalt, ivory) = (&stores[0], &stores[1], &stores[2]);
    apply(amber, &bundle("initial", false), 10);
    signed_share(amber, cobalt);
    signed_share(amber, ivory);
    apply(amber, &bundle("one", false), 20);
    apply(cobalt, &bundle("two", false), 20);
    signed_share(amber, ivory);
    signed_share(cobalt, ivory);
    assert!(ivory.owned_member_guard("agent/garden/orchard").is_err());
    assert!(!ivory.owned_sets().unwrap()[0].blockers.is_empty());
    apply(ivory, &bundle("settled", false), 30);
    signed_share(ivory, amber);
    signed_share(ivory, cobalt);
    for store in &stores {
        let view = store.owned_sets().unwrap().remove(0);
        assert_eq!(view.receipt.source.sequence, 30);
        assert!(view.blockers.is_empty());
        assert!(store.owned_member_guard("agent/garden/orchard").is_ok());
    }
}

#[test]
fn rollout_signed_partition_heal_keeps_the_winning_owner_operation_and_status() {
    let stores = signed_fleet();
    for from in &stores {
        from.append_claim(&ClaimInput {
            subject: format!("daemon/{}", from.origin), kind: "daemon.started".into(), actor: None,
            fields: serde_json::from_value(json!({"status":"running","features":{"owned_sets":1,"seat_rollout":1}})).unwrap(),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
        for to in &stores { signed_share(from, to); }
    }
    let (amber, cobalt, ivory) = (&stores[0], &stores[1], &stores[2]);
    let native = |model: &str| crate::graph::parse_owned_set_intent(&format!(
        "version 2\nagent \"garden/orchard\" {{ host \"amber\"; workspace \".\"; harness \"claude\" {{ model {model:?}; }} }}"), "amber").unwrap();
    apply(amber, &native("initial"), 10);
    let old = amber.desired_subjects_named(&["agent/garden/orchard".into()]).unwrap().remove(0).member.unwrap();
    amber.append_claim(&ClaimInput {
        subject: "agent/garden/orchard".into(), kind: "runtime.observed".into(), actor: Some("person/operator".into()),
        fields: serde_json::from_value(json!({"status":"running","host":"amber","runtime_id":old.runtime_id,"incarnation_id":"original-one"})).unwrap(),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
    signed_share(amber, cobalt); signed_share(amber, ivory);
    let policy = crate::rollout::Policy::when_idle(1_800_000, false);
    for (store, sequence, model) in [(amber, 30, "winner"), (cobalt, 20, "stale")] {
        let mut opts = options(store, sequence); opts.rollout = Some(policy.clone());
        let input = native(model);
        let preview = store.owned_set_preview(&input, &opts).unwrap();
        assert!(preview.blockers.is_empty(), "{:?}", preview.blockers);
        opts.expected_subjects = preview.expected_subjects;
        store.apply_owned_set(&input, &opts, &format!("partition-{sequence}"), "person/operator").unwrap();
    }
    let token = amber.selected_desired_token("agent/garden/orchard").unwrap().unwrap();
    let request = amber.request_rollout("agent/garden/orchard", &token, &old, "original-one", "person/operator", &policy, "winner-operation").unwrap();
    let operation = amber.rollout("agent/garden/orchard").unwrap().unwrap();
    crate::rollout::phase(amber, "agent/garden/orchard", &operation, "held", Some("busy at deadline"), &["claimed-work".into()]).unwrap();
    signed_share(cobalt, ivory); signed_share(amber, ivory);
    signed_share(cobalt, amber); signed_share(amber, cobalt);
    for store in &stores {
        let selected = store.rollout_selection("agent/garden/orchard").unwrap().unwrap();
        let operation = store.rollout("agent/garden/orchard").unwrap().unwrap();
        assert_eq!(selected.source.sequence, 30);
        assert_eq!(operation.id, request.id);
        assert_eq!(operation.phase, "held");
        assert_eq!(operation.old_incarnation, "original-one");
        assert_eq!(operation.deadline_unix_ms, operation.requested_at_unix_ms + 1_800_000);
        assert_eq!(operation.blocking, vec!["claimed-work"]);
    }
}
