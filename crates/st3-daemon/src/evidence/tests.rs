use super::*;
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    key: PathBuf,
    trusted: PathBuf,
    base: String,
    head: String,
}

fn command(program: &str, args: &[&str]) {
    let output = Command::new(program).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn policy() -> Policy {
    Policy {
        version: 1,
        risk_class: "daemon".into(),
        required_checks: BTreeSet::from(["unit-tests".into()]),
        allowed_tiers: BTreeSet::from([TrustTier::Full]),
        test_signers: BTreeSet::from(["fleet/fixture-observer".into()]),
        review_signers: BTreeSet::from(["fleet/fixture-observer".into()]),
    }
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path();
        command("git", &["init", "-q", repo.to_str().unwrap()]);
        let key = repo.join("throwaway-key");
        command(
            "ssh-keygen",
            &["-q", "-t", "ed25519", "-N", "", "-f", key.to_str().unwrap()],
        );
        let trusted = repo.join("allowed-signers");
        let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
        std::fs::write(
            &trusted,
            format!("fleet/fixture-observer namespaces=\"git\" {public}"),
        )
        .unwrap();
        let mut fixture = Self {
            root,
            key,
            trusted,
            base: String::new(),
            head: String::new(),
        };
        fixture.base = fixture.revision("before", None);
        fixture.head = fixture.revision("after", Some(&fixture.base));
        fixture
    }

    fn repo(&self) -> &Path {
        self.root.path()
    }

    fn revision(&self, content: &str, parent: Option<&str>) -> String {
        let file = git_text(
            self.repo(),
            &["hash-object", "-w", "--stdin"],
            Some(content.as_bytes()),
        )
        .unwrap();
        let policy_blob = git_text(
            self.repo(),
            &["hash-object", "-w", "--stdin"],
            Some(&serde_json::to_vec(&policy()).unwrap()),
        )
        .unwrap();
        let policy_tree = git_text(
            self.repo(),
            &["mktree"],
            Some(format!("100644 blob {policy_blob}\tevidence-policy.json\n").as_bytes()),
        )
        .unwrap();
        let recipe = git_text(
            self.repo(),
            &["hash-object", "-w", "--stdin"],
            Some(b"fixture-ci-source-recipe"),
        )
        .unwrap();
        let scripts = git_text(
            self.repo(),
            &["mktree"],
            Some(format!("100644 blob {recipe}\tci-evidence-source\n").as_bytes()),
        )
        .unwrap();
        let workflow = git_text(
            self.repo(),
            &["hash-object", "-w", "--stdin"],
            Some(b"fixture-workflow"),
        )
        .unwrap();
        let workflows = git_text(
            self.repo(),
            &["mktree"],
            Some(format!("100644 blob {workflow}\tfleet.yml\n").as_bytes()),
        )
        .unwrap();
        let github = git_text(
            self.repo(),
            &["mktree"],
            Some(format!("040000 tree {workflows}\tworkflows\n").as_bytes()),
        )
        .unwrap();
        let tree = git_text(
            self.repo(),
            &["mktree"],
            Some(
                format!("040000 tree {github}\t.github\n040000 tree {policy_tree}\t.st\n100644 blob {file}\tcheck.rs\n040000 tree {scripts}\tscripts\n")
                    .as_bytes(),
            ),
        )
        .unwrap();
        self.commit_tree(&tree, parent)
    }

    fn commit_tree(&self, tree: &str, parent: Option<&str>) -> String {
        let mut args = vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit-tree",
            tree,
        ];
        if let Some(parent) = parent {
            args.extend(["-p", parent]);
        }
        git_text(self.repo(), &args, Some(b"fixture\n")).unwrap()
    }

    fn statement(&self, fact: Fact) -> Statement {
        Statement {
            statement_type: STATEMENT_TYPE.into(),
            subject: vec![Subject {
                name: self.head.clone(),
                digest: BTreeMap::from([(
                    "gitTree".into(),
                    tree(self.repo(), &self.head).unwrap(),
                )]),
            }],
            predicate_type: PREDICATE_TYPE.into(),
            predicate: Observation {
                schema_version: 1,
                change_id: "fixture-change".into(),
                signer: Some("fleet/fixture-observer".into()),
                tier: TrustTier::Full,
                re_attests: None,
                producer_sha256: "a".repeat(64),
                observed_at: chrono::Utc::now().to_rfc3339(),
                fact,
            },
        }
    }

    fn tests(&self, runner: &str, exit_code: i32) -> Statement {
        self.statement(Fact::Tests {
            inputs: BTreeMap::new(),
            runner: runner.into(),
            checks: BTreeMap::from([(
                "unit-tests".into(),
                Check {
                    command: vec!["cargo".into(), "test".into()],
                    exit_code,
                    output_sha256: "b".repeat(64),
                },
            )]),
        })
    }

    fn review(&self, tests: Vec<String>, verdict: Verdict) -> Statement {
        self.statement(Fact::Review {
            reviewer: "agent/fixture-reviewer".into(),
            verdict,
            base_commit: self.base.clone(),
            files: changed_files(self.repo(), &self.base, &self.head).unwrap(),
            tests,
        })
    }

    fn put(&self, statement: &Statement) -> String {
        let id = sign_observation(self.repo(), statement, &self.key).unwrap();
        put(self.repo(), &id, &self.trusted).unwrap();
        id
    }

    fn verify(&self, head: &str, base: &str) -> Result<Verified> {
        verify(
            self.repo(),
            head,
            base,
            "fixture-change",
            &self.trusted,
            &policy(),
        )
    }
}

