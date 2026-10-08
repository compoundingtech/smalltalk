use super::*;

pub(crate) fn fixture() -> (Arc<Store>, String) {
    let store = Arc::new(Store::open_memory("node").unwrap());
    let steps = (0..20)
        .map(|index| format!("step \"work-{index}\" timeout=\"1h\" {{ title \"Investigate finding {index}\"; fresh-context }}\n"))
        .collect::<String>();
    apply_source(
        &store,
        &format!(
            "version 2\nmission \"evaluation-history\" state=\"ready\" {{\n goal \"Investigate and resolve the current findings.\"\n completion {{ when \"all-steps-exhausted\" }}\n queue \"findings\" {{ assigned-to \"person/test\"; {steps} }}\n}}"
        ),
        "evaluation-history-definition",
    );
    let run = store
        .create_mission_run(&crate::model::MissionRunRequest {
            mission: "evaluation-history".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "evaluation-history-run".into(),
        })
        .unwrap();
    for step in &run.steps {
        for index in 0..12 {
            store.append_claim(&ClaimInput {
                subject: step.subject.clone(),
                kind: "work.progress".into(),
                actor: Some("person/test".into()),
                fields: BTreeMap::from([
                    ("attempt".into(), Value::from(step.attempt)),
                    ("summary".into(), Value::String(format!("Inspected finding {index}; recorded evidence and remaining checks."))),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("history-{}-{index}", step.step)),
            }).unwrap();
        }
        store.append_claim(&ClaimInput {
            subject: step.subject.clone(),
            kind: "work.extended".into(),
            actor: Some("person/test".into()),
            fields: BTreeMap::from([
                ("attempt".into(), Value::from(step.attempt)),
                ("extend_ms".into(), Value::from(60_000)),
                ("reason".into(), Value::String("Additional evidence review".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("extension-{}", step.step)),
        }).unwrap();
    }
    (store, run.id)
}

// Pre-reuse baseline (debug test profile, 100 full evaluations): 5640.540 ms thread
// CPU, 56.405 ms/evaluation, 5689.020 ms wall. Reproduce before/after with:
// flock --close /tmp/misc-heavy-build.lock nice -n 19 ionice -c3 env CARGO_BUILD_JOBS=4
// /run/current-system/sw/bin/nix develop -c cargo test -p st3 --lib
// mission_run_evaluation_cpu_20_steps -- --ignored --nocapture --test-threads=1
#[test]
#[ignore = "CPU timing benchmark; run explicitly with --ignored --nocapture --test-threads=1"]
fn mission_run_evaluation_cpu_20_steps() {
    let (store, id) = fixture();
    let reconciler = Reconciler::new(
        store.clone(), Arc::new(FakeRuntime::default()), "node".into(), Arc::new(Notify::new()),
    );
    // Settle admission before timing; every sample still loads and fully evaluates the run,
    // rather than measuring the incremental scheduler's unchanged-run skip.
    for _ in 0..3 {
        let (run, mission) = store.mission_run_for_evaluation(&id).unwrap();
        reconciler.evaluate_active_mission_run(&run, mission).unwrap();
    }
    const SAMPLES: u32 = 100;
    let cpu_started = crate::incremental::thread_cpu();
    let started = std::time::Instant::now();
    for _ in 0..SAMPLES {
        let (run, mission) = store.mission_run_for_evaluation(&id).unwrap();
        assert_eq!(run.steps.len(), 20);
        std::hint::black_box(reconciler.evaluate_active_mission_run(&run, mission).unwrap());
    }
    let cpu = crate::incremental::thread_cpu().saturating_sub(cpu_started);
    println!("mission-run evaluation: steps=20 history=260 samples={SAMPLES} thread_cpu_ms={:.3} mean_cpu_ms={:.3} wall_ms={:.3}", cpu.as_secs_f64() * 1000.0, cpu.as_secs_f64() * 1000.0 / f64::from(SAMPLES), started.elapsed().as_secs_f64() * 1000.0);
}
