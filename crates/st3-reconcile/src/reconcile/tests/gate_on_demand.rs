//! Round-local gate seats are admitted by real step readiness, rather than round creation.
use super::*;

    const SOURCE: &str = r#"version 2
mission "orchid" state="ready" {
  goal "Refine a garden."
  loop "refine" {
    max-rounds 2
    round {
      completion { when "all-steps-exhausted" }
      agent "builder" { workspace "/tmp"; command "true"; restart "never" }
      agent "gate" { workspace "/tmp"; command "true"; restart "never" }
      agent "helper" { workspace "/tmp"; command "true"; restart "never" }
      step "build" { assigned-to "agent/${ST_MISSION_RUN}/builder" }
      step "gate" {
        assigned-to "agent/${ST_MISSION_RUN}/gate"
        depends-on { step "build" completed }
        retry { attempts 2 }
      }
    }
  }
}"#;

    struct Fixture {
    store: Arc<Store>,
    runtime: Arc<FakeRuntime>,
    reconciler: Reconciler<FakeRuntime>,
    round: MissionRunView,
}

    impl Fixture {
    fn new(source: &str) -> Self {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(&store, source, "publish");
        let parent = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "orchid".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/lichen".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "garden".into(),
            })
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        )
        .skipping_unneeded(true);
        Self::passes(&reconciler, &runtime);
        let child = store.mission_run_subject_for_idempotency_key(&format!(
            "loop-round:{}:1",
            parent.steps[0].subject
        ));
        let round = store.mission_run(&child).unwrap().unwrap();
        let fixture = Self {
            store,
            runtime,
            reconciler,
            round,
        };
        fixture.settle();
        fixture
    }

    fn settle(&self) {
        Self::passes(&self.reconciler, &self.runtime);
    }

    fn passes(reconciler: &Reconciler<FakeRuntime>, runtime: &FakeRuntime) {
        for _ in 0..8 {
            let members = runtime.started_members.lock().unwrap().clone();
            let stops = runtime.stops.lock().unwrap().clone();
            let mut ptys = runtime.ptys.lock().unwrap();
            for observation in ptys.iter_mut() {
                if stops.contains(&observation.runtime_id) {
                    observation.status = "stopped".into();
                }
            }
            for member in members {
                if !stops.contains(&member.runtime_id)
                    && !ptys.iter().any(|p| p.runtime_id == member.runtime_id)
                {
                    ptys.push(RuntimeObservation {
                        runtime_id: member.runtime_id.clone(),
                        terminal: true,
                        status: "running".into(),
                        exit_code: None,
                        incarnation_id: Some(format!("{}-one", member.runtime_id)),
                    });
                }
            }
            drop(ptys);
            reconciler.reconcile_once().unwrap();
        }
    }

    fn step(&self, path: &str) -> crate::model::StepRunView {
        self.store
            .mission_run(&self.round.id)
            .unwrap()
            .unwrap()
            .steps
            .into_iter()
            .find(|s| s.step == path)
            .unwrap()
    }

    fn action(&self, path: &str, action: &str, key: &str) -> crate::model::StepRunView {
        let step = self.step(path);
        let actor = step.assigned_to.unwrap();
        let member = self
            .runtime
            .started_members
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.environment.get("ST_AGENT").map(String::as_str) == Some(actor.as_str()))
            .unwrap()
            .clone();
        self.store
            .work_action(
                &step.subject,
                action,
                &crate::model::WorkRequest {
                    wait_ms: None,
                    actor: Some(actor),
                    incarnation: Some(format!("{}-one", member.runtime_id)),
                    summary: Some("The work is done.".into()),
                    reason: Some("Try another review.".into()),
                    evidence: Vec::new(),
                    idempotency_key: key.into(),
                },
            )
            .unwrap()
    }

    fn gate_starts(&self) -> usize {
        let gate = format!("agent/{}/gate", self.round.id);
        self.runtime
            .started_members
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.environment.get("ST_AGENT").map(String::as_str) == Some(gate.as_str()))
            .count()
    }

    fn finish_build(&self) {
        self.action("build", "claim", "claim-build");
        self.action("build", "complete", "complete-build");
        self.settle();
    }

    fn restart(&mut self) {
        self.reconciler = Reconciler::new(
            self.store.clone(),
            self.runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        )
        .skipping_unneeded(true);
        self.settle();
    }

    fn revise(&self, key: &str) {
        let source = SOURCE.replace(
            "      agent \"helper\"",
            &format!(
                "      resource \"{key}\" {{ kind \"custom.test.garden\" }}\n      agent \"helper\""
            ),
        );
        apply_source(&self.store, &source, key);
        let next = self
            .store
            .mission_spec("__st3/orchid/loop/refine/round", None)
            .unwrap()
            .unwrap();
        self.store
            .adopt_mission_revision(
                &self.round.id,
                &next,
                "person/lichen",
                "Add garden evidence.",
                &format!("adopt-{key}"),
            )
            .unwrap();
        self.settle();
    }
}

    #[test]