#[test]
fn legacy_and_fractal_predicate_uris_verify_but_an_unknown_uri_is_rejected() {
    let f = Fixture::new();
    assert_eq!(PREDICATE_TYPE, "https://smalltalk.dev/evidence/gate/v1");
    for uri in [
        "https://smalltalk.dev/evidence/gate/v1",
        "https://compounding.tech/fractal/evidence/gate/v1",
    ] {
        let mut statement = f.tests("fixture-runner", 0);
        statement.predicate_type = uri.into();
        let record = f.put(&statement);
        assert_eq!(
            verify_record(f.repo(), &record, &f.trusted)
                .unwrap()
                .predicate_type,
            uri
        );
    }
    let mut statement = f.tests("fixture-runner", 0);
    statement.predicate_type = "https://example.invalid/evidence/gate/v1".into();
    assert_eq!(
        statement.validate(f.repo()).unwrap_err().to_string(),
        "unsupported evidence format"
    );
}

#[test]
fn tests_and_review_round_trip_and_gate() {
    let f = Fixture::new();
    let tests = f.put(&f.tests("fixture-runner", 0));
    let review = f.put(&f.review(vec![tests.clone()], Verdict::GO));
    assert!(matches!(
        show(f.repo(), &tests).unwrap().predicate.fact,
        Fact::Tests { .. }
    ));
    assert!(matches!(
        verify_record(f.repo(), &review, &f.trusted)
            .unwrap()
            .predicate
            .fact,
        Fact::Review {
            verdict: Verdict::GO,
            ..
        }
    ));
    let verified = f.verify(&f.head, &f.base).unwrap();
    assert_eq!(verified.tests, vec![tests]);
    assert_eq!(verified.review, review);
    assert_eq!(load_policy(f.repo(), &f.head).unwrap().risk_class, "daemon");
}

#[test]
fn forged_signature_missing_key_and_wrong_principal_are_rejected() {
    let f = Fixture::new();
    let mut missing_signer = f.tests("fixture-runner", 0);
    missing_signer.predicate.signer = None;
    assert!(sign_observation(f.repo(), &missing_signer, &f.key).is_err());
    let tests = sign_observation(f.repo(), &f.tests("fixture-runner", 0), &f.key).unwrap();
    let raw = git(f.repo(), &["cat-file", "commit", &tests], None).unwrap();
    // Tamper with signed content while retaining the original SSH signature.
    let forged = String::from_utf8(raw)
        .unwrap()
        .replace("st gate observation v1", "forged observation");
    let id = git_text(
        f.repo(),
        &["hash-object", "-t", "commit", "-w", "--stdin"],
        Some(forged.as_bytes()),
    )
    .unwrap();
    assert!(put(f.repo(), &id, &f.trusted).is_err());
    std::fs::write(&f.trusted, "").unwrap();
    assert!(verify_record(f.repo(), &tests, &f.trusted).is_err());
    let public = std::fs::read_to_string(f.key.with_extension("pub")).unwrap();
    std::fs::write(&f.trusted, format!("fleet/fixture-other {public}")).unwrap();
    assert!(verify_record(f.repo(), &tests, &f.trusted).is_err());
    assert!(records(f.repo()).unwrap().is_empty());
}

#[test]
fn changed_file_blob_needs_a_fresh_review() {
    let f = Fixture::new();
    let tests = f.put(&f.tests("fixture-runner", 0));
    f.put(&f.review(vec![tests], Verdict::GO));
    let changed = f.revision("changed again", Some(&f.head));
    assert!(f.verify(&changed, &f.base).is_err());
}

#[test]
fn clean_rebase_with_same_files_and_cited_tests_keeps_go() {
    let f = Fixture::new();
    let tests = f.put(&f.tests("fixture-runner", 0));
    let review = f.put(&f.review(vec![tests], Verdict::GO));
    let base_tree = tree(f.repo(), &f.base).unwrap();
    let new_base = f.commit_tree(&base_tree, Some(&f.base));
    let head_tree = tree(f.repo(), &f.head).unwrap();
    let rebased = f.commit_tree(&head_tree, Some(&new_base));
    assert_ne!(rebased, f.head);
    assert_eq!(f.verify(&rebased, &new_base).unwrap().review, review);
}

#[test]
fn missing_failed_or_changed_cited_tests_block_stage() {
    let f = Fixture::new();
    assert!(f.verify(&f.head, &f.base).is_err());
    let failed = f.put(&f.tests("fixture-runner", 1));
    f.put(&f.review(vec![failed], Verdict::GO));
    assert!(f.verify(&f.head, &f.base).is_err());
    let passing = f.put(&f.tests("fixture-runner-2", 0));
    f.put(&f.review(vec![passing.clone()], Verdict::GO));
    assert!(f.verify(&f.head, &f.base).is_ok());
    git(
        f.repo(),
        &["update-ref", "-d", &format!("{NOTES_PREFIX}{passing}")],
        None,
    )
    .unwrap();
    // A new successful test cannot silently substitute for the test a GO cites.
    f.put(&f.tests("fixture-runner-3", 0));
    assert!(f.verify(&f.head, &f.base).is_err());
}

