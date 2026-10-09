//! The published attention list against the full attention computation: every window it
//! serves (every person's rows, each person's, and a person with none) equals
//! `client_attention_resources_at` at the publication's cut and clock, through asks, answers,
//! custom requests, replication in either order, rollback, reopen, checkpoint and projection
//! changes; and claims attention never reads publish the same rows without a fold.
use super::*;
use crate::api::{client_attention_resources_at, published_attention_rows, refresh_attention_list};
use crate::model::PersonStepResponse;
use crate::store::person_work::tests::fixture;

const PEOPLE: &[&str] = &["person/avery", "person/robin", "person/lichen"];

fn input(subject: &str, kind: &str, actor: Option<&str>, fields: Value) -> ClaimInput {
    ClaimInput {
        subject: subject.into(),
        kind: kind.into(),
        actor: actor.map(str::to_owned),
        fields: serde_json::from_value(fields).unwrap(),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    }
}

fn custom(store: &Store, subject: &str, person: &str) {
    let manifest =
        serde_json::from_str(include_str!("../../../../../examples/st3/custom-review.json"))
            .unwrap();
    store
        .register_custom_kind(&crate::store::custom::RegistrationRequest {
            manifest,
            actor: "agent/garden/seed".into(),
        })
        .unwrap();
    store
        .append_claim(&input(
            subject,
            "custom.garden.review.v1.requested",
            Some("agent/garden/seed"),
            json!({"title":"Choose a seed","detail":"Keep or discard","recipient":person}),
        ))
        .unwrap();
}

fn folds(store: &Store) -> u64 {
    store.attention_list_folds().values().sum()
}

fn newest(store: &Store) -> Arc<AttentionPublication> {
    store.newest_attention_list().1.expect("a publication")
}

/// Refresh, then check every window the publication serves against the full computation at the
/// same cut and clock.
fn refresh_and_check(store: &Store) -> Arc<AttentionPublication> {
    refresh_attention_list(store).unwrap();
    let publication = newest(store);
    store
        .read_snapshot(|index| {
            assert_eq!(index, publication.cut, "published at the current cut");
            for person in PEOPLE.iter().copied().map(Some).chain([None]) {
                let full = client_attention_resources_at(
                    store,
                    person,
                    false,
                    publication.evaluated_at_unix_ms,
                )?;
                let (cut, _, rows) =
                    published_attention_rows(store, index, person, usize::MAX).expect("published");
                assert_eq!(cut, publication.cut);
                assert_eq!(rows, full, "the window for {person:?}");
            }
            Ok(())
        })
        .unwrap();
    publication
}

fn answer(store: &Store, subject: &str, actor: &str, key: &str) {
    store
        .finish_person_step(
            &PersonStepResponse {
                delegation: None,
                subject: subject.into(),
                actor: actor.into(),
                summary: "Reviewed".into(),
                evidence: vec![],
                episode: None,
                idempotency_key: key.into(),
                answer: None,
            },
            false,
        )
        .unwrap();
}

#[test]
fn asks_and_answers_match_the_full_list_for_every_person_at_each_cut() {
    let (store, _, request) = fixture();
    store.start_attention_list_refresher().unwrap();
    let first = refresh_and_check(&store);
    let ask = store.ask_person(&request).unwrap();
    let asked = refresh_and_check(&store);
    assert!(asked.cut > first.cut);
    assert!(asked.rows.iter().any(|row| row["source_id"] == ask.subject));
    custom(&store, "custom/garden/review/v1/alpha", "person/lichen");
    let requested = refresh_and_check(&store);
    assert!(requested.rows.iter().any(|row| row["person_id"] == "person/lichen"));
    answer(&store, &ask.subject, "person/avery", "release-answer");
    let answered = refresh_and_check(&store);
    assert!(answered.rows.iter().all(|row| row["source_id"] != ask.subject));
    // The first fold, then one per change attention reads.
    assert_eq!(folds(&store), 4);
}