fn gate_on_demand_waits_for_builder_then_starts_once_and_receives_work() {
    let fixture = Fixture::new(SOURCE);
    fixture.action("build", "claim", "claim-build");
    fixture.settle();
    assert_eq!(fixture.step("gate").status, "pending");
    assert_eq!(
        fixture.gate_starts(),
        0,
        "the gate must not occupy a seat during build"
    );
    assert_eq!(
        fixture.runtime.started_members.lock().unwrap().len(),
        2,
        "builder and unrelated helper remain eager"
    );
    fixture.action("build", "complete", "complete-build");
    fixture.settle();
    assert_eq!(fixture.step("gate").status, "ready");
    assert_eq!(fixture.gate_starts(), 1);
    let gate = format!("agent/{}/gate", fixture.round.id);
    let readiness = fixture
        .store
        .claims_for(&fixture.step("gate").subject, Some("step-run.state"))
        .unwrap()
        .into_iter()
        .find(|claim| claim.body.pointer("/fields/status").and_then(Value::as_str) == Some("ready"))
        .unwrap();
    let declaration = fixture
        .store
        .claims_for(&gate, Some("intent.desired"))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(
        readiness.store_index < declaration.store_index,
        "durable readiness precedes gate desire, including across a crash"
    );
    assert!(
        fixture
            .store
            .work(Some(&gate), false)
            .unwrap()
            .iter()
            .any(|s| s.subject == fixture.step("gate").subject)
    );
    assert_eq!(
        fixture
            .action("gate", "claim", "claim-gate")
            .claimant
            .as_deref(),
        Some(gate.as_str())
    );
    fixture.settle();
    assert_eq!(fixture.gate_starts(), 1);
}

    #[test]
fn gate_on_demand_restart_and_revision_preserve_build_and_gate_admission() {
    let mut fixture = Fixture::new(SOURCE);
    fixture.action("build", "claim", "claim-build");
    fixture.restart();
    fixture.revise("mid-build");
    fixture.restart();
    assert_eq!(fixture.gate_starts(), 0);
    assert_eq!(fixture.step("gate").status, "pending");
    fixture.action("build", "complete", "complete-build");
    fixture.settle();
    assert_eq!(fixture.step("gate").status, "ready");
    fixture.restart();
    assert_eq!(fixture.gate_starts(), 1);
    fixture.action("gate", "claim", "claim-gate");
    fixture.revise("mid-gate");
    fixture.restart();
    assert_eq!(fixture.step("gate").status, "claimed");
    assert_eq!(fixture.gate_starts(), 1);
    let desired = fixture
        .store
        .desired_subjects_named(&[format!("agent/{}/gate", fixture.round.id)])
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        desired.owner_generation,
        Some(
            fixture
                .store
                .mission_run(&fixture.round.id)
                .unwrap()
                .unwrap()
                .generation
        )
    );
    assert!(
        !fixture
            .runtime
            .stops
            .lock()
            .unwrap()
            .contains(&desired.member.unwrap().runtime_id)
    );
}

    #[test]
fn gate_on_demand_restart_restores_ready_step_without_an_existing_seat() {
    let mut fixture = Fixture::new(SOURCE);
    // Persist readiness as if the daemon ended between the readiness and seat writes.
    fixture
        .store
        .set_step_state(&fixture.step("build").subject, "completed", None)
        .unwrap();
    fixture
        .store
        .set_step_state(&fixture.step("gate").subject, "ready", None)
        .unwrap();
    fixture.restart();
    assert_eq!(fixture.gate_starts(), 1);
    assert_eq!(fixture.step("gate").status, "ready");
    fixture.action("gate", "claim", "restored-gate-claim");
}

    #[test]
fn gate_on_demand_release_and_failed_retry_keep_the_seat() {
    let mut fixture = Fixture::new(SOURCE);
    fixture.finish_build();
    fixture.action("gate", "claim", "claim-gate");
    fixture.action("gate", "release", "release-gate");
    fixture.restart();
    assert_eq!(fixture.step("gate").status, "ready");
    fixture.action("gate", "claim", "claim-gate-again");
    fixture.action("gate", "fail", "fail-gate");
    fixture.restart();
    assert_eq!(fixture.step("gate").attempt, 2);
    assert_eq!(fixture.step("gate").status, "ready");
    fixture.action("gate", "claim", "claim-retried-gate");
    assert_eq!(fixture.gate_starts(), 1);
    assert!(fixture.runtime.stops.lock().unwrap().is_empty());
}

    #[test]