#[test]
fn stop_wrong_change_and_unapproved_role_cannot_supply_go() {
    let f = Fixture::new();
    let tests = f.put(&f.tests("fixture-runner", 0));
    f.put(&f.review(vec![tests.clone()], Verdict::STOP));
    assert!(f.verify(&f.head, &f.base).is_err());
    let mut review = f.review(vec![tests.clone()], Verdict::GO);
    review.predicate.change_id = "fixture-other-change".into();
    f.put(&review);
    assert!(f.verify(&f.head, &f.base).is_err());
    f.put(&f.review(vec![tests], Verdict::GO));
    let mut p = policy();
    p.review_signers = BTreeSet::from(["fleet/fixture-other".into()]);
    assert!(verify(f.repo(), &f.head, &f.base, "fixture-change", &f.trusted, &p).is_err());
}

#[test]
fn partial_review_and_extra_current_paths_are_rejected() {
    let f = Fixture::new();
    let tests = f.put(&f.tests("fixture-runner", 0));
    let mut partial = f.review(vec![tests.clone()], Verdict::GO);
    if let Fact::Review { files, .. } = &mut partial.predicate.fact {
        files.clear();
    }
    assert!(sign_observation(f.repo(), &partial, &f.key).is_err());
    f.put(&f.review(vec![tests], Verdict::GO));
    // The wrong base makes additional paths part of the change: cannot reuse GO.
    let empty = git_text(f.repo(), &["mktree"], Some(b"")).unwrap();
    let other_base = f.commit_tree(&empty, None);
    assert!(f.verify(&f.head, &other_base).is_err());
}

#[test]
fn simultaneous_writers_keep_every_record_and_same_import_is_idempotent() {
    let f = Fixture::new();
    let ids: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..6)
            .map(|i| {
                let f = &f;
                scope.spawn(move || f.put(&f.tests(&format!("fixture-runner-{i}"), 0)))
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    assert_eq!(records(f.repo()).unwrap().len(), 6);
    git(
        f.repo(),
        &["update-ref", "-d", &format!("{NOTES_PREFIX}{}", ids[0])],
        None,
    )
    .unwrap();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..6)
            .map(|_| scope.spawn(|| put(f.repo(), &ids[0], &f.trusted).unwrap()))
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    });
    assert_eq!(records(f.repo()).unwrap().len(), 6);
    // Six concurrent first writers measured the process image once between them.
    assert_eq!(MEASUREMENTS.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn notes_push_and_fetch_carry_source_and_work_without_shared_daemon() {
    let f = Fixture::new();
    let tests = f.put(&f.tests("fixture-runner", 0));
    f.put(&f.review(vec![tests], Verdict::GO));
    let remote = tempfile::tempdir().unwrap();
    command(
        "git",
        &["init", "--bare", "-q", remote.path().to_str().unwrap()],
    );
    git(
        f.repo(),
        &[
            "push",
            remote.path().to_str().unwrap(),
            "refs/notes/evidence/*:refs/notes/evidence/*",
        ],
        None,
    )
    .unwrap();
    let peer = tempfile::tempdir().unwrap();
    command("git", &["init", "-q", peer.path().to_str().unwrap()]);
    git(
        peer.path(),
        &[
            "fetch",
            remote.path().to_str().unwrap(),
            "refs/notes/evidence/*:refs/notes/evidence/*",
        ],
        None,
    )
    .unwrap();
    assert!(
        verify(
            peer.path(),
            &f.head,
            &f.base,
            "fixture-change",
            &f.trusted,
            &load_policy(peer.path(), &f.head).unwrap()
        )
        .is_ok()
    );
}

#[test]
fn untrusted_repo_signature_config_and_replace_objects_do_not_grant_trust() {
    let f = Fixture::new();
    let tests = f.put(&f.tests("fixture-runner", 0));
    let fake = f.repo().join("fake-signature-program");
    std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
    git(
        f.repo(),
        &["config", "gpg.ssh.program", fake.to_str().unwrap()],
        None,
    )
    .unwrap();
    git(
        f.repo(),
        &[
            "config",
            "gpg.ssh.allowedSignersFile",
            f.trusted.to_str().unwrap(),
        ],
        None,
    )
    .unwrap();
    let no_keys = f.repo().join("no-keys");
    std::fs::write(&no_keys, "").unwrap();
    assert!(verify_record(f.repo(), &tests, &no_keys).is_err());
    let other = f.put(&f.tests("fixture-other-runner", 0));
    git(f.repo(), &["replace", &tests, &other], None).unwrap();
    let statement = verify_record(f.repo(), &tests, &f.trusted).unwrap();
    assert!(
        matches!(statement.predicate.fact, Fact::Tests { runner, .. } if runner == "fixture-runner")
    );
}

#[test]
fn policy_must_have_nonempty_checks_and_signer_roles() {
    let mut p = policy();
    p.required_checks.clear();
    assert!(p.validate().is_err());
    p = policy();
    p.test_signers.clear();
    assert!(p.validate().is_err());
    p = policy();
    p.risk_class = "unknown".into();
    assert!(p.validate().is_err());
}

#[test]
fn newer_stop_supersedes_go_and_a_second_fleet_key_can_be_added() {
    let f = Fixture::new();
    let tests = f.put(&f.tests("fixture-runner", 0));
    f.put(&f.review(vec![tests.clone()], Verdict::GO));
    assert!(f.verify(&f.head, &f.base).is_ok());
    f.put(&f.review(vec![tests.clone()], Verdict::STOP));
    assert!(f.verify(&f.head, &f.base).is_err());
    // A second throwaway key from the same fixture fleet stands in for a future peer.
    let peer_key = f.repo().join("throwaway-peer-key");
    command(
        "ssh-keygen",
        &[
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-f",
            peer_key.to_str().unwrap(),
        ],
    );
    let mut peer_go = f.review(vec![tests], Verdict::GO);
    peer_go.predicate.signer = Some("fleet/fixture-peer".into());
    let id = sign_observation(f.repo(), &peer_go, &peer_key).unwrap();
    assert!(put(f.repo(), &id, &f.trusted).is_err());
    use std::io::Write;
    let public = std::fs::read_to_string(peer_key.with_extension("pub")).unwrap();
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&f.trusted)
            .unwrap(),
        "fleet/fixture-peer namespaces=\"git\" {public}"
    )
    .unwrap();
    put(f.repo(), &id, &f.trusted).unwrap();
    let mut p = policy();
    // Cryptographic trust alone must not widen the allowed review role.
    assert!(f.verify(&f.head, &f.base).is_err());
    p.review_signers.insert("fleet/fixture-peer".into());
    assert_eq!(
        verify(f.repo(), &f.head, &f.base, "fixture-change", &f.trusted, &p)
            .unwrap()
            .review,
        id
    );
    // Removing a peer key after a mismatch immediately revokes its prior evidence.
    let public = std::fs::read_to_string(f.key.with_extension("pub")).unwrap();
    std::fs::write(
        &f.trusted,
        format!("fleet/fixture-observer namespaces=\"git\" {public}"),
    )
    .unwrap();
    assert!(verify_record(f.repo(), &id, &f.trusted).is_err());
    assert!(verify(f.repo(), &f.head, &f.base, "fixture-change", &f.trusted, &p).is_err());
}

