//! Pass candidates and cached work remain subject to fresh declaration authority.
use super::*;

fn append_receipt(store: &Store, sequence: u64, incomplete: bool) {
    let current = store.owned_sets().unwrap().remove(0);
    let mut receipt = current.receipt.clone();
    receipt.previous = Some(format!("{}@{}", current.id, current.revision));
    receipt.source.sequence = sequence;
    receipt.source.sha = format!("{sequence:040x}");
    if incomplete {
        receipt
            .members
            .get_mut("agent/garden/orchard")
            .unwrap()
            .claim = "f".repeat(64);
    } else {
        // Restore the real member reference from the original admitted receipt.
        receipt.members = store
            .owned_set_history("garden")
            .unwrap()
            .into_iter()
            .find(|view| view.receipt.source.sequence == 10)
            .unwrap()
            .receipt
            .members;
    }
    store
        .append_claim(&ClaimInput {
            subject: current.id,
            kind: "owned-set.revised".into(),
            actor: None,
            fields: serde_json::from_value(serde_json::json!({
                "revision": smallclaims::hash::canonical_hash(&receipt).unwrap(), "body": receipt,
            }))
            .unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

#[test]
fn a_clean_cached_wake_preserves_fault_and_dependencies_after_authority_changes() {
    for (revoke, replacement) in [(false, false), (true, false), (true, true)] {
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_memory("node").unwrap());
        publish_owned_member(&store, workspace.path(), "node", 10);
        let runtime = Arc::new(FakeRuntime::default());
        let mut reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        let declaration = store.desired_subjects().unwrap().remove(0);
        let member = declaration.member.as_ref().unwrap();
        *runtime.ptys.lock().unwrap() = vec![RuntimeObservation {
            runtime_id: member.runtime_id.clone(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("incumbent".into()),
        }];
        store
            .append_claim(&ClaimInput {
                subject: declaration.subject.clone(),
                kind: "harness.observed".into(),
                actor: Some(declaration.subject.clone()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("ready".into())),
                    ("driver".into(), Value::String("codex".into())),
                    ("incarnation_id".into(), Value::String("incumbent".into())),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        reconciler.skip_unneeded = true;
        reconciler.reconcile_once().unwrap();
        assert!(
            reconciler
                .member_wakes
                .lock()
                .unwrap()
                .contains_key(&declaration.subject)
        );
        let fault = "retained prior member fault";
        reconciler
            .record_member_reconcile_result(&declaration.subject, Err(anyhow::anyhow!(fault)))
            .unwrap();
        let decisions = store
            .claims_for(&declaration.subject, Some("runtime.reconcile-decision"))
            .unwrap();
        let ids = decisions
            .iter()
            .map(|claim| claim.id.clone())
            .collect::<Vec<_>>();
        reconciler.incremental.observe(&store).unwrap();
        let member_item = format!("member:{}", declaration.subject);
        let member_reads = reconciler.incremental.reads_of(&member_item);
        let wake = format!("wake:{}@incumbent", declaration.subject);
        let wake_reads = reconciler.incremental.reads_of(&wake);
        assert!(wake_reads.contains("kind:owned-set.revised"));
        reconciler
            .incremental
            .evaluated(&member_item, member_reads.clone(), Some(u128::MAX));
        reconciler
            .incremental
            .evaluated(&wake, wake_reads.clone(), Some(u128::MAX));
        let render_reads = reconciler.incremental.reads_of("render");
        reconciler
            .incremental
            .evaluated("render", render_reads, None);
        let changed = store.clone();
        let captured = declaration.clone();
        let changed_workspace = workspace.path().to_path_buf();
        let cached_item = member_item.clone();
        let cached_wake = wake.clone();
        let raced = Arc::new(AtomicBool::new(false));
        let marker = raced.clone();
        *reconciler.after_work_wake_observe.lock().unwrap() =
            Some(Box::new(move |incremental, skip| {
                // Exact clean branch: the real feed observation is over and no wake is due/dirty.
                assert!(skip);
                assert!(!incremental.needs(&cached_wake, now_ms()));
                assert_eq!(
                    incremental.next_due("member:"),
                    Some(u128::MAX),
                    "member reused its cache"
                );
                assert_eq!(
                    changed
                        .member_reconcile_fault(&captured.subject, None)
                        .unwrap()
                        .as_deref(),
                    Some(fault)
                );
                assert!(changed.owned_desired_guard(&captured).is_ok());
                if replacement {
                    publish_owned_member(&changed, &changed_workspace, "node", 20);
                    assert_eq!(
                        changed.owned_desired_guard(&captured).unwrap_err().code,
                        "stale-set-member"
                    );
                } else if revoke {
                    append_receipt(&changed, 20, true);
                }
                assert!(
                    !incremental.needs(&cached_wake, now_ms()),
                    "publication after observation has not dirtied the wake"
                );
                assert!(!incremental.needs(&cached_item, now_ms()));
                marker.store(true, Ordering::SeqCst);
            }));
        let starts = runtime.starts.lock().unwrap().len();
        let stops = runtime.stops.lock().unwrap().len();
        let actions = store
            .claims_for(&declaration.subject, Some("runtime.action.requested"))
            .unwrap()
            .len();
        let messages = store
            .messages(Some(&declaration.subject), true)
            .unwrap()
            .len();
        reconciler.reconcile_once().unwrap();
        assert!(raced.load(Ordering::SeqCst));
        assert_eq!(reconciler.incremental.reads_of(&member_item), member_reads);
        assert_eq!(reconciler.incremental.next_due("member:"), Some(u128::MAX));
        assert_eq!(runtime.starts.lock().unwrap().len(), starts);
        assert_eq!(runtime.stops.lock().unwrap().len(), stops);
        assert_eq!(
            store
                .claims_for(&declaration.subject, Some("runtime.action.requested"))
                .unwrap()
                .len(),
            actions
        );
        assert_eq!(
            store
                .messages(Some(&declaration.subject), true)
                .unwrap()
                .len(),
            messages
        );
        let after = store
            .claims_for(&declaration.subject, Some("runtime.reconcile-decision"))
            .unwrap();
        // No evaluation means no success/recovery report, even while authority remains valid.
        // A racing revocation/replacement likewise has no effect to fence in this pass. Its
        // notification must retain the old dependencies for the next actual evaluation.
        assert_eq!(
            after
                .iter()
                .map(|claim| claim.id.clone())
                .collect::<Vec<_>>(),
            ids
        );
        assert_eq!(
            store
                .member_reconcile_fault(&declaration.subject, None)
                .unwrap()
                .as_deref(),
            Some(fault)
        );
        assert_eq!(reconciler.incremental.reads_of(&wake), wake_reads);
        assert_eq!(reconciler.incremental.next_due("wake:"), Some(u128::MAX));
    }
}

#[test]
fn evaluated_member_results_survive_a_clean_wake_and_remain_fenced() {
    for revoke_recovery in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let store = Arc::new(Store::open_memory("node").unwrap());
        publish_owned_member(&store, &workspace, "node", 10);
        let runtime = Arc::new(FakeRuntime::default());
        let mut reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        let declaration = store.desired_subjects().unwrap().remove(0);
        let member = declaration.member.as_ref().unwrap();
        *runtime.ptys.lock().unwrap() = vec![RuntimeObservation {
            runtime_id: member.runtime_id.clone(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("incumbent".into()),
        }];
        store
            .append_claim(&ClaimInput {
                subject: declaration.subject.clone(),
                kind: "harness.observed".into(),
                actor: Some(declaration.subject.clone()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("ready".into())),
                    ("driver".into(), Value::String("codex".into())),
                    ("incarnation_id".into(), Value::String("incumbent".into())),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        reconciler.skip_unneeded = true;
        reconciler.reconcile_once().unwrap();
        let wake = format!("wake:{}@incumbent", declaration.subject);
        let member_item = format!("member:{}", declaration.subject);
        let starts = runtime.starts.lock().unwrap().len();
        for failed in [true, false] {
            if failed {
                fs::remove_dir_all(&workspace).unwrap();
            } else {
                fs::create_dir(&workspace).unwrap();
            }
            reconciler.incremental.observe(&store).unwrap();
            reconciler.incremental.touch(&declaration.subject);
            assert!(reconciler.incremental.needs(&member_item, now_ms()));
            let cached_wake = wake.clone();
            let changed = store.clone();
            *reconciler.after_work_wake_observe.lock().unwrap() =
                Some(Box::new(move |incremental, skip| {
                    assert!(skip);
                    let reads = incremental.reads_of(&cached_wake);
                    assert!(!reads.is_empty());
                    incremental.evaluated(&cached_wake, reads, Some(u128::MAX));
                    if !failed && revoke_recovery {
                        append_receipt(&changed, 20, true);
                    }
                    assert!(!incremental.needs(&cached_wake, now_ms()));
                }));
            reconciler.reconcile_once().unwrap();
            let fault = store
                .member_reconcile_fault(&declaration.subject, None)
                .unwrap();
            if failed || revoke_recovery {
                assert!(
                    fault
                        .as_deref()
                        .is_some_and(|reason| reason.contains("workspace")),
                    "evaluated member fault must survive a clean wake"
                );
            } else {
                assert!(
                    fault.is_none(),
                    "evaluated member success must recover through the fresh guard"
                );
            }
            assert_eq!(runtime.starts.lock().unwrap().len(), starts);
            assert!(runtime.stops.lock().unwrap().is_empty());
        }
    }
}

#[test]
fn generic_item_skip_preserves_success_without_work_or_evaluation() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    let item = "subscription:unchanged";
    let reads = BTreeSet::from(["fixture:retained".to_owned()]);
    reconciler
        .incremental
        .evaluated(item, reads.clone(), Some(u128::MAX));
    let before = store.index().unwrap();
    reconciler
        .reconcile_item("subscription", item, true, || {
            panic!("clean item must skip work")
        })
        .unwrap();
    assert_eq!(store.index().unwrap(), before);
    assert_eq!(reconciler.incremental.reads_of(item), reads);
    assert_eq!(
        reconciler.incremental.next_due("subscription:"),
        Some(u128::MAX)
    );
    assert!(
        reconciler
            .reconcile_item("subscription", item, false, || anyhow::bail!("work failed"))
            .is_err()
    );
    assert_eq!(reconciler.incremental.reads_of(item), reads);
    assert_eq!(
        reconciler.incremental.next_due("subscription:"),
        Some(u128::MAX)
    );
}

#[test]
fn ownership_error_diagnostics_are_bounded_across_candidates_and_passes() {
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let workspace = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open_memory("node").unwrap());
    publish_owned_member(&store, workspace.path(), "node", 10);
    let declaration = store.desired_subjects().unwrap().remove(0);
    let receipt = store.owned_sets().unwrap().remove(0);
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    store
        .connection
        .write()
        .execute(
            "UPDATE claims SET accepted_at_unix_ms='invalid' WHERE id=?1",
            [&receipt.claim],
        )
        .unwrap();
    let before = store.index().unwrap();
    let capture = Capture(Arc::new(Mutex::new(Vec::new())));
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::ERROR)
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let output = || String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    let diagnostics = || {
        output()
            .matches("reconciler ownership read failed; candidates fenced")
            .count()
    };
    tracing::subscriber::with_default(subscriber, || {
        for pass in 0..100 {
            assert!(!reconciler.owned_desired_ready(&declaration));
            let item = format!("wake:different-candidate-{pass}");
            assert!(matches!(
                reconciler.reconcile_guarded_item(
                    "wake",
                    &item,
                    true,
                    || reconciler.owned_desired_check(&declaration),
                    || panic!("log suppression cannot authorize work")
                ),
                ReconcileItemOutcome::GuardFailed
            ));
            assert!(reconciler.incremental.reads_of(&item).is_empty());
        }
        assert_eq!(diagnostics(), 1, "one shared limit, not one per item/pass");
        assert!(output().contains("suppressed_errors=0"));
        // A different fault shares the same limit, and contributes to the next summary.
        reconciler.report_ownership_error("snapshot", "node", &"a different storage error");
        assert_eq!(diagnostics(), 1);
        // Advance only the logging state: no sleeps, guard-result cache or retry deadline.
        reconciler.ownership_error_log.lock().unwrap().last = Some(
            std::time::Instant::now()
                .checked_sub(OWNERSHIP_ERROR_LOG_INTERVAL + Duration::from_secs(1))
                .unwrap(),
        );
        let error = store
            .owned_desired_subjects(std::slice::from_ref(&declaration))
            .unwrap_err();
        reconciler.report_ownership_error("snapshot", "node", &error);
        assert_eq!(
            diagnostics(),
            2,
            "persistent errors remain visible over time"
        );
        assert!(output().contains("suppressed_errors=200"));
        assert!(!reconciler.owned_desired_ready(&declaration));
        assert_eq!(
            diagnostics(),
            2,
            "snapshot and member errors share the limit"
        );
        reconciler.ownership_error_log.lock().unwrap().last = Some(
            std::time::Instant::now()
                .checked_sub(OWNERSHIP_ERROR_LOG_INTERVAL + Duration::from_secs(1))
                .unwrap(),
        );
        assert!(matches!(
            reconciler.reconcile_guarded_item(
                "wake",
                "wake:still-fenced",
                true,
                || reconciler.owned_desired_check(&declaration),
                || panic!("a later diagnostic cannot authorize work")
            ),
            ReconcileItemOutcome::GuardFailed
        ));
        assert_eq!(diagnostics(), 3);
        let output = output();
        assert!(output.contains("member"));
        assert!(output.contains("snapshot"));
        assert!(output.contains("wake:still-fenced"));
        assert!(output.contains("diagnostic_interval_s=60"));
        assert!(output.contains("suppressed_errors=1"));
    });
    assert_eq!(store.index().unwrap(), before);
}

#[test]
fn a_storage_guard_error_is_distinct_from_authority_rejection() {
    let workspace = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open_memory("node").unwrap());
    publish_owned_member(&store, workspace.path(), "node", 10);
    let declaration = store.desired_subjects().unwrap().remove(0);
    let receipt = store.owned_sets().unwrap().remove(0);
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    let item = format!("wake:{}@incumbent", declaration.subject);
    let reads = BTreeSet::from(["fixture:prior-read".to_owned()]);
    reconciler
        .incremental
        .evaluated(&item, reads.clone(), Some(u128::MAX));
    store
        .connection
        .write()
        .execute(
            "UPDATE claims SET accepted_at_unix_ms='invalid' WHERE id=?1",
            [&receipt.claim],
        )
        .unwrap();
    assert!(reconciler.owned_desired_check(&declaration).is_err());
    let before = store.index().unwrap();
    assert!(matches!(
        reconciler.reconcile_guarded_item(
            "wake",
            &item,
            true,
            || reconciler.owned_desired_check(&declaration),
            || panic!("a storage guard failure cannot do work")
        ),
        ReconcileItemOutcome::GuardFailed
    ));
    assert_eq!(store.index().unwrap(), before);
    assert_eq!(reconciler.incremental.reads_of(&item), reads);
    assert_eq!(reconciler.incremental.next_due("wake:"), Some(u128::MAX));
}

#[test]
fn a_cached_wake_checks_captured_authority_before_message_or_fresh_context_effects() {
    // Ordinary work proves delivery suppression; fresh work also proves incumbent-stop fencing.
    for (fresh, revoke) in [(false, false), (false, true), (true, false), (true, true)] {
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_memory("node").unwrap());
        publish_owned_member(&store, workspace.path(), "node", 10);
        apply_source(
            &store,
            &format!(
                "version 2\nmission \"wake-guard\" state=\"ready\" {{ goal \"Do work\"; step \"work\" {{ assigned-to \"agent/garden/orchard\"; {} }} }}",
                if fresh { "fresh-context" } else { "" },
            ),
            "wake-guard-mission",
        );
        let runtime = Arc::new(FakeRuntime::default());
        let mut reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        let declaration = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|subject| subject.subject == "agent/garden/orchard")
            .unwrap();
        let member = declaration.member.clone().unwrap();
        *runtime.ptys.lock().unwrap() = vec![RuntimeObservation {
            runtime_id: member.runtime_id.clone(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("incumbent".into()),
        }];
        store
            .append_claim(&ClaimInput {
                subject: declaration.subject.clone(),
                kind: "harness.observed".into(),
                actor: Some(declaration.subject.clone()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("ready".into())),
                    ("driver".into(), Value::String("codex".into())),
                    ("incarnation_id".into(), Value::String("incumbent".into())),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        reconciler.skip_unneeded = true;
        reconciler.reconcile_once().unwrap();
        assert!(
            reconciler
                .member_wakes
                .lock()
                .unwrap()
                .contains_key(&declaration.subject)
        );
        store
            .create_mission_run(&MissionRunRequest {
                mission: "wake-guard".into(),
                revision: None,
                workspace: workspace.path().display().to_string(),
                requester: Some("person/operator".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "wake-guard-run".into(),
            })
            .unwrap();
        // Keep the genuinely settled member's dependencies, but make only its wake due.
        reconciler.incremental.observe(&store).unwrap();
        let item = format!("member:{}", declaration.subject);
        let reads = reconciler.incremental.reads_of(&item);
        assert!(!reads.is_empty());
        reconciler
            .incremental
            .evaluated(&item, reads, Some(u128::MAX));
        let render_reads = reconciler.incremental.reads_of("render");
        reconciler
            .incremental
            .evaluated("render", render_reads, None);
        let wake = format!("wake:{}@incumbent", declaration.subject);
        reconciler
            .incremental
            .evaluated(&wake, BTreeSet::new(), Some(0));
        assert!(!reconciler.incremental.needs(&item, now_ms()));
        assert!(reconciler.incremental.needs(&wake, now_ms()));
        let revoked = Arc::new(AtomicBool::new(false));
        let changed = store.clone();
        let marker = revoked.clone();
        let snapshot_declaration = declaration.clone();
        reconciler = reconciler.with_disk_probe(
            vec![workspace.path().to_path_buf()],
            Arc::new(move |_| {
                // This real stage is after the member skip/cached reuse, before work messages.
                if !marker.swap(true, Ordering::SeqCst) {
                    assert!(changed.owned_desired_guard(&snapshot_declaration).is_ok());
                    if revoke {
                        append_receipt(&changed, 20, true);
                        assert!(changed.owned_desired_guard(&snapshot_declaration).is_err());
                    }
                }
                Ok(crate::disk::DiskSpace {
                    filesystem: 1,
                    available: 8 << 30,
                    total: 16 << 30,
                })
            }),
        );
        runtime.stops.lock().unwrap().clear();
        reconciler.reconcile_once().unwrap();
        assert!(revoked.load(Ordering::SeqCst));
        assert_eq!(
            reconciler.incremental.next_due("member:"),
            Some(u128::MAX),
            "member was skipped"
        );
        if !revoke {
            assert!(
                reconciler
                    .incremental
                    .reads_of(&wake)
                    .contains("kind:owned-set.revised")
            );
            if fresh {
                assert_eq!(runtime.stops.lock().unwrap().len(), 1);
                assert!(
                    store
                        .messages(Some(&declaration.subject), true)
                        .unwrap()
                        .is_empty()
                );
            } else {
                assert_eq!(
                    store
                        .messages(Some(&declaration.subject), true)
                        .unwrap()
                        .len(),
                    1
                );
                assert!(runtime.stops.lock().unwrap().is_empty());
            }
            continue;
        }
        assert!(
            store
                .messages(Some(&declaration.subject), true)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .claims_for(&declaration.subject, Some("runtime.action.requested"))
                .unwrap()
                .iter()
                .all(|claim| claim.body["fields"]["action"] != "fresh-context")
        );
        assert!(runtime.stops.lock().unwrap().is_empty());
        assert!(runtime.kills.lock().unwrap().is_empty());
        assert!(
            reconciler.incremental.reads_of(&wake).is_empty(),
            "rejected wake is not evaluated"
        );
        assert_eq!(
            store
                .member_reconcile_fault(&declaration.subject, None)
                .unwrap(),
            None
        );
        // Restoring the same declaration is a positive control for the very same cached tuple.
        append_receipt(&store, 30, false);
        assert!(store.owned_desired_guard(&declaration).is_ok());
        reconciler.disk_probe = None;
        reconciler.reconcile_once().unwrap();
        assert!(
            reconciler
                .incremental
                .reads_of(&wake)
                .contains("kind:owned-set.revised")
        );
        if fresh {
            assert_eq!(runtime.stops.lock().unwrap().len(), 1);
            assert!(
                store
                    .messages(Some(&declaration.subject), true)
                    .unwrap()
                    .is_empty()
            );
        } else {
            assert_eq!(
                store
                    .messages(Some(&declaration.subject), true)
                    .unwrap()
                    .len(),
                1
            );
            assert!(runtime.stops.lock().unwrap().is_empty());
        }
    }
}

fn publish_owned_member(store: &Store, workspace: &Path, host: &str, sequence: u64) {
    let intent = parse_intent(&format!(
        "version 2\nagent \"garden/orchard\" {{ host {host:?}; workspace {:?}; command {:?} }}\n",
        workspace.display().to_string(), format!("echo {sequence}"),
    ), "node").unwrap();
    publish_owned_intent(store, &intent, sequence);
}

fn publish_owned_intent(store: &Store, intent: &crate::NormalizedIntent, sequence: u64) {
    use crate::store::owned_sets::{Options, Source};
    let mut options = Options {
        set: "garden".into(),
        source: Source {
            repository: "acme/garden".into(),
            r#ref: "refs/heads/main".into(),
            sha: format!("{sequence:040x}"),
            sequence,
        },
        expected_set: store
            .owned_sets()
            .unwrap()
            .first()
            .map_or("absent".into(), |view| view.revision.clone()),
        rollout: None,
        adopt: BTreeSet::new(),
        allow_empty: false,
        confirm_retire: None,
        expected_subjects: BTreeMap::new(),
    };
    options.expected_subjects = store
        .owned_set_preview(intent, &options)
        .unwrap()
        .expected_subjects;
    store
        .apply_owned_set(
            intent,
            &options,
            &format!("set-{sequence}"),
            "person/operator",
        )
        .unwrap();
}

#[test]
fn quiet_owned_pass_sql_budget() {
    const CHILD: &str = "ST3_QUIET_OWNED_PASS_BUDGET_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "reconcile::tests::ownership_guard_tests::quiet_owned_pass_sql_budget",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env_remove("ST3_PROFILE_DIR")
            .status()
            .unwrap();
        assert!(status.success(), "isolated SQL budget failed");
        return;
    }
    let mut costs = Vec::new();
    for count in [8, 64] {
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_memory("node").unwrap());
        let mut source = "version 2\n".to_owned();
        for index in 0..count {
            source.push_str(&format!("agent \"garden/worker-{index}\" {{ host \"node\"; workspace {:?}; command \"true\" }}\n", workspace.path().display().to_string()));
        }
        publish_owned_intent(&store, &parse_intent(&source, "node").unwrap(), 10);
        let runtime = Arc::new(FakeRuntime::default());
        let mut reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        let declarations = store.desired_subjects().unwrap();
        *runtime.ptys.lock().unwrap() = declarations
            .iter()
            .map(|declaration| RuntimeObservation {
                runtime_id: declaration.member.as_ref().unwrap().runtime_id.clone(),
                terminal: true,
                status: "running".into(),
                exit_code: None,
                incarnation_id: Some("incumbent".into()),
            })
            .collect();
        for declaration in &declarations {
            store
                .append_claim(&ClaimInput {
                    subject: declaration.subject.clone(),
                    kind: "harness.observed".into(),
                    actor: Some(declaration.subject.clone()),
                    fields: BTreeMap::from([
                        ("state".into(), Value::String("ready".into())),
                        ("driver".into(), Value::String("codex".into())),
                        ("incarnation_id".into(), Value::String("incumbent".into())),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        reconciler.skip_unneeded = true;
        reconciler.reconcile_once().unwrap();
        assert_eq!(reconciler.member_wakes.lock().unwrap().len(), count);
        reconciler.incremental.observe(&store).unwrap();
        // This budget measures a pass with no member/wake deadline armed. Keep the real read
        // dependencies, including negative reads; due-work correctness has separate controls.
        for declaration in &declarations {
            for item in [
                format!("member:{}", declaration.subject),
                format!("wake:{}@incumbent", declaration.subject),
            ] {
                let reads = reconciler.incremental.reads_of(&item);
                assert!(!reads.is_empty());
                reconciler
                    .incremental
                    .evaluated(&item, reads, Some(u128::MAX));
            }
        }
        let render_reads = reconciler.incremental.reads_of("render");
        reconciler
            .incremental
            .evaluated("render", render_reads, None);
        smallclaims::sqlite::histogram::take();
        let before = smallclaims::sqlite::work::total();
        reconciler.reconcile_once().unwrap();
        let cost = smallclaims::sqlite::work::total() - before;
        let receipt_reads: u64 = smallclaims::sqlite::histogram::take()
            .iter()
            .filter(|(sql, _)| sql.starts_with("SELECT id,subject,body,accepted_at_unix_ms FROM claims WHERE kind="))
            .map(|(_, shape)| shape.count)
            .sum();
        eprintln!("quiet owned roster={count}: {cost:?}, receipt_reads={receipt_reads}");
        assert!(
            (1..=12).contains(&receipt_reads),
            "quiet passes must not fence or prepare every unchanged member"
        );
        assert!(
            cost.statements <= 600 && cost.vm_steps <= 30_000,
            "quiet-pass absolute SQL work budget: {cost:?}"
        );
        assert!(runtime.stops.lock().unwrap().is_empty());
        assert_eq!(runtime.starts.lock().unwrap().len(), count);
        costs.push(cost);
    }
    assert!(
        costs[1].statements <= costs[0].statements + 8 * (64 - 8) + 32,
        "quiet SQL must not grow quadratically with owned members: {costs:?}"
    );
    assert!(
        costs[1].vm_steps <= costs[0].vm_steps + 400 * (64 - 8),
        "quiet VM work growth budget: {costs:?}"
    );
}

#[test]
fn an_ownership_change_after_pass_selection_skips_without_a_member_fault() {
    // Exercise both changed sites in the real pass, including a remote-away candidate.
    for host in ["node", "cobalt"] {
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_memory("node").unwrap());
        publish_owned_member(&store, workspace.path(), host, 10);
        let subject = "agent/garden/orchard";
        let old = store.desired_subjects().unwrap();
        assert!(
            store
                .owned_desired_subjects(&old)
                .unwrap()
                .contains(subject)
        );
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        let item = format!(
            "{}:{subject}",
            if host == "node" { "member" } else { "away" }
        );
        let prior_reads = BTreeSet::from(["exec:garden.orchard".to_owned()]);
        reconciler.incremental.evaluated(&item, prior_reads, None);
        let claims_after_publication = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let updated = store.clone();
        let count = claims_after_publication.clone();
        let path = workspace.path().to_path_buf();
        *runtime.before_observe_exec.lock().unwrap() = Some(Box::new(move || {
            // Exec polling is after pass eligibility/workspace/render and before member/away.
            assert!(
                updated
                    .owned_desired_subjects(&old)
                    .unwrap()
                    .contains(subject)
            );
            publish_owned_member(&updated, &path, host, 20);
            assert!(updated.owned_desired_guard(&old[0]).is_err());
            count.store(
                updated.claims_for(subject, None).unwrap().len(),
                Ordering::SeqCst,
            );
        }));
        reconciler.reconcile_once().unwrap();
        assert!(
            runtime.before_observe_exec.lock().unwrap().is_none(),
            "race hook ran"
        );
        assert!(claims_after_publication.load(Ordering::SeqCst) > 0);
        assert_eq!(
            store.claims_for(subject, None).unwrap().len(),
            claims_after_publication.load(Ordering::SeqCst),
            "guard rejection writes no member claim"
        );
        assert!(
            store
                .claims_for(subject, Some("runtime.reconcile-decision"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.member_reconcile_fault(subject, None).unwrap(), None);
        assert!(
            reconciler.incremental.reads_of(&item).is_empty(),
            "rejected item was not evaluated"
        );
        assert!(runtime.starts.lock().unwrap().is_empty());
        assert!(runtime.stops.lock().unwrap().is_empty());
        assert!(runtime.kills.lock().unwrap().is_empty());
        assert!(runtime.removes.lock().unwrap().is_empty());
    }
}

#[test]
fn a_pass_snapshot_read_error_fences_member_effects_and_reports_once() {
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let workspace = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open_memory("node").unwrap());
    publish_owned_member(&store, workspace.path(), "node", 10);
    let declaration = store.desired_subjects().unwrap().remove(0);
    assert!(
        store
            .owned_desired_subjects(std::slice::from_ref(&declaration))
            .unwrap()
            .contains(&declaration.subject)
    );
    let receipt = store.owned_sets().unwrap().remove(0);
    let accepted_at = store
        .connection
        .write()
        .query_row(
            "SELECT accepted_at_unix_ms FROM claims WHERE id=?1",
            [&receipt.claim],
            |row| row.get::<_, rusqlite::types::Value>(0),
        )
        .unwrap();
    store
        .connection
        .write()
        .execute(
            "UPDATE claims SET accepted_at_unix_ms='invalid' WHERE id=?1",
            [&receipt.claim],
        )
        .unwrap();
    assert!(
        store
            .owned_desired_subjects(std::slice::from_ref(&declaration))
            .is_err()
    );
    let runtime = Arc::new(FakeRuntime::default());
    let reconciler = Reconciler::new(
        store.clone(),
        runtime.clone(),
        "node".into(),
        Arc::new(Notify::new()),
    );
    let capture = Capture(Arc::new(Mutex::new(Vec::new())));
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::ERROR)
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let claims = store.claims_for(&declaration.subject, None).unwrap().len();
    tracing::subscriber::with_default(subscriber, || {
        for _ in 0..2 {
            // Exercise the real pass's snapshot Err arm, rather than calling its reporter.
            reconciler.reconcile_once().unwrap();
            assert!(runtime.starts.lock().unwrap().is_empty());
            assert!(runtime.stops.lock().unwrap().is_empty());
            assert!(runtime.kills.lock().unwrap().is_empty());
            assert!(runtime.removes.lock().unwrap().is_empty());
            assert_eq!(
                store.claims_for(&declaration.subject, None).unwrap().len(),
                claims
            );
            assert!(
                reconciler
                    .incremental
                    .reads_of(&format!("member:{}", declaration.subject),)
                    .is_empty()
            );
        }
    });
    let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert_eq!(
        output
            .matches("reconciler ownership read failed; candidates fenced")
            .count(),
        1
    );
    assert!(output.contains("snapshot"));
    assert!(reconciler.ownership_error_log.lock().unwrap().suppressed >= 1);
    assert_eq!(
        store
            .member_reconcile_fault(&declaration.subject, None)
            .unwrap(),
        None
    );
    // Repair the actual read failure: suppression must not cache a fenced candidate result.
    store
        .connection
        .write()
        .execute(
            "UPDATE claims SET accepted_at_unix_ms=?1 WHERE id=?2",
            rusqlite::params![accepted_at, receipt.claim],
        )
        .unwrap();
    reconciler.reconcile_once().unwrap();
    assert!(
        !runtime.starts.lock().unwrap().is_empty(),
        "a repaired snapshot permits supervision again"
    );
}
