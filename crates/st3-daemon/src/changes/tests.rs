use super::transport::{Authentication, Repository, validate_advertisement};
use super::*;
use std::path::Path;
use std::process::Command;

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

struct Fixture {
    _root: tempfile::TempDir,
    path: std::path::PathBuf,
    remote: std::path::PathBuf,
    repo: Repository,
    record: Record,
}

impl Fixture {
    fn new(trailer: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("work");
        let remote = root.path().join("origin.git");
        std::fs::create_dir(&path).unwrap();
        std::fs::create_dir(&remote).unwrap();
        git(&path, &["init", "-q", "-b", "main"]);
        git(&remote, &["init", "--bare", "-q"]);
        git(
            &path,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(
            &path,
            &["commit", "--allow-empty", "-q", "-m", "Synthetic base"],
        );
        let base = git(&path, &["rev-parse", "HEAD"]);
        let id = new_change_id().unwrap();
        std::fs::write(path.join("source.txt"), "one\n").unwrap();
        git(&path, &["add", "source.txt"]);
        let message = if trailer {
            format!("Synthetic change\n\nChange-Id: {id}")
        } else {
            "Synthetic external change".into()
        };
        git(&path, &["commit", "-q", "-m", &message]);
        let repo = Repository::open(&path, Authentication::default()).unwrap();
        let (source, tree) = repo.source("HEAD").unwrap();
        let record = Record {
            schema_version: 1,
            change_id: id,
            generation: 1,
            previous_record: None,
            source_commit: source,
            source_tree: tree,
            base_commit: base,
            trailer_status: TrailerStatus::Present,
            state: State::Draft,
            next: None,
            depends_on: vec![],
            actor: "agent/fixture/builder".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            claim: None,
            mirror: None,
        };
        Self {
            _root: root,
            path,
            remote,
            repo,
            record,
        }
    }

    fn create(&self) -> Revision {
        let revision = self.repo.prepare(self.record.clone()).unwrap();
        self.repo.publish("origin", None, &revision).unwrap();
        revision
    }

    fn ready(&self) -> Revision {
        let old = self.create();
        let ready = self
            .repo
            .prepare_state(
                &old,
                "agent/fixture/builder",
                State::Ready,
                Some("review".into()),
            )
            .unwrap();
        self.repo.publish("origin", Some(&old), &ready).unwrap();
        ready
    }
}

fn work(worker: &str) -> WorkBinding {
    WorkBinding {
        step: format!("step-run/fixture/{worker}"),
        claim_id: format!("claim-{worker}"),
        incarnation: "fixture-incarnation".into(),
    }
}

#[test]
fn state_transitions_are_atomic_and_terminal_current_retains_source_history() {
    let fixture = Fixture::new(true);
    git(&fixture.path, &["config", "push.followTags", "true"]);
    git(
        &fixture.path,
        &["tag", "-a", "fixture-release", "-m", "Synthetic release"],
    );
    let initial = fixture.create();
    assert!(git(&fixture.remote, &["for-each-ref", "refs/tags"]).is_empty());
    let mut changed_base = transport::successor(&initial, "agent/fixture/builder").unwrap();
    changed_base.base_commit = changed_base.source_commit.clone();
    assert!(fixture.repo.prepare(changed_base).is_err());
    let ready = fixture
        .repo
        .prepare_state(
            &initial,
            "agent/fixture/builder",
            State::Ready,
            Some("review".into()),
        )
        .unwrap();
    fixture
        .repo
        .publish("origin", Some(&initial), &ready)
        .unwrap();
    let advertisement = fixture.repo.discover("origin").unwrap();
    validate_advertisement(&advertisement, &ready).unwrap();
    assert_eq!(advertisement.refs.len(), 4);
    let abandoned = fixture
        .repo
        .prepare_state(&ready, "agent/fixture/builder", State::Abandoned, None)
        .unwrap();
    fixture
        .repo
        .publish("origin", Some(&ready), &abandoned)
        .unwrap();
    let advertisement = fixture.repo.discover("origin").unwrap();
    validate_advertisement(&advertisement, &abandoned).unwrap();
    assert_eq!(advertisement.refs.len(), 1);
    // Fetch only current; its parents retain the otherwise unbranched source/history.
    let clone = fixture._root.path().join("fresh.git");
    std::fs::create_dir(&clone).unwrap();
    git(&clone, &["init", "--bare", "-q"]);
    let current = format!("{PREFIX}{}/current", ready.record.change_id);
    git(
        &clone,
        &["fetch", "-q", fixture.remote.to_str().unwrap(), &current],
    );
    git(&clone, &["cat-file", "-e", &initial.oid]);
    git(&clone, &["cat-file", "-e", &initial.record.source_commit]);
}

#[test]
fn two_concurrent_claims_have_one_winner_and_release_fences_a_stale_worker() {
    let fixture = Fixture::new(true);
    let ready = fixture.ready();
    let first = fixture
        .repo
        .prepare_claim(&ready, "agent/fixture/one", work("one"))
        .unwrap();
    let second = fixture
        .repo
        .prepare_claim(&ready, "agent/fixture/two", work("two"))
        .unwrap();
    assert_ne!(first.oid, second.oid);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = [&first, &second]
            .into_iter()
            .map(|candidate| {
                let barrier = barrier.clone();
                let repo = &fixture.repo;
                let ready = &ready;
                scope.spawn(move || {
                    barrier.wait();
                    repo.publish("origin", Some(ready), candidate).is_ok()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|&&ok| ok).count(), 1);
    let winner = if results[0] { first } else { second };
    validate_advertisement(&fixture.repo.discover("origin").unwrap(), &winner).unwrap();
    assert!(
        fixture
            .repo
            .prepare_state(&winner, "agent/fixture/other", State::Draft, None)
            .is_err()
    );
    assert!(
        fixture
            .repo
            .prepare_claim(&winner, "agent/fixture/other", work("other"))
            .is_err()
    );
    let claim = winner.record.claim.as_ref().unwrap();
    assert!(
        fixture
            .repo
            .prepare_release(&winner, &claim.worker, "wrong-nonce", None)
            .is_err()
    );
    let release = fixture
        .repo
        .prepare_release(&winner, &claim.worker, &claim.nonce, Some("review".into()))
        .unwrap();
    fixture
        .repo
        .publish_release("origin", &winner, &release, &claim.worker, &claim.nonce)
        .unwrap();
    let stale_finish = fixture
        .repo
        .prepare_release(&winner, &claim.worker, &claim.nonce, Some("land".into()))
        .unwrap();
    assert!(
        fixture
            .repo
            .publish_release(
                "origin",
                &winner,
                &stale_finish,
                &claim.worker,
                &claim.nonce
            )
            .is_err()
    );
    validate_advertisement(&fixture.repo.discover("origin").unwrap(), &release).unwrap();
}

#[test]
fn a_stale_revision_cannot_create_a_claim_and_a_no_op_current_is_rejected() {
    let fixture = Fixture::new(true);
    let old = fixture.ready();
    let next = fixture
        .repo
        .prepare_state(
            &old,
            "agent/fixture/builder",
            State::Ready,
            Some("tests".into()),
        )
        .unwrap();
    fixture.repo.publish("origin", Some(&old), &next).unwrap();
    let stale = fixture
        .repo
        .prepare_claim(&old, "agent/fixture/reviewer", work("reviewer"))
        .unwrap();
    assert!(fixture.repo.publish("origin", Some(&old), &stale).is_err());
    assert!(fixture.repo.publish("origin", Some(&next), &next).is_err());
    validate_advertisement(&fixture.repo.discover("origin").unwrap(), &next).unwrap();
}

#[test]
fn missing_trailer_requires_explicit_import_or_recovery_and_duplicates_fail() {
    let mut fixture = Fixture::new(false);
    assert_eq!(fixture.repo.trailer("HEAD").unwrap(), None);
    assert!(fixture.repo.prepare(fixture.record.clone()).is_err());
    fixture.record.trailer_status = TrailerStatus::ExternalMissing;
    let imported = fixture.repo.prepare(fixture.record.clone()).unwrap();
    assert_eq!(
        imported.record.source_commit,
        fixture.repo.source("HEAD").unwrap().0
    );
    // Explicit recovery supplies the already selected ID; no tree matching guesses.
    git(
        &fixture.path,
        &[
            "commit",
            "--amend",
            "--no-edit",
            "-q",
            "--trailer",
            &format!("Change-Id: {}", fixture.record.change_id),
        ],
    );
    let (source, tree) = fixture.repo.source("HEAD").unwrap();
    assert_ne!(source, imported.record.source_commit);
    assert_eq!(tree, imported.record.source_tree);
    fixture.record.source_commit = source;
    fixture.record.trailer_status = TrailerStatus::Present;
    fixture.repo.prepare(fixture.record.clone()).unwrap();
    let message = format!(
        "Synthetic duplicate\n\nChange-Id: {}\nChange-Id: {}",
        fixture.record.change_id, fixture.record.change_id
    );
    git(&fixture.path, &["commit", "--amend", "-q", "-m", &message]);
    assert!(fixture.repo.trailer("HEAD").is_err());
}

#[test]
fn a_dropped_trailer_or_different_tree_cannot_validate_owned_metadata() {
    let fixture = Fixture::new(true);
    let mut record = fixture.record.clone();
    record.source_tree = record.base_commit.clone();
    assert!(fixture.repo.prepare(record).is_err());
    git(
        &fixture.path,
        &["commit", "--amend", "-q", "-m", "Synthetic missing trailer"],
    );
    let (source, tree) = fixture.repo.source("HEAD").unwrap();
    let mut record = fixture.record.clone();
    record.source_commit = source;
    record.source_tree = tree;
    assert!(fixture.repo.prepare(record).is_err());
}

#[test]
fn missing_atomic_support_or_conflicting_markers_never_report_a_claim() {
    let fixture = Fixture::new(true);
    let ready = fixture.ready();
    git(
        &fixture.remote,
        &["config", "receive.advertiseAtomic", "false"],
    );
    let claim = fixture
        .repo
        .prepare_claim(&ready, "agent/fixture/reviewer", work("reviewer"))
        .unwrap();
    assert!(
        fixture
            .repo
            .publish("origin", Some(&ready), &claim)
            .is_err()
    );
    validate_advertisement(&fixture.repo.discover("origin").unwrap(), &ready).unwrap();
    let extra = format!("{PREFIX}{}/state/draft", ready.record.change_id);
    git(
        &fixture.path,
        &["push", "-q", "origin", &format!("{}:{extra}", ready.oid)],
    );
    assert!(validate_advertisement(&fixture.repo.discover("origin").unwrap(), &ready).is_err());
}

#[test]
fn explicit_sync_populates_tracking_objects_without_overwriting_active_local_refs() {
    let fixture = Fixture::new(true);
    let ready = fixture.ready();
    let clone = fixture._root.path().join("other");
    std::fs::create_dir(&clone).unwrap();
    git(&clone, &["init", "-q"]);
    git(
        &clone,
        &["remote", "add", "origin", fixture.remote.to_str().unwrap()],
    );
    let repo = Repository::open(&clone, Authentication::default()).unwrap();
    assert!(repo.load(&ready.oid).is_err());
    repo.sync("origin").unwrap();
    assert_eq!(repo.load(&ready.oid).unwrap(), ready);
    assert!(git(&clone, &["for-each-ref", PREFIX]).is_empty());
    assert!(!git(&clone, &["for-each-ref", "refs/st/tracking/"]).is_empty());
}

#[test]
fn source_rebase_and_amend_preserve_identity_but_record_exact_content() {
    let fixture = Fixture::new(true);
    std::fs::write(fixture.path.join("source.txt"), "two\n").unwrap();
    git(&fixture.path, &["add", "source.txt"]);
    git(&fixture.path, &["commit", "--amend", "--no-edit", "-q"]);
    assert_eq!(
        fixture.repo.trailer("HEAD").unwrap(),
        Some(fixture.record.change_id.clone())
    );
    assert_ne!(
        fixture.repo.source("HEAD").unwrap().1,
        fixture.record.source_tree
    );
    git(&fixture.path, &["branch", "topic"]);
    git(&fixture.path, &["checkout", "-q", "main"]);
    git(
        &fixture.path,
        &["reset", "--hard", "-q", &fixture.record.base_commit],
    );
    std::fs::write(fixture.path.join("unrelated.txt"), "unrelated\n").unwrap();
    git(&fixture.path, &["add", "unrelated.txt"]);
    git(
        &fixture.path,
        &["commit", "-q", "-m", "Synthetic unrelated base"],
    );
    git(&fixture.path, &["checkout", "-q", "topic"]);
    git(&fixture.path, &["rebase", "main"]);
    assert_eq!(
        fixture.repo.trailer("HEAD").unwrap(),
        Some(fixture.record.change_id)
    );
}

#[test]
fn invalid_record_identity_lane_claim_and_history_fail_before_publication() {
    let fixture = Fixture::new(true);
    let mut record = fixture.record.clone();
    record.next = Some("../review".into());
    assert!(record.validate().is_err());
    record = fixture.record.clone();
    record.depends_on = vec![record.change_id.clone()];
    assert!(record.validate().is_err());
    record = fixture.record.clone();
    record.generation = 2;
    assert!(record.validate().is_err());
    let initial = fixture.create();
    let mut record = transport::successor(&initial, "agent/fixture/builder").unwrap();
    record.change_id = new_change_id().unwrap();
    assert!(fixture.repo.prepare(record).is_err());
    for id in [
        "fixture",
        "i0123456789abcdef0123456789abcdef01234567",
        "I0123456789ABCDEF0123456789abcdef01234567",
    ] {
        assert!(change_id(id).is_err());
    }
    change_id(&new_change_id().unwrap()).unwrap();
}

#[test]
fn checkout_url_rewrites_cannot_execute_ext_helpers() {
    let fixture = Fixture::new(true);
    let sentinel = fixture._root.path().join("unexpected-helper");
    let rewritten = format!("url.ext::sh -c touch {}.insteadOf", sentinel.display());
    git(
        &fixture.path,
        &["config", &rewritten, fixture.remote.to_str().unwrap()],
    );
    assert!(fixture.repo.discover("origin").is_err());
    assert!(!sentinel.exists());
}

#[test]
fn checkout_rewrites_to_another_https_host_are_rejected_before_network_access() {
    let fixture = Fixture::new(true);
    let remote = "https://example.invalid/fixture/repo.git";
    git(&fixture.path, &["remote", "set-url", "origin", remote]);
    for key in ["insteadOf", "pushInsteadOf"] {
        let rewrite = format!("url.https://another.invalid/.{key}");
        git(
            &fixture.path,
            &["config", &rewrite, "https://example.invalid/"],
        );
        let error = fixture.repo.discover("origin").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("remote URL rewrite is not permitted")
        );
        git(&fixture.path, &["config", "--unset", &rewrite]);
    }
}

#[test]
fn generic_preparation_cannot_publish_landing_or_reopen_terminal_work() {
    let fixture = Fixture::new(true);
    let draft = fixture.create();
    let mut record = transport::successor(&draft, "agent/fixture/builder").unwrap();
    record.state = State::Landed;
    let landed = fixture.repo.prepare(record).unwrap();
    assert!(
        fixture
            .repo
            .publish("origin", Some(&draft), &landed)
            .is_err()
    );
    validate_advertisement(&fixture.repo.discover("origin").unwrap(), &draft).unwrap();
    let mut initial = fixture.record.clone();
    initial.state = State::Landed;
    let initial = fixture.repo.prepare(initial).unwrap();
    assert!(fixture.repo.publish("origin", None, &initial).is_err());
    for state in [State::Abandoned, State::Landed] {
        let mut record = transport::successor(&draft, "agent/fixture/builder").unwrap();
        record.state = state;
        let terminal = fixture.repo.prepare(record).unwrap();
        // Simulate a foreign writer's terminal observation. Reading its data does
        // not grant an ordinary publisher authority to reopen it.
        let current = format!("{PREFIX}{}/current", terminal.record.change_id);
        git(
            &fixture.path,
            &[
                "push",
                "-q",
                "--force",
                "origin",
                &format!("{}:{current}", terminal.oid),
            ],
        );
        for state in [State::Draft, State::Ready] {
            let mut record = transport::successor(&terminal, "agent/fixture/builder").unwrap();
            record.state = state;
            let reopened = fixture.repo.prepare(record).unwrap();
            assert!(
                fixture
                    .repo
                    .publish("origin", Some(&terminal), &reopened)
                    .is_err()
            );
            assert_eq!(
                fixture.repo.discover("origin").unwrap().refs[&current],
                terminal.oid
            );
        }
    }
}

#[test]
fn generic_preparation_cannot_claim_a_different_lane_or_draft_or_changed_content() {
    let fixture = Fixture::new(true);
    let ready = fixture.ready();
    let claim = fixture
        .repo
        .prepare_claim(&ready, "agent/fixture/one", work("one"))
        .unwrap();
    let mut wrong_lane = claim.record.clone();
    wrong_lane.claim.as_mut().unwrap().lane = "build".into();
    let wrong_lane = fixture.repo.prepare(wrong_lane).unwrap();
    assert!(
        fixture
            .repo
            .publish("origin", Some(&ready), &wrong_lane)
            .is_err()
    );
    let mut changed_payload = claim.record.clone();
    changed_payload.depends_on.push(new_change_id().unwrap());
    let changed_payload = fixture.repo.prepare(changed_payload).unwrap();
    assert!(
        fixture
            .repo
            .publish("origin", Some(&ready), &changed_payload)
            .is_err()
    );
    std::fs::write(fixture.path.join("source.txt"), "changed before claim\n").unwrap();
    git(&fixture.path, &["add", "source.txt"]);
    git(
        &fixture.path,
        &[
            "commit",
            "-q",
            "-m",
            &format!(
                "Synthetic revision\n\nChange-Id: {}",
                ready.record.change_id
            ),
        ],
    );
    let mut changed_source = claim.record.clone();
    let (source, tree) = fixture.repo.source("HEAD").unwrap();
    changed_source.source_commit = source.clone();
    changed_source.source_tree = tree.clone();
    changed_source.claim.as_mut().unwrap().expected_source = source;
    changed_source.claim.as_mut().unwrap().expected_tree = tree;
    let changed_source = fixture.repo.prepare(changed_source).unwrap();
    assert!(
        fixture
            .repo
            .publish("origin", Some(&ready), &changed_source)
            .is_err()
    );
    let draft = fixture
        .repo
        .prepare_state(&ready, "agent/fixture/builder", State::Draft, None)
        .unwrap();
    fixture
        .repo
        .publish("origin", Some(&ready), &draft)
        .unwrap();
    let mut claimed_draft = transport::successor(&draft, "agent/fixture/one").unwrap();
    claimed_draft.state = State::Ready;
    claimed_draft.claim = claim.record.claim.clone();
    let claimed_draft = fixture.repo.prepare(claimed_draft).unwrap();
    assert!(
        fixture
            .repo
            .publish("origin", Some(&draft), &claimed_draft)
            .is_err()
    );
    validate_advertisement(&fixture.repo.discover("origin").unwrap(), &draft).unwrap();
}

#[test]
fn generic_release_needs_worker_nonce_and_cannot_change_state_or_retarget_claim() {
    let fixture = Fixture::new(true);
    let ready = fixture.ready();
    let claimed = fixture
        .repo
        .prepare_claim(&ready, "agent/fixture/one", work("one"))
        .unwrap();
    fixture
        .repo
        .publish("origin", Some(&ready), &claimed)
        .unwrap();
    let claim = claimed.record.claim.as_ref().unwrap();
    let release = fixture
        .repo
        .prepare_release(&claimed, &claim.worker, &claim.nonce, Some("review".into()))
        .unwrap();
    assert!(
        fixture
            .repo
            .publish("origin", Some(&claimed), &release)
            .is_err()
    );
    assert!(
        fixture
            .repo
            .publish_release(
                "origin",
                &claimed,
                &release,
                "agent/fixture/two",
                &claim.nonce
            )
            .is_err()
    );
    assert!(
        fixture
            .repo
            .publish_release("origin", &claimed, &release, &claim.worker, "wrong-nonce")
            .is_err()
    );
    let mut foreign_release = release.record.clone();
    foreign_release.actor = "agent/fixture/two".into();
    let foreign_release = fixture.repo.prepare(foreign_release).unwrap();
    assert!(
        fixture
            .repo
            .publish_release(
                "origin",
                &claimed,
                &foreign_release,
                &claim.worker,
                &claim.nonce
            )
            .is_err()
    );
    let mut changed_state = release.record.clone();
    changed_state.state = State::Draft;
    changed_state.next = None;
    let changed_state = fixture.repo.prepare(changed_state).unwrap();
    assert!(
        fixture
            .repo
            .publish_release(
                "origin",
                &claimed,
                &changed_state,
                &claim.worker,
                &claim.nonce
            )
            .is_err()
    );
    let mut retargeted = transport::successor(&claimed, &claim.worker).unwrap();
    retargeted.claim.as_mut().unwrap().lane = "build".into();
    let retargeted = fixture.repo.prepare(retargeted).unwrap();
    assert!(
        fixture
            .repo
            .publish("origin", Some(&claimed), &retargeted)
            .is_err()
    );
    validate_advertisement(&fixture.repo.discover("origin").unwrap(), &claimed).unwrap();
    fixture
        .repo
        .publish_release("origin", &claimed, &release, &claim.worker, &claim.nonce)
        .unwrap();
    validate_advertisement(&fixture.repo.discover("origin").unwrap(), &release).unwrap();
}