#[test]
fn degraded_and_test_tiers_are_explicit_and_never_satisfy_daemon_policy() {
    let f = Fixture::new();
    let mut degraded = f.tests("fixture-runner", 0);
    degraded.predicate.tier = TrustTier::Degraded;
    degraded.predicate.signer = None;
    assert!(sign_observation(f.repo(), &degraded, &f.key).is_err());
    let id = record_degraded(f.repo(), &degraded).unwrap();
    put(f.repo(), &id, &f.trusted).unwrap();
    assert_eq!(
        verify_record(f.repo(), &id, &f.trusted)
            .unwrap()
            .predicate
            .tier,
        TrustTier::Degraded
    );
    f.put(&f.review(vec![id], Verdict::GO));
    assert!(f.verify(&f.head, &f.base).is_err());
    let mut test = f.tests("fixture-test-runner", 0);
    test.predicate.tier = TrustTier::Test;
    let id = f.put(&test);
    assert_eq!(
        verify_record(f.repo(), &id, &f.trusted)
            .unwrap()
            .predicate
            .tier,
        TrustTier::Test
    );
    f.put(&f.review(vec![id], Verdict::GO));
    assert!(f.verify(&f.head, &f.base).is_err());
    let mut p = policy();
    p.allowed_tiers.insert(TrustTier::Test);
    assert!(p.validate().is_err());
    p.allowed_tiers = BTreeSet::from([TrustTier::Degraded]);
    assert!(p.validate().is_err());
}

#[test]
fn fresh_full_rerun_re_attests_without_promoting_degraded_record() {
    let f = Fixture::new();
    let mut degraded = f.tests("fixture-runner", 0);
    degraded.predicate.tier = TrustTier::Degraded;
    degraded.predicate.signer = None;
    let prior = record_degraded(f.repo(), &degraded).unwrap();
    put(f.repo(), &prior, &f.trusted).unwrap();
    let mut rerun = f.tests("fixture-rerun", 0);
    rerun.predicate.re_attests = Some(prior.clone());
    let full = f.put(&rerun);
    f.put(&f.review(vec![full], Verdict::GO));
    assert_eq!(f.verify(&f.head, &f.base).unwrap().tier, TrustTier::Full);
    assert_eq!(
        show(f.repo(), &prior).unwrap().predicate.tier,
        TrustTier::Degraded
    );
    if let Fact::Tests { checks, .. } = &mut rerun.predicate.fact {
        checks
            .get_mut("unit-tests")
            .unwrap()
            .command
            .push("--different-input".into());
    }
    let forged_rerun = sign_observation(f.repo(), &rerun, &f.key).unwrap();
    assert!(put(f.repo(), &forged_rerun, &f.trusted).is_err());
}