#[test]
fn claims_attention_never_reads_republish_the_same_rows_without_a_fold() {
    let (store, _, request) = fixture();
    store.start_attention_list_refresher().unwrap();
    store.ask_person(&request).unwrap();
    let before = refresh_and_check(&store);
    let folded = folds(&store);
    let mut revisions = store.subscribe_attention_list();
    revisions.borrow_and_update();
    store
        .append_claim(&input(
            "message/seed-note",
            "message.sent",
            Some("person/robin"),
            json!({"from":"person/robin","to":"agent/alder.asker","content":"A note","status":"sent"}),
        ))
        .unwrap();
    store
        .append_claim(&input(
            "agent/alder.asker",
            "harness.usage",
            Some("agent/alder.asker"),
            json!({"driver":"codex","incarnation_id":"current","semantics":"response","total_tokens":7}),
        ))
        .unwrap();
    store
        .append_claim(&input("host/one", "transport.observed", None, json!({"status":"up"})))
        .unwrap();
    // A seat that was never asked to log in and is not retiring: neither its harness nor its
    // runtime observations reach attention.
    store
        .append_claim(&input(
            "agent/alder.asker",
            "runtime.observed",
            None,
            json!({"status":"running","runtime_id":"asker","incarnation_id":"one"}),
        ))
        .unwrap();
    store
        .append_claim(&input(
            "agent/alder.asker",
            "harness.observed",
            Some("agent/alder.asker"),
            json!({"state":"idle","incarnation_id":"one","driver":"claude"}),
        ))
        .unwrap();
    let after = refresh_and_check(&store);
    assert!(after.cut > before.cut);
    assert!(Arc::ptr_eq(&after.rows, &before.rows), "the same rows");
    assert_eq!(folds(&store), folded, "no fold");
    assert!(!revisions.has_changed().unwrap(), "nothing for a window to reread");
    // A login-shaped observation makes the seat a login candidate: that folds.
    store
        .append_claim(&input(
            "agent/alder.asker",
            "harness.observed",
            Some("agent/alder.asker"),
            json!({"state":"needs-login","incarnation_id":"one","driver":"claude"}),
        ))
        .unwrap();
    refresh_and_check(&store);
    assert_eq!(folds(&store), folded + 1);
    assert_eq!(
        store.attention_list_folds().get("harness.observed"),
        Some(&1),
        "folded for that observation"
    );
}

#[test]
fn custom_requests_converge_whichever_order_replication_delivers_them() {
    let left = Store::open_memory("alder").unwrap();
    let right = Store::open_memory("birch").unwrap();
    let at = now_ms() + 60_000;
    for (store, person) in [(&left, "person/lichen"), (&right, "person/avery")] {
        store.set_write_clock_at(at).unwrap();
        custom(store, "custom/garden/review/v1/concurrent", person);
    }
    custom(&left, "custom/garden/review/v1/left-only", "person/robin");
    let first = Store::open_memory("cedar").unwrap();
    let second = Store::open_memory("elm").unwrap();
    for store in [&first, &second] {
        store.start_attention_list_refresher().unwrap();
        refresh_and_check(store);
    }
    // Each receiver publishes after every exchange, in its own order.
    for (target, sources) in [(&first, [&left, &right]), (&second, [&right, &left])] {
        for source in sources {
            target
                .import_replication(&source.origin, &source.export_replication(0).unwrap())
                .unwrap();
            refresh_and_check(target);
        }
    }
    let (first, second) = (newest(&first), newest(&second));
    assert!(!first.rows.is_empty());
    assert_eq!(first.rows, second.rows);
}

#[test]
fn a_rolled_back_write_changes_nothing_published() {
    let (store, _, request) = fixture();
    store.start_attention_list_refresher().unwrap();
    let ask = store.ask_person(&request).unwrap();
    let before = refresh_and_check(&store);
    {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        tx.execute("UPDATE step_runs SET status='cancelled' WHERE subject=?1", [&ask.subject])
            .unwrap();
        tx.rollback().unwrap();
    }
    let after = refresh_and_check(&store);
    assert_eq!(after.rows, before.rows);
    assert!(after.rows.iter().any(|row| row["source_id"] == ask.subject));
}

#[test]
fn a_reopened_store_publishes_the_same_rows_from_nothing() {
    let (source, _, request) = fixture();
    source.ask_person(&request).unwrap();
    custom(&source, "custom/garden/review/v1/persisted", "person/lichen");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("attention.sqlite3");
    let target = Store::open(&path, "birch").unwrap();
    target
        .import_replication(&source.origin, &source.export_replication(0).unwrap())
        .unwrap();
    target.start_attention_list_refresher().unwrap();
    let before = refresh_and_check(&target);
    assert!(before.rows.len() >= 2);
    drop(target);
    let target = Store::open(&path, "birch").unwrap();
    target.start_attention_list_refresher().unwrap();
    assert!(target.newest_attention_list().1.is_none(), "nothing survives a restart");
    assert!(published_attention_rows(&target, target.index().unwrap(), None, 10).is_none());
    let after = refresh_and_check(&target);
    assert_eq!(after.rows, before.rows);
}

