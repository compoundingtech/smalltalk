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
    store
        .append_claim(&input(
            "glass/person/avery/019a0000-0000-7000-8000-000000000001",
            "glass.upserted",
            Some("person/avery"),
            json!({"body":{"name":"Desk","layout":{"tabs":[{"pane":"opaque:anything"}]}},"base_revision":null}),
        ))
        .unwrap();
    store
        .append_client_claim(&input(
            "arrangement/person/avery/019a0000-0000-7000-8000-000000000002",
            "arrangement.edited",
            Some("person/avery"),
            json!({"owner":"person/avery","operations":[{"op":"create","name":"Work"}]}),
        ))
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
            json!({"state":"blocked","reason":"providerAuth","provider_auth":false,"incarnation_id":"one","driver":"claude"}),
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
    // Both writers date their request the same past instant, so neither waits to be eligible.
    let at = now_ms() - 60_000;
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
            due_at_unix_ms: None,
            future_after: None,
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
fn a_projection_catching_up_folds_only_when_the_claims_it_projected_matter() {
    let (store, _, request) = fixture();
    store.start_attention_list_refresher().unwrap();
    let project_through = |through: u64| {
        store
            .connection
            .write()
            .execute(
                "INSERT INTO projection_health(aggregate, status, last_good_store_index, updated_at_unix_ms)
                 VALUES ('graph', 'healthy', ?1, '0')
                 ON CONFLICT(aggregate) DO UPDATE SET status='healthy', last_good_store_index=?1",
                [through],
            )
            .unwrap();
    };
    // Replication projection deferred past the ask: the list is published with that frontier.
    let before_ask = store.index().unwrap();
    store.ask_person(&request).unwrap();
    project_through(before_ask);
    refresh_and_check(&store);
    let folded = folds(&store);
    // Catching up over the ask folds; over a message alone it does not.
    project_through(store.index().unwrap());
    refresh_and_check(&store);
    assert_eq!(folds(&store), folded + 1);
    assert!(
        store.attention_list_folds().keys().any(|cause| cause.starts_with("projection ")),
        "{:?}",
        store.attention_list_folds()
    );
    let before_note = store.index().unwrap();
    store
        .append_claim(&input(
            "message/seed-note",
            "message.sent",
            Some("person/robin"),
            json!({"from":"person/robin","to":"agent/alder.asker","content":"A note","status":"sent"}),
        ))
        .unwrap();
    project_through(before_note);
    refresh_and_check(&store);
    project_through(store.index().unwrap());
    refresh_and_check(&store);
    assert_eq!(folds(&store), folded + 1, "a projected message folds nothing");
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
            due_at_unix_ms: None,
            future_after: None,
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
        store.read_snapshot(|index| store.attention_list_delta(&before, index, now_ms())).unwrap(),
        AttentionDelta::Unchanged(None)
    );
    store.ask_person(&request).unwrap();
    let delta = store
        .read_snapshot(|index| store.attention_list_delta(&before, index, now_ms()))
        .unwrap();
    assert!(matches!(delta, AttentionDelta::Refold(_)), "{delta:?}");
    assert!(store.index().unwrap() > index);
}

#[test]
fn a_view_failing_for_its_limit_is_withdrawn_until_it_publishes_again() {
    let store = Store::open_memory("alder").unwrap();
    assert!(!store.published_view_serving("attention"), "nothing serves without a refresher");
    store.start_attention_list_refresher().unwrap();
    for view in PUBLISHED_VIEWS {
        assert!(store.published_view_serving(view));
    }
    assert!(!store.published_view_serving("missions"), "not this refresher's view");
    // A first failure is not yet a withdrawal; one that has lasted the limit is.
    store.note_view_refreshed("summary", false);
    assert!(store.published_view_serving("summary"));
    store.smalltalk.attention_list.health[3]
        .failing_since
        .store(now_ms() as u64 - WITHDRAW_AFTER_MS - 1, AtomicOrdering::Release);
    store.note_view_refreshed("summary", false);
    assert!(!store.published_view_serving("summary"));
    assert!(!store.collection_view_published("summary"), "windows follow commits again");
    assert!(store.published_view_serving("attention"), "the other views keep serving");
    store.note_view_refreshed("summary", true);
    assert!(store.published_view_serving("summary"));
    assert!(store.collection_view_published("summary"), "windows reread the publication");
    // A stopped refresher serves nothing, whatever its views' health.
    store.publish_collection_view("attention");
    store.stop_attention_list_refresher();
    assert!(!store.attention_list_refresher_running());
    for view in PUBLISHED_VIEWS {
        assert!(!store.published_view_serving(view));
        assert!(!store.collection_view_published(view));
    }
}