#[test]
fn open_test_inputs_round_trip_but_do_not_authorize_scoped_reuse() {
    let f = Fixture::new();
    let mut tests = f.tests("fixture-runner", 0);
    if let Fact::Tests { inputs, .. } = &mut tests.predicate.fact {
        inputs.insert("crate/fixture".into(), "sha256:opaque-input-key".into());
    }
    let id = f.put(&tests);
    assert!(
        matches!(show(f.repo(), &id).unwrap().predicate.fact, Fact::Tests { inputs, .. } if inputs["crate/fixture"] == "sha256:opaque-input-key")
    );
    f.put(&f.review(vec![id], Verdict::GO));
    let changed = f.revision("changed source", Some(&f.head));
    assert!(f.verify(&changed, &f.base).is_err());
    let mut unknown = tests;
    unknown.predicate.schema_version = 2;
    assert!(sign_observation(f.repo(), &unknown, &f.key).is_err());
}

fn ci_observation(
    f: &Fixture,
) -> (
    observers::CiBinding,
    serde_json::Value,
    observers::SourceReceipt,
    Vec<serde_json::Value>,
) {
    use sha2::{Digest, Sha256};
    let recipe_digest = hex::encode(Sha256::digest(b"fixture-ci-source-recipe"));
    let binding = observers::CiBinding {
        repository: "example/project".into(),
        run: 123,
        attempt: 2,
        recipe_sha256: recipe_digest.clone(),
        accepted_base: f.base.clone(),
        workflow_sha256: hex::encode(Sha256::digest(b"fixture-workflow")),
        required_checks: BTreeSet::from(["unit-tests".into()]),
    };
    let run = serde_json::json!({
        "id":123, "run_attempt":2, "repository":{"full_name":"example/project"},
        "name":"Workspace CI", "path":".github/workflows/fleet.yml",
        "status":"completed", "event":"merge_group", "head_sha":f.head,
        "head_branch":format!("gh-readonly-queue/main/pr-7-{}", f.head),
        "pull_requests":[{"number":7}]
    });
    let receipt = observers::SourceReceipt {
        version: 1,
        repository: "example/project".into(),
        event: "merge_group".into(),
        run_id: "123".into(),
        run_attempt: "2".into(),
        source: f.head.clone(),
        tree: tree(f.repo(), &f.head).unwrap(),
        object_format: "sha1".into(),
        change_id: "github:example/project#7".into(),
        producer_sha256: recipe_digest,
        pack_sha256: "b".repeat(64),
        pack_bytes: 123,
    };
    let jobs = vec![
        serde_json::json!({"id":10, "run_id":123, "run_attempt":2, "name":"evidence-source",
            "status":"completed", "conclusion":"success", "steps":[
                {"name":"Retain effective tested source", "status":"completed", "conclusion":"success"},
                {"name":"Upload effective tested source", "status":"completed", "conclusion":"success"}]}),
        serde_json::json!({"id":11, "run_id":123, "run_attempt":2, "name":"unit-tests",
            "status":"completed", "conclusion":"success", "completed_at":"2026-10-12T12:00:00Z"}),
    ];
    (binding, run, receipt, jobs)
}

#[test]
fn ci_observer_signs_retained_merge_tree_and_reports_observed_job() {
    let f = Fixture::new();
    let (binding, run, receipt, jobs) = ci_observation(&f);
    let records = observers::produce_ci(
        f.repo(),
        &binding,
        &run,
        &receipt,
        &jobs,
        &observers::ObserverSigner::Full {
            principal: "fleet/fixture-observer",
            key: &f.key,
        },
    )
    .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].tier, TrustTier::Full);
    let statement = verify_record(f.repo(), &records[0].record, &f.trusted).unwrap();
    assert_eq!(statement.subject[0].name, f.head);
    assert_eq!(run["head_sha"], f.head); // Queue source is platform-bound.
    let Fact::Tests { runner, checks, .. } = statement.predicate.fact else {
        panic!()
    };
    assert!(runner.ends_with("/attempts/2/jobs/11"));
    assert_eq!(checks["unit-tests"].command[0], "github-actions");
    assert_eq!(checks["unit-tests"].exit_code, 0);
}

#[test]
fn ci_outage_is_degraded_and_replaying_the_same_job_cannot_promote_it() {
    let f = Fixture::new();
    let (binding, run, receipt, jobs) = ci_observation(&f);
    let degraded = observers::produce_ci(
        f.repo(),
        &binding,
        &run,
        &receipt,
        &jobs,
        &observers::ObserverSigner::Full {
            principal: "fleet/fixture-observer",
            key: &f.repo().join("absent"),
        },
    )
    .unwrap();
    assert_eq!(degraded[0].tier, TrustTier::Degraded);
    assert!(degraded[0].signing_unavailable);
    let restored = observers::produce_ci(
        f.repo(),
        &binding,
        &run,
        &receipt,
        &jobs,
        &observers::ObserverSigner::Full {
            principal: "fleet/fixture-observer",
            key: &f.key,
        },
    )
    .unwrap();
    assert_eq!(restored[0].record, degraded[0].record);
    assert_eq!(restored[0].tier, TrustTier::Degraded);
    assert!(verify_record(f.repo(), &restored[0].record, &f.trusted).is_ok());
    assert_eq!(records(f.repo()).unwrap().len(), 1);
}