#[test]
fn a_checkpoint_forgets_the_list_and_a_refresh_from_before_it_cannot_publish() {
    let store = Store::open_memory("alder").unwrap();
    store.start_attention_list_refresher().unwrap();
    custom(&store, "custom/garden/review/v1/checkpoint", "person/lichen");
    for number in 0..3 {
        store
            .append_claim(&input(
                "resource/garden/checkpoint",
                "resource.observed",
                None,
                json!({"kind":"custom.garden.checkpoint","observer":"observer/garden","facts":{"number":number}}),
            ))
            .unwrap();
    }
    let before = refresh_and_check(&store);
    let (generation, _) = store.newest_attention_list();
    let plan = checkpoint_rules::plan_drops(&store.checkpoint_sealed_set(now_ms() + 1000).unwrap());
    assert!(!plan.claims.is_empty());
    store
        .apply_checkpoint_drop("checkpoint/garden-attention", &plan.envelopes, &plan.claims)
        .unwrap();
    assert!(store.newest_attention_list().1.is_none(), "the checkpoint forgot the list");
    // A refresh that read its previous publication before the checkpoint publishes nothing.
    store.publish_attention_list(
        generation,
        AttentionPublication {
            cut: before.cut,
            projection: before.projection.clone(),
            evaluated_at_unix_ms: before.evaluated_at_unix_ms,
            published_at_unix_ms: now_ms(),
            rows: Arc::clone(&before.rows),
        },
        false,
    );
    assert!(store.newest_attention_list().1.is_none());
    let after = refresh_and_check(&store);
    assert_eq!(after.rows, before.rows, "the request survives the trim");
}

#[test]
fn a_projection_change_under_the_same_claims_folds_again() {
    let (store, _, request) = fixture();
    store.start_attention_list_refresher().unwrap();
    store.ask_person(&request).unwrap();
    refresh_and_check(&store);
    let folded = folds(&store);
    store
        .connection
        .write()
        .execute(
            "INSERT INTO projection_health(aggregate, status, last_good_store_index, updated_at_unix_ms)
             VALUES ('graph', 'healthy', 1, '0')
             ON CONFLICT(aggregate) DO UPDATE SET last_good_store_index=last_good_store_index+1",
            [],
        )
        .unwrap();
    refresh_and_check(&store);
    assert_eq!(folds(&store), folded + 1);
    assert_eq!(store.attention_list_folds().get("projection"), Some(&1));
}

#[test]
fn rows_older_than_the_clock_period_fold_again() {
    let (store, _, request) = fixture();
    store.start_attention_list_refresher().unwrap();
    store.ask_person(&request).unwrap();
    let before = refresh_and_check(&store);
    let (generation, _) = store.newest_attention_list();
    let stale = before.evaluated_at_unix_ms - 31_000;
    store.publish_attention_list(
        generation,
        AttentionPublication {
            cut: before.cut,
            projection: before.projection.clone(),
            evaluated_at_unix_ms: stale,
            published_at_unix_ms: before.published_at_unix_ms,
            rows: Arc::clone(&before.rows),
        },
        false,
    );
    let after = refresh_and_check(&store);
    assert!(after.evaluated_at_unix_ms > stale);
    assert_eq!(store.attention_list_folds().get("clock"), Some(&1));
}

#[test]
fn delta_reads_only_claims_since_the_cut() {
    let (store, _, request) = fixture();
    store.start_attention_list_refresher().unwrap();
    let before = refresh_and_check(&store);
    let index = store.index().unwrap();
    assert_eq!(
        store.read_snapshot(|index| store.attention_list_delta(&before, index)).unwrap(),
        AttentionDelta::Unchanged
    );
    store.ask_person(&request).unwrap();
    let delta = store.read_snapshot(|index| store.attention_list_delta(&before, index)).unwrap();
    assert!(matches!(delta, AttentionDelta::Refold(_)), "{delta:?}");
    assert!(store.index().unwrap() > index);
}