fn gate_on_demand_revision_during_pending_retry_keeps_admitted_seat() {
    let mut fixture = Fixture::new(SOURCE);
    fixture.finish_build();
    fixture.action("gate", "claim", "claim-gate");
    fixture.action("gate", "fail", "fail-gate");
    fixture
        .store
        .retry_step(
            &fixture.step("gate").subject,
            "Review again after the backoff.",
            60_000,
        )
        .unwrap();
    assert_eq!(fixture.step("gate").status, "pending");
    fixture.revise("mid-retry");
    fixture.restart();
    assert_eq!(fixture.step("gate").status, "pending");
    assert_eq!(fixture.step("gate").attempt, 2);
    let gate = format!("agent/{}/gate", fixture.round.id);
    let desired = fixture
        .store
        .desired_subjects_named(&[gate])
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(desired.kind, "agent");
    assert_eq!(
        desired.owner_generation,
        Some(
            fixture
                .store
                .mission_run(&fixture.round.id)
                .unwrap()
                .unwrap()
                .generation
        )
    );
    assert_eq!(fixture.gate_starts(), 1);
    assert!(fixture.runtime.stops.lock().unwrap().is_empty());
    fixture
        .store
        .set_step_state(&fixture.step("gate").subject, "ready", None)
        .unwrap();
    fixture.settle();
    fixture.action("gate", "claim", "claim-after-revision");
    assert_eq!(fixture.gate_starts(), 1);
}

    #[test]
fn gate_on_demand_cancelled_build_leaves_no_gate_or_stop_declaration() {
    let fixture = Fixture::new(SOURCE);
    fixture
        .store
        .request_mission_run_cancellation(&fixture.round.id, "End the garden round.")
        .unwrap();
    fixture.settle();
    assert_eq!(fixture.gate_starts(), 0);
    assert_eq!(
        fixture
            .store
            .selected_desired_kind(&format!("agent/{}/gate", fixture.round.id))
            .unwrap(),
        None
    );
    assert_eq!(
        fixture
            .store
            .mission_run(&fixture.round.id)
            .unwrap()
            .unwrap()
            .status,
        "cancelled"
    );
}

    #[test]
fn gate_on_demand_failed_build_leaves_no_gate_or_stop_declaration() {
    let fixture = Fixture::new(SOURCE);
    fixture.action("build", "claim", "claim-build");
    fixture.action("build", "fail", "fail-build");
    fixture.settle();
    assert_eq!(fixture.gate_starts(), 0);
    assert_eq!(
        fixture
            .store
            .selected_desired_kind(&format!("agent/{}/gate", fixture.round.id))
            .unwrap(),
        None
    );
    assert_eq!(
        fixture
            .store
            .mission_run(&fixture.round.id)
            .unwrap()
            .unwrap()
            .status,
        "failed"
    );
}

    #[test]