#[test]
fn ci_throwaway_service_expires_and_test_tier_never_satisfies_production() {
    let f = Fixture::new();
    let (binding, run, receipt, jobs) = ci_observation(&f);
    let expired = observers::ObserverSigner::Test {
        principal: "fleet/fixture-observer",
        key: &f.key,
        expires_at: chrono::Utc::now() - chrono::Duration::seconds(1),
    };
    assert!(observers::produce_ci(f.repo(), &binding, &run, &receipt, &jobs, &expired).is_err());
    assert!(records(f.repo()).unwrap().is_empty());
    let active = observers::ObserverSigner::Test {
        principal: "fleet/fixture-observer",
        key: &f.key,
        expires_at: chrono::Utc::now() + chrono::Duration::minutes(5),
    };
    let tests = observers::produce_ci(f.repo(), &binding, &run, &receipt, &jobs, &active).unwrap();
    assert_eq!(tests[0].tier, TrustTier::Test);
    let mut review = f.review(vec![tests[0].record.clone()], Verdict::GO);
    review.predicate.change_id = receipt.change_id.clone();
    f.put(&review);
    assert!(
        verify(
            f.repo(),
            &f.head,
            &f.base,
            &receipt.change_id,
            &f.trusted,
            &policy()
        )
        .is_err()
    );
}

#[test]
fn ci_observer_rejects_wrong_provenance_and_duplicate_or_incomplete_checks() {
    let f = Fixture::new();
    let (binding, run, receipt, jobs) = ci_observation(&f);
    for (key, value) in [
        ("id", serde_json::json!(124)),
        ("run_attempt", serde_json::json!(1)),
        ("path", serde_json::json!(".github/workflows/other.yml")),
        ("status", serde_json::json!("in_progress")),
        (
            "repository",
            serde_json::json!({"full_name":"other/project"}),
        ),
    ] {
        let mut wrong = run.clone();
        wrong[key] = value;
        assert!(observers::ci_statements(f.repo(), &binding, &wrong, &receipt, &jobs).is_err());
    }
    for modify in 0..5 {
        let mut wrong = jobs.clone();
        match modify {
            0 => {
                wrong.pop();
            }
            1 => {
                wrong.push(jobs[1].clone());
            }
            2 => {
                wrong[1]["run_attempt"] = serde_json::json!(1);
            }
            3 => {
                wrong[1]["status"] = serde_json::json!("in_progress");
            }
            _ => {
                wrong[0]["steps"][0]["conclusion"] = serde_json::json!("failure");
            }
        }
        assert!(observers::ci_statements(f.repo(), &binding, &run, &receipt, &wrong).is_err());
    }
    let mut wrong_receipt = receipt;
    wrong_receipt.producer_sha256 = "c".repeat(64);
    assert!(observers::ci_statements(f.repo(), &binding, &run, &wrong_receipt, &jobs).is_err());
    assert!(records(f.repo()).unwrap().is_empty());
}

#[test]
fn ci_failed_and_skipped_checks_are_recorded_as_failures() {
    let f = Fixture::new();
    let (binding, run, receipt, mut jobs) = ci_observation(&f);
    for conclusion in ["failure", "cancelled", "timed_out", "neutral", "skipped"] {
        jobs[1]["conclusion"] = serde_json::json!(conclusion);
        let statements =
            observers::ci_statements(f.repo(), &binding, &run, &receipt, &jobs).unwrap();
        let Fact::Tests { checks, .. } = &statements[0].predicate.fact else {
            panic!()
        };
        assert_eq!(checks["unit-tests"].exit_code, 1);
    }
}

#[test]
fn pull_request_self_reported_tree_never_gets_full_trust_even_with_a_pinned_recipe() {
    let f = Fixture::new();
    let (binding, mut run, mut receipt, jobs) = ci_observation(&f);
    run["event"] = "pull_request".into();
    run["head_sha"] = f.head.clone().into();
    receipt.event = "pull_request".into();
    // The author retains an old clean tree, not the actual head. Every recipe byte matches.
    receipt.source = f.base.clone();
    receipt.tree = tree(f.repo(), &f.base).unwrap();
    assert!(receipt.validate(f.repo(), &binding, &run).is_ok());
    assert!(!binding.full_eligible(f.repo(), &receipt, &run).unwrap());
    assert!(
        observers::produce_ci(
            f.repo(),
            &binding,
            &run,
            &receipt,
            &jobs,
            &observers::ObserverSigner::Full {
                principal: "fleet/fixture-observer",
                key: &f.key
            }
        )
        .is_err()
    );
    let grade = observers::produce_ci(
        f.repo(),
        &binding,
        &run,
        &receipt,
        &jobs,
        &observers::ObserverSigner::Unavailable,
    )
    .unwrap();
    assert_eq!(grade[0].tier, TrustTier::Degraded);
    assert_eq!(
        verify_record(f.repo(), &grade[0].record, &f.trusted)
            .unwrap()
            .subject[0]
            .name,
        f.base
    );
}