#[test]
fn an_ask_dated_in_the_future_appears_when_it_is_due_without_another_claim() {
    let (store, _, request) = fixture();
    store.start_attention_list_refresher().unwrap();
    refresh_and_check(&store);
    let accepted = now_ms() + 400;
    store.set_write_clock_at(accepted).unwrap();
    let ask = store.ask_person(&request).unwrap();
    let asked = crate::store::person_work::request(&store.readers.get(), &ask.subject)
        .unwrap()
        .expect("the ask")
        .accepted_at_unix_ms;
    assert!(asked >= accepted);
    let early = refresh_and_check(&store);
    assert!(early.rows.iter().all(|row| row["source_id"] != ask.subject), "not yet eligible");
    assert_eq!(early.due_at_unix_ms, Some(accepted));
    assert_eq!(store.attention_list_due_at(30_000), Some(accepted));
    // Once it is due, a refresh with no new claim folds it in.
    while now_ms() < asked {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let due = refresh_and_check(&store);
    assert!(due.rows.iter().any(|row| row["source_id"] == ask.subject));
    assert_eq!(due.cut, early.cut);
    assert_eq!(store.attention_list_folds().get("due"), Some(&1));
}

#[test]
fn every_seat_a_login_item_covers_counts_as_holding_it() {
    let rows = vec![
        json!({"attention_kind":"harness-login","source_id":"agent/alder.first",
            "targets":["agent/alder.first","agent/alder.second"]}),
        json!({"attention_kind":"person-step","source_id":"step-run/alder/review",
            "targets":["agent/alder.unrelated"]}),
    ];
    assert_eq!(
        login_seats(&rows),
        BTreeSet::from(["agent/alder.first".to_owned(), "agent/alder.second".to_owned()])
    );
}

#[test]
fn a_claim_from_any_seat_a_shared_login_item_covers_folds() {
    let (store, origin, _) = fixture();
    store.start_attention_list_refresher().unwrap();
    let published = refresh_and_check(&store);
    // One login item for two seats sharing a login directory: its source is the first seat to
    // fail; the asker is covered only through its targets.
    let shared_at = |publication: &AttentionPublication, cut: u64| {
        let mut rows = (*publication.rows).clone();
        rows.push(json!({"attention_kind":"harness-login","source_id":"agent/alder.first",
            "targets":["agent/alder.first","agent/alder.asker"]}));
        AttentionPublication {
            cut,
            projection: publication.projection.clone(),
            evaluated_at_unix_ms: publication.evaluated_at_unix_ms,
            due_at_unix_ms: None,
            future_after: None,
            published_at_unix_ms: now_ms(),
            rows: Arc::new(rows),
        }
    };
    let without_at = |publication: &AttentionPublication, cut: u64| AttentionPublication {
        cut,
        projection: publication.projection.clone(),
        evaluated_at_unix_ms: publication.evaluated_at_unix_ms,
        due_at_unix_ms: None,
        future_after: None,
        published_at_unix_ms: now_ms(),
        rows: Arc::clone(&publication.rows),
    };
    let delta = |publication: &AttentionPublication| {
        store
            .read_snapshot(|index| store.attention_list_delta(publication, index, now_ms()))
            .unwrap()
    };
    // The covered seat recovers: an ordinary observation from it folds when it shares the item.
    store
        .append_claim(&input(
            "agent/alder.asker",
            "harness.observed",
            Some("agent/alder.asker"),
            json!({"state":"idle","incarnation_id":"one","driver":"claude"}),
        ))
        .unwrap();
    assert_eq!(
        delta(&shared_at(&published, published.cut)),
        AttentionDelta::Refold("harness.observed".into())
    );
    assert_eq!(delta(&without_at(&published, published.cut)), AttentionDelta::Unchanged(None));
    // So does work it reports, which ends its login wait.
    let observed = store.index().unwrap();
    {
        let mut writer = store.connection.write();
        let transaction = writer.transaction().unwrap();
        append_claim_tx(&transaction, "alder", &origin.subject, "work.progress",
            Some("agent/alder.asker"), &json!({"fields": {"summary": "halfway"}}), &[], None)
            .unwrap();
        transaction.commit().unwrap();
    }
    assert_eq!(
        delta(&shared_at(&published, observed)),
        AttentionDelta::Refold("work.progress".into())
    );
    assert_eq!(delta(&without_at(&published, observed)), AttentionDelta::Unchanged(None));
}