fn gate_on_demand_dependency_predicate_blocks_seat_admission() {
    let source = SOURCE.replace(
        "depends-on { step \"build\" completed }",
        "depends-on { step \"build\" completed; field \"state\" \"resource/garden-ready\" is \"ready\" }",
    );
    let fixture = Fixture::new(&source);
    fixture.finish_build();
    assert_eq!(fixture.step("gate").status, "pending");
    assert_eq!(fixture.gate_starts(), 0);
    fixture
        .store
        .append_claim(&crate::model::ClaimInput {
            subject: "resource/garden-ready".into(),
            kind: "resource.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("kind".into(), Value::String("custom.test.garden".into())),
                ("state".into(), Value::String("ready".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("review-ready".into()),
        })
        .unwrap();
    fixture.settle();
    assert_eq!(fixture.step("gate").status, "ready");
    assert_eq!(fixture.gate_starts(), 1);
}

    #[test]
fn gate_on_demand_kdl_handoffs_can_address_the_deferred_declaration() {
    let handoff = r#"message "handoff" {
        from "builder"
        to "gate"
        content "Review the garden."
      }"#;
    let sources = [
        SOURCE.replace(
            "      step \"build\"",
            &format!("      {handoff}\n      step \"build\""),
        ),
        SOURCE.replace(
            "step \"build\" { assigned-to \"agent/${ST_MISSION_RUN}/builder\" }",
            &format!(
                "step \"build\" {{ assigned-to \"agent/${{ST_MISSION_RUN}}/builder\"; {handoff} }}"
            ),
        ),
    ];
    for source in sources {
        let fixture = Fixture::new(&source);
        fixture.action("build", "claim", "claim-build");
        fixture.settle();
        assert_eq!(fixture.gate_starts(), 0);
        assert_eq!(fixture.step("gate").status, "pending");
        let gate = format!("agent/{}/gate", fixture.round.id);
        assert!(
            fixture
                .store
                .messages(Some(&gate), false)
                .unwrap()
                .iter()
                .any(|m| m.content == "Review the garden.")
        );
        fixture.action("build", "complete", "complete-build");
        fixture.settle();
        assert_eq!(fixture.gate_starts(), 1);
        assert_eq!(fixture.step("gate").status, "ready");
    }
}

    #[test]
fn gate_on_demand_other_kdl_recipients_still_require_delivery() {
    let source = SOURCE.replace(
        "step \"build\" { assigned-to \"agent/${ST_MISSION_RUN}/builder\" }",
        r#"step "build" {
        assigned-to "agent/${ST_MISSION_RUN}/builder"
        message "handoff" { from "builder"; to "helper"; content "Check the garden." }
      }"#,
    );
    let fixture = Fixture::new(&source);
    fixture.finish_build();
    assert_eq!(fixture.step("build").status, "verifying");
    assert_eq!(fixture.step("gate").status, "pending");
    assert_eq!(fixture.gate_starts(), 0);
    let helper = format!("agent/{}/helper", fixture.round.id);
    let handoff = fixture
        .store
        .messages(Some(&helper), false)
        .unwrap()
        .into_iter()
        .find(|m| m.content == "Check the garden.")
        .unwrap();
    fixture
        .store
        .append_claim(&crate::model::ClaimInput {
            subject: handoff.subject,
            kind: "message.delivered".into(),
            actor: Some(helper),
            fields: BTreeMap::from([("status".into(), Value::String("delivered".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    fixture.settle();
    assert_eq!(fixture.step("build").status, "completed");
    assert_eq!(fixture.step("gate").status, "ready");
    assert_eq!(fixture.gate_starts(), 1);
}

    #[test]
fn gate_on_demand_public_preview_keeps_undeclared_recipient_validation() {
    let fixture = Fixture::new(SOURCE);
    let source = r#"version 2
message "gate-handoff" { from "builder"; to "gate"; content "Review the garden." }
message "missing-handoff" { from "builder"; to "missing"; content "Check the garden." }
"#;
    let intent = crate::graph::parse_execution_intent(source, "node", &fixture.round.id).unwrap();
    let preview = fixture
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    assert_eq!(
        preview.blockers.len(),
        2,
        "public preview still requires known recipients"
    );
    let declared = BTreeSet::from([format!("agent/{}/gate", fixture.round.id)]);
    let refused = fixture
        .store
        .apply_internal_with_declared_agents(&intent, "missing-handoff", &declared)
        .unwrap_err();
    assert_eq!(refused.code, "internal-mission-blocked");
    assert!(refused.message.contains("missing"));
    assert!(!refused.message.contains("gate-handoff"));
}

    #[test]
fn gate_on_demand_shared_pooled_root_and_non_gate_seats_stay_eager() {
    let sources = [
        SOURCE.replace("agent/${ST_MISSION_RUN}/builder", "agent/${ST_MISSION_RUN}/gate"),
        SOURCE.replace("assigned-to \"agent/${ST_MISSION_RUN}/gate\"", "available-to \"agent/${ST_MISSION_RUN}/gate\""),
        SOURCE.replace("step \"gate\"", "step \"review\""),
        SOURCE.replace("  loop \"refine\"", "  agent \"standing-gate\" { workspace \"/tmp\"; command \"true\"; restart \"never\" }\n  loop \"refine\"")
            .replace("assigned-to \"agent/${ST_MISSION_RUN}/gate\"", "assigned-to \"agent/${ST_ROOT_MISSION_RUN_ID}/standing-gate\""),
    ];
    for source in sources {
        let fixture = Fixture::new(&source);
        assert_eq!(
            fixture.gate_starts(),
            1,
            "only the exclusive round gate may be deferred: {source}"
        );
    }
}

    #[test]
fn gate_on_demand_baseline_predicate_blocks_seat_admission() {
    let source = SOURCE.replace("        retry { attempts 2 }", "        baseline \"review-ready\" { field \"state\" \"resource/garden-ready\" is \"ready\" }\n        retry { attempts 2 }");
    let fixture = Fixture::new(&source);
    fixture.finish_build();
    assert_eq!(fixture.step("gate").status, "blocked");
    assert_eq!(fixture.gate_starts(), 0);
    fixture
        .store
        .append_claim(&crate::model::ClaimInput {
            subject: "resource/garden-ready".into(),
            kind: "resource.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("kind".into(), Value::String("custom.test.garden".into())),
                ("state".into(), Value::String("ready".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("review-ready".into()),
        })
        .unwrap();
    fixture.settle();
    assert_eq!(fixture.step("gate").status, "ready");
    assert_eq!(fixture.gate_starts(), 1);
}