#[test]
fn full_queue_trust_requires_exact_workflow_and_recipe_from_operator_accepted_base() {
    let f = Fixture::new();
    let (mut binding, mut run, mut receipt, jobs) = ci_observation(&f);
    assert!(binding.full_eligible(f.repo(), &receipt, &run).unwrap());
    let changed_workflow = git_text(
        f.repo(),
        &["hash-object", "-w", "--stdin"],
        Some(b"always report success"),
    )
    .unwrap();
    let workflows = git_text(
        f.repo(),
        &["mktree"],
        Some(format!("100644 blob {changed_workflow}\tfleet.yml\n").as_bytes()),
    )
    .unwrap();
    let github = git_text(
        f.repo(),
        &["mktree"],
        Some(format!("040000 tree {workflows}\tworkflows\n").as_bytes()),
    )
    .unwrap();
    let entries = git_text(
        f.repo(),
        &["ls-tree", &tree(f.repo(), &f.head).unwrap()],
        None,
    )
    .unwrap();
    let mut rows = entries
        .lines()
        .filter(|row| !row.ends_with("\t.github"))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    rows.push(format!("040000 tree {github}\t.github"));
    let tree = git_text(
        f.repo(),
        &["mktree"],
        Some(format!("{}\n", rows.join("\n")).as_bytes()),
    )
    .unwrap();
    receipt.source = f.commit_tree(&tree, Some(&f.head));
    receipt.tree = tree;
    run["head_sha"] = receipt.source.clone().into();
    // Platform source binding alone is insufficient when the jobs' workflow is changed.
    assert!(!binding.full_eligible(f.repo(), &receipt, &run).unwrap());
    assert!(
        observers::produce_ci(
            f.repo(),
            &binding,
            &run,
            &receipt,
            &jobs,
            &observers::ObserverSigner::Full {
                principal: "fleet/fixture-observer",
                key: &f.key
            }
        )
        .is_err()
    );
    // Caller-controlled producer claims or an unreviewed baseline cannot replace the operator pin.
    binding.accepted_base = receipt.source.clone();
    assert!(binding.full_eligible(f.repo(), &receipt, &run).is_err());
    binding.accepted_base = f.base.clone();
    binding.recipe_sha256 = "a".repeat(64);
    assert!(binding.full_eligible(f.repo(), &receipt, &run).is_err());
}

#[test]
fn malformed_configured_signer_is_rejected_before_any_observation_write() {
    let f = Fixture::new();
    let (binding, run, receipt, jobs) = ci_observation(&f);
    assert!(
        observers::produce_ci(
            f.repo(),
            &binding,
            &run,
            &receipt,
            &jobs,
            &observers::ObserverSigner::Full {
                principal: "invalid signer",
                key: &f.key
            }
        )
        .is_err()
    );
    assert!(records(f.repo()).unwrap().is_empty());
}

#[test]
fn protected_reattest_links_only_authenticated_fresh_same_keyed_check_and_measures_issuance() {
    let f = Fixture::new();
    let (binding, run, receipt, mut jobs) = ci_observation(&f);
    let now = chrono::Utc::now();
    jobs[1]["started_at"] = (now - chrono::Duration::minutes(11)).to_rfc3339().into();
    jobs[1]["completed_at"] = (now - chrono::Duration::minutes(10)).to_rfc3339().into();
    let old = observers::produce_ci(
        f.repo(),
        &binding,
        &run,
        &receipt,
        &jobs,
        &observers::ObserverSigner::Unavailable,
    )
    .unwrap();
    let old_id = old[0].record.clone();
    let predecessors = vec![(old_id.clone(), run.clone(), jobs[1].clone())];
    let mut next_binding = binding;
    next_binding.attempt = 3;
    let mut next_run = run;
    next_run["run_attempt"] = 3.into();
    let mut next_receipt = receipt;
    next_receipt.run_attempt = "3".into();
    let mut next_jobs = jobs.clone();
    for job in &mut next_jobs {
        job["run_attempt"] = 3.into();
    }
    next_jobs[1]["id"] = 12.into();
    next_jobs[1]["started_at"] = (now - chrono::Duration::minutes(4)).to_rfc3339().into();
    next_jobs[1]["completed_at"] = (now - chrono::Duration::minutes(1)).to_rfc3339().into();
    let statements = observers::ci_statements(
        f.repo(),
        &next_binding,
        &next_run,
        &next_receipt,
        &next_jobs,
    )
    .unwrap();
    let links = ci_reattest::prepare(
        f.repo(),
        &next_binding.repository,
        &statements,
        &next_jobs,
        &predecessors,
    )
    .unwrap();
    let signed = observers::produce_ci_linked(
        f.repo(),
        &next_binding,
        &next_run,
        &next_receipt,
        &next_jobs,
        &observers::ObserverSigner::Full {
            principal: "fleet/fixture-observer",
            key: &f.key,
        },
        &links,
    )
    .unwrap();
    let full = verify_record(f.repo(), &signed[0].record, &f.trusted).unwrap();
    assert_eq!(full.predicate.re_attests, Some(old_id.clone()));
    assert_eq!(
        show(f.repo(), &old_id).unwrap().predicate.tier,
        TrustTier::Degraded
    );
    let metrics = ci_reattest::measurements(f.repo(), &signed).unwrap();
    // Includes the minute since rerun completion, not merely the nine-minute check gap.
    assert!(metrics[0]["elapsed_ms"].as_i64().unwrap() >= 599_000);
    let replay = observers::produce_ci_linked(
        f.repo(),
        &next_binding,
        &next_run,
        &next_receipt,
        &next_jobs,
        &observers::ObserverSigner::Full {
            principal: "fleet/fixture-observer",
            key: &f.key,
        },
        &links,
    )
    .unwrap();
    assert!(
        ci_reattest::measurements(f.repo(), &replay)
            .unwrap()
            .is_empty()
    );
    // The original degraded job can never itself supply a re-attestation.
    let old_statements = observers::ci_statements(
        f.repo(),
        &next_binding,
        &next_run,
        &next_receipt,
        &next_jobs,
    )
    .unwrap();
    let mut same_execution = predecessors.clone();
    same_execution[0].2["output"] = "tampered after completion".into();
    assert!(
        ci_reattest::prepare(
            f.repo(),
            &next_binding.repository,
            &old_statements,
            &next_jobs,
            &same_execution
        )
        .is_err()
    );
    let mut wrong_workflow = predecessors.clone();
    wrong_workflow[0].1["path"] = ".github/workflows/other.yml".into();
    assert!(
        ci_reattest::prepare(
            f.repo(),
            &next_binding.repository,
            &statements,
            &next_jobs,
            &wrong_workflow
        )
        .is_err()
    );
    let mut bad_start = next_jobs.clone();
    bad_start[1]["started_at"] = (now - chrono::Duration::minutes(12)).to_rfc3339().into();
    assert!(
        ci_reattest::prepare(
            f.repo(),
            &next_binding.repository,
            &statements,
            &bad_start,
            &predecessors
        )
        .is_err()
    );
    let mut other_tree = statements.clone();
    other_tree[0].subject[0]
        .digest
        .insert("gitTree".into(), tree(f.repo(), &f.base).unwrap());
    assert!(
        ci_reattest::prepare(
            f.repo(),
            &next_binding.repository,
            &other_tree,
            &next_jobs,
            &predecessors
        )
        .is_err()
    );
    let mut other_command = statements.clone();
    if let Fact::Tests { checks, .. } = &mut other_command[0].predicate.fact {
        checks
            .get_mut("unit-tests")
            .unwrap()
            .command
            .push("different".into());
    }
    assert!(
        ci_reattest::prepare(
            f.repo(),
            &next_binding.repository,
            &other_command,
            &next_jobs,
            &predecessors
        )
        .is_err()
    );
    let mut other_inputs = statements.clone();
    if let Fact::Tests { inputs, .. } = &mut other_inputs[0].predicate.fact {
        inputs.insert("new-input".into(), "digest".into());
    }
    assert!(
        ci_reattest::prepare(
            f.repo(),
            &next_binding.repository,
            &other_inputs,
            &next_jobs,
            &predecessors
        )
        .is_err()
    );
    let mut same_runner = statements.clone();
    if let Fact::Tests { runner, .. } = &mut same_runner[0].predicate.fact {
        *runner = format!(
            "github:{}/actions/runs/123/attempts/2/jobs/11",
            next_binding.repository
        );
    }
    assert!(
        ci_reattest::prepare(
            f.repo(),
            &next_binding.repository,
            &same_runner,
            &jobs,
            &predecessors
        )
        .is_err()
    );
}

#[test]
fn fresh_rerun_signing_outage_keeps_degraded_tier_and_does_not_attach_or_promote_link() {
    let f = Fixture::new();
    let (mut binding, mut run, mut receipt, mut jobs) = ci_observation(&f);
    let now = chrono::Utc::now();
    jobs[1]["started_at"] = (now - chrono::Duration::minutes(3)).to_rfc3339().into();
    jobs[1]["completed_at"] = (now - chrono::Duration::minutes(2)).to_rfc3339().into();
    let old = observers::produce_ci(
        f.repo(),
        &binding,
        &run,
        &receipt,
        &jobs,
        &observers::ObserverSigner::Unavailable,
    )
    .unwrap();
    let predecessors = vec![(old[0].record.clone(), run.clone(), jobs[1].clone())];
    binding.attempt = 3;
    run["run_attempt"] = 3.into();
    receipt.run_attempt = "3".into();
    for job in &mut jobs {
        job["run_attempt"] = 3.into();
    }
    jobs[1]["id"] = 12.into();
    jobs[1]["started_at"] = (now - chrono::Duration::minutes(1)).to_rfc3339().into();
    jobs[1]["completed_at"] = now.to_rfc3339().into();
    let statements = observers::ci_statements(f.repo(), &binding, &run, &receipt, &jobs).unwrap();
    let links = ci_reattest::prepare(
        f.repo(),
        &binding.repository,
        &statements,
        &jobs,
        &predecessors,
    )
    .unwrap();
    let grade = observers::produce_ci_linked(
        f.repo(),
        &binding,
        &run,
        &receipt,
        &jobs,
        &observers::ObserverSigner::Full {
            principal: "fleet/fixture-observer",
            key: &f.repo().join("missing-key"),
        },
        &links,
    )
    .unwrap();
    assert_eq!(grade[0].tier, TrustTier::Degraded);
    assert!(
        show(f.repo(), &grade[0].record)
            .unwrap()
            .predicate
            .re_attests
            .is_none()
    );
    let repeated = observers::produce_ci_linked(
        f.repo(),
        &binding,
        &run,
        &receipt,
        &jobs,
        &observers::ObserverSigner::Full {
            principal: "fleet/fixture-observer",
            key: &f.key,
        },
        &links,
    )
    .unwrap();
    assert_eq!(repeated[0].record, grade[0].record);
    assert_eq!(repeated[0].tier, TrustTier::Degraded);
    assert!(
        ci_reattest::measurements(f.repo(), &repeated)
            .unwrap()
            .is_empty()
    );
}
