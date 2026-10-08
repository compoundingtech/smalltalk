use super::*;
use crate::model::PersonStepResponse;
use crate::store::custom::{RegistrationRequest, ReplyRequest};
use smallclaims::ivm::{Readiness, SourceCut, events};

fn views(store: &Store) -> Views {
    let views = Views::new(definitions()).unwrap();
    views.create_schema(&store.connection.write()).unwrap();
    views
}

// Test-only installation after an isolated Store's explicit projection has completed. This
// is NOT a production certification hook. Each fixture compares the indexed result with
// the canonical full reader; production installation must certify its mutation coverage.
fn install(store: &Store, views: &Views) {
    store.project_replication_backlog().unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let index = smallclaims::store::current_index_tx(&tx).unwrap();
    views
        .initialize_empty(
            &tx,
            SourceCut {
                epoch: 1,
                admitted: index,
                projected: index,
                local_generation: 0,
            },
        )
        .unwrap();
    events::install(&tx, 1024).unwrap();
    for view in [PERSON_VIEW, CUSTOM_VIEW] {
        let mut after = String::new();
        loop {
            let keys = seed_page(&tx, view, &after, 2).unwrap();
            if keys.is_empty() {
                break;
            }
            after = keys.last().unwrap().clone();
        }
    }
    while !backfill_page(&tx, 0, 2).unwrap() {}
    tx.execute("UPDATE ivm_views SET ready=1", []).unwrap();
    tx.commit().unwrap();
}

fn maintain(store: &Store, views: &Views) {
    store.project_replication_backlog().unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let previous = source_cut(&tx).unwrap().unwrap();
    let index = smallclaims::store::current_index_tx(&tx).unwrap();
    views
        .publish_cut(
            &tx,
            SourceCut {
                admitted: index,
                projected: index,
                ..previous
            },
        )
        .unwrap();
    for _ in 0..100 {
        if flush(&tx, views, 0, 2).unwrap() {
            tx.commit().unwrap();
            return;
        }
    }
    panic!("fixture maintenance did not complete");
}

fn parity(store: &Store, views: &Views, view: &str, person: &str, at: u128) -> Vec<Value> {
    let mut expected =
        crate::api::client_attention_resources_at(store, Some(person), false, at).unwrap();
    expected.retain(|r| match view {
        PERSON_VIEW => r["attention_kind"] == "person-step",
        CUSTOM_VIEW => r["source_kind"] == "custom",
        _ => false,
    });
    let actual = window(&store.readers.get(), views, view, person, at, 501).unwrap();
    assert_eq!(actual, expected);
    for item in &actual {
        let id = item["id"].as_str().unwrap();
        assert_eq!(
            row(&store.readers.get(), views, view, person, id, at)
                .unwrap()
                .as_ref(),
            Some(item)
        );
        assert!(
            row(
                &store.readers.get(),
                views,
                view,
                if person == "person/robin" {
                    "person/avery"
                } else {
                    "person/robin"
                },
                id,
                at
            )
            .unwrap()
            .is_none()
        );
    }
    actual
}

fn input(subject: &str, kind: &str, actor: &str, fields: Value) -> ClaimInput {
    ClaimInput {
        subject: subject.into(),
        kind: kind.into(),
        actor: Some(actor.into()),
        fields: serde_json::from_value(fields).unwrap(),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    }
}
fn custom(store: &Store, subject: &str, person: &str) {
    let manifest = serde_json::from_str(include_str!(
        "../../../../../examples/st3/custom-review.json"
    ))
    .unwrap();
    store
        .register_custom_kind(&RegistrationRequest {
            manifest,
            actor: "agent/garden/seed".into(),
        })
        .unwrap();
    store
        .append_claim(&input(
            subject,
            "custom.garden.review.v1.requested",
            "agent/garden/seed",
            json!({"title":"Choose a seed","detail":"Keep or discard","recipient":person}),
        ))
        .unwrap();
}

#[test]
fn person_rows_match_full_reader_before_and_after_answer_and_cancel() {
    for cancel in [false, true] {
        let (store, _, request) = person_work::tests::fixture();
        let views = views(&store);
        install(&store, &views);
        let ask = store.ask_person(&request).unwrap();
        // A reader cannot silently repair or expose stale rows between source and operator.
        assert!(
            window(
                &store.readers.get(),
                &views,
                PERSON_VIEW,
                "person/avery",
                u128::MAX,
                10
            )
            .is_err()
        );
        maintain(&store, &views);
        let cards = parity(&store, &views, PERSON_VIEW, "person/avery", u128::MAX);
        assert!(cards.iter().any(|r| r["source_id"] == ask.subject));
        parity(&store, &views, PERSON_VIEW, "person/avery", 0);
        parity(&store, &views, PERSON_VIEW, "person/robin", u128::MAX);
        let response = PersonStepResponse {
            delegation: None,
            subject: ask.subject,
            actor: if cancel {
                request.actor
            } else {
                "person/avery".into()
            },
            summary: "Reviewed".into(),
            evidence: vec![],
            episode: None,
            idempotency_key: "seed-answer".into(),
            answer: None,
        };
        store.finish_person_step(&response, cancel).unwrap();
        maintain(&store, &views);
        parity(&store, &views, PERSON_VIEW, "person/avery", u128::MAX);
        assert!(
            row(
                &store.readers.get(),
                &views,
                PERSON_VIEW,
                "person/avery",
                cards[0]["id"].as_str().unwrap(),
                u128::MAX
            )
            .unwrap()
            .is_none()
        );
    }
}

#[test]
fn custom_rows_preserve_form_identity_order_and_recipient_changes() {
    let store = Store::open_memory("alder").unwrap();
    let views = views(&store);
    // Custom creation is tested on the real registration/admission path.
    custom(&store, "custom/garden/review/v1/zeta", "person/lichen");
    custom(&store, "custom/garden/review/v1/alpha", "person/lichen");
    install(&store, &views);
    let cards = parity(&store, &views, CUSTOM_VIEW, "person/lichen", u128::MAX);
    assert_eq!(cards.len(), 2);
    assert!(cards[0]["custom_form"].is_object());
    let subject = cards[0]["source_id"].as_str().unwrap();
    let source = store.custom_subject(subject).unwrap().unwrap();
    let reply = ReplyRequest {
        subject: subject.into(),
        registration: source["registration"].as_str().unwrap().into(),
        revision: source["revision"].as_str().unwrap().into(),
        episode: source["attention"]["episode"].as_str().unwrap().into(),
        fields: serde_json::from_value(json!({"selection":"keep"})).unwrap(),
        actor: "person/lichen".into(),
        idempotency_key: "seed-keep".into(),
    };
    store.reply_custom_subject(&reply).unwrap();
    maintain(&store, &views);
    let after = parity(&store, &views, CUSTOM_VIEW, "person/lichen", u128::MAX);
    assert_eq!(after.len(), 1);
    assert!(
        row(
            &store.readers.get(),
            &views,
            CUSTOM_VIEW,
            "person/lichen",
            cards[0]["id"].as_str().unwrap(),
            u128::MAX
        )
        .unwrap()
        .is_none()
    );
    parity(&store, &views, CUSTOM_VIEW, "person/lichen", 0);
    assert_eq!(
        window(
            &store.readers.get(),
            &views,
            CUSTOM_VIEW,
            "person/lichen",
            u128::MAX,
            1
        )
        .unwrap(),
        after[..1]
    );
}

#[test]
fn unrelated_writes_do_not_change_keys_and_rollback_restores_rows_and_journal() {
    let (store, _, request) = person_work::tests::fixture();
    let views = views(&store);
    let ask = store.ask_person(&request).unwrap();
    install(&store, &views);
    let before = parity(&store, &views, PERSON_VIEW, "person/avery", u128::MAX);
    let boundary = events::capture(&store.readers.get(), &views, PERSON_VIEW).unwrap();
    store.append_claim(&input("message/seed-note","message.sent","person/robin",json!({"from":"person/robin","to":"agent/garden/seed","content":"A note","status":"sent"}))).unwrap();
    // An unchanged row is still unavailable while the common source prefix is pending.
    {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        let previous = source_cut(&tx).unwrap().unwrap();
        let admitted = smallclaims::store::current_index_tx(&tx).unwrap();
        assert!(admitted > previous.projected);
        views
            .publish_cut(
                &tx,
                SourceCut {
                    admitted,
                    ..previous
                },
            )
            .unwrap();
        assert!(matches!(
            views.readiness(&tx, PERSON_VIEW, previous.epoch).unwrap(),
            Readiness::SourcePending
        ));
        assert!(window(&tx, &views, PERSON_VIEW, "person/avery", u128::MAX, 501).is_err());
        assert!(
            row(
                &tx,
                &views,
                PERSON_VIEW,
                "person/avery",
                before[0]["id"].as_str().unwrap(),
                u128::MAX
            )
            .is_err()
        );
        tx.rollback().unwrap();
    }
    maintain(&store, &views);
    let next = events::capture(&store.readers.get(), &views, PERSON_VIEW).unwrap();
    assert_eq!(boundary.keys, next.keys);
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    tx.execute(
        "UPDATE step_runs SET status='cancelled' WHERE subject=?1",
        [&ask.subject],
    )
    .unwrap();
    assert!(window(&tx, &views, PERSON_VIEW, "person/avery", u128::MAX, 501).is_err());
    assert!(
        row(
            &tx,
            &views,
            PERSON_VIEW,
            "person/avery",
            before[0]["id"].as_str().unwrap(),
            u128::MAX
        )
        .is_err()
    );
    assert!(flush(&tx, &views, 0, 128).unwrap());
    assert!(
        window(&tx, &views, PERSON_VIEW, "person/avery", u128::MAX, 501)
            .unwrap()
            .iter()
            .all(|r| r["source_id"] != ask.subject)
    );
    tx.rollback().unwrap();
    drop(writer);
    assert_eq!(
        parity(&store, &views, PERSON_VIEW, "person/avery", u128::MAX),
        before
    );
    assert_eq!(
        events::capture(&store.readers.get(), &views, PERSON_VIEW)
            .unwrap()
            .keys,
        next.keys
    );
}

#[test]
fn run_closure_and_reopen_only_invalidate_dependents() {
    let (store, origin, request) = person_work::tests::fixture();
    let views = views(&store);
    store.ask_person(&request).unwrap();
    install(&store, &views);
    let before = parity(&store, &views, PERSON_VIEW, "person/avery", u128::MAX);
    // Source projection repair/reopen with no admission frontier change.
    for status in ["cancelled", "running"] {
        store
            .connection
            .batched(|tx| -> Result<()> {
                tx.execute(
                    "UPDATE mission_runs SET status=?1 WHERE id=?2",
                    params![status, origin.run.trim_start_matches("mission-run/")],
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        maintain(&store, &views);
        let actual = parity(&store, &views, PERSON_VIEW, "person/avery", u128::MAX);
        if status == "cancelled" {
            assert!(actual.is_empty());
        } else {
            assert_eq!(actual, before);
        }
    }
}

#[test]
fn scheduled_eligibility_uses_captured_time_and_emits_key_changes() {
    let store = Store::open_memory("alder").unwrap();
    let views = views(&store);
    custom(&store, "custom/garden/review/v1/clock", "person/lichen");
    install(&store, &views);
    let at: u128 = store
        .readers
        .get()
        .query_row(
            "SELECT eligible FROM local_attention_open WHERE family='custom'",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
        .parse()
        .unwrap();
    assert!(parity(&store, &views, CUSTOM_VIEW, "person/lichen", at - 1).is_empty());
    assert_eq!(
        parity(&store, &views, CUSTOM_VIEW, "person/lichen", at).len(),
        1
    );
    let before = events::capture(&store.readers.get(), &views, CUSTOM_VIEW).unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    assert_eq!(
        clock_page(&tx, &views, at - 1, at, None, 1).unwrap().len(),
        1
    );
    tx.commit().unwrap();
    drop(writer);
    let after = events::capture(&store.readers.get(), &views, CUSTOM_VIEW).unwrap();
    assert_ne!(before.keys, after.keys);
    assert!(matches!(after.availability.readiness, Readiness::Ready(_)));
}

#[test]
fn source_reassignment_retires_old_public_key_without_rebuilding_other_rows() {
    let (store, _, request) = person_work::tests::fixture();
    let views = views(&store);
    let ask = store.ask_person(&request).unwrap();
    install(&store, &views);
    let before = parity(&store, &views, PERSON_VIEW, "person/avery", u128::MAX);
    let old = before
        .iter()
        .find(|r| r["source_id"] == ask.subject)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    store
        .connection
        .batched(|tx| -> Result<()> {
            tx.execute(
                "UPDATE step_runs SET assignee='person/robin' WHERE subject=?1",
                [&ask.subject],
            )?;
            Ok(())
        })
        .unwrap()
        .unwrap();
    maintain(&store, &views);
    parity(&store, &views, PERSON_VIEW, "person/avery", u128::MAX);
    let new = parity(&store, &views, PERSON_VIEW, "person/robin", u128::MAX);
    assert_eq!(new.len(), 1);
    assert_ne!(new[0]["id"], old);
    assert!(
        row(
            &store.readers.get(),
            &views,
            PERSON_VIEW,
            "person/avery",
            &old,
            u128::MAX
        )
        .unwrap()
        .is_none()
    );
    let page = views
        .changed_keys(&store.readers.get(), PERSON_VIEW, 1, 0, 128, None)
        .unwrap();
    assert!(
        page.keys
            .iter()
            .all(|k| k.key == serde_json::to_string(&("person", ask.subject.as_str())).unwrap())
    );
}

#[test]
fn unproved_retirement_fences_person_family_and_preserves_custom_availability() {
    let (store, _, request) = person_work::tests::fixture();
    let views = views(&store);
    store.ask_person(&request).unwrap();
    custom(
        &store,
        "custom/garden/review/v1/independent",
        "person/lichen",
    );
    install(&store, &views);
    let intent = crate::graph::parse_internal_intent(
        r#"version 2
stop "agent/alder.asker"
"#,
        "alder",
    )
    .unwrap();
    store.apply_internal(&intent, "stop-requester").unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let previous = source_cut(&tx).unwrap().unwrap();
    let index = smallclaims::store::current_index_tx(&tx).unwrap();
    views
        .publish_cut(
            &tx,
            SourceCut {
                admitted: index,
                projected: index,
                ..previous
            },
        )
        .unwrap();
    assert!(!flush(&tx, &views, 0, 128).unwrap());
    assert!(matches!(
        views.readiness(&tx, PERSON_VIEW, 1).unwrap(),
        Readiness::Fenced
    ));
    assert!(matches!(
        views.readiness(&tx, CUSTOM_VIEW, 1).unwrap(),
        Readiness::Ready(_)
    ));
    assert!(window(&tx, &views, PERSON_VIEW, "person/avery", u128::MAX, 501).is_err());
    assert_eq!(
        window(&tx, &views, CUSTOM_VIEW, "person/lichen", u128::MAX, 501)
            .unwrap()
            .len(),
        1
    );
    tx.commit().unwrap();
}

#[test]
fn missing_document_dependency_and_recovery_match_full_reader() {
    let store = Store::open_memory("alder").unwrap();
    let views = views(&store);
    let manifest = serde_json::from_str(include_str!(
        "../../../../../examples/st3/custom-review.json"
    ))
    .unwrap();
    store
        .register_custom_kind(&RegistrationRequest {
            manifest,
            actor: "agent/garden/seed".into(),
        })
        .unwrap();
    let content = b"A seed description";
    let hash = hex::encode(Sha256::digest(content));
    let registration = store.custom_registrations().unwrap()[0]["registration"]
        .as_str()
        .unwrap()
        .to_owned();
    let subject = "custom/garden/review/v1/missing-context";
    store.connection.batched(|tx|append_claim_tx(tx,&store.origin,subject,"custom.garden.review.v1.requested",Some("agent/garden/seed"),&json!({"fields":{"_registration":registration,"title":"Review seed","detail":"Use the evidence","recipient":"person/lichen","context":format!("doc/garden/context@{hash}")}}),&[],None)).unwrap().unwrap();
    install(&store, &views);
    assert!(parity(&store, &views, CUSTOM_VIEW, "person/lichen", u128::MAX).is_empty());
    assert_eq!(
        store.custom_subject(subject).unwrap().unwrap()["state"],
        "pending-dependencies"
    );
    store
        .put_document_as(
            "doc/garden/context",
            content,
            &None,
            "seed-context",
            Some("agent/garden/seed"),
        )
        .unwrap();
    maintain(&store, &views);
    assert_eq!(
        parity(&store, &views, CUSTOM_VIEW, "person/lichen", u128::MAX).len(),
        1
    );
}

#[test]
fn concurrent_custom_creation_ties_converge_under_permuted_replication() {
    let left = Store::open_memory("alder").unwrap();
    let right = Store::open_memory("birch").unwrap();
    let at = now_ms() + 60_000;
    for (store, person) in [(&left, "person/lichen"), (&right, "person/avery")] {
        store.set_write_clock_at(at).unwrap();
        custom(store, "custom/garden/review/v1/concurrent", person);
    }
    let first = Store::open_memory("cedar").unwrap();
    let second = Store::open_memory("elm").unwrap();
    let first_views = views(&first);
    let second_views = views(&second);
    for (target, sources) in [(&first, [&left, &right]), (&second, [&right, &left])] {
        for source in sources {
            target
                .import_replication(&source.origin, &source.export_replication(0).unwrap())
                .unwrap();
        }
    }
    install(&first, &first_views);
    install(&second, &second_views);
    for person in ["person/lichen", "person/avery"] {
        assert_eq!(
            parity(&first, &first_views, CUSTOM_VIEW, person, u128::MAX),
            parity(&second, &second_views, CUSTOM_VIEW, person, u128::MAX)
        );
    }
    // A repeated delivery produces neither a semantic change nor a new public episode.
    let boundary = events::capture(&first.readers.get(), &first_views, CUSTOM_VIEW).unwrap();
    first
        .import_replication(&left.origin, &left.export_replication(0).unwrap())
        .unwrap();
    maintain(&first, &first_views);
    assert_eq!(
        events::capture(&first.readers.get(), &first_views, CUSTOM_VIEW)
            .unwrap()
            .keys,
        boundary.keys
    );
}

#[test]
fn persisted_families_reopen_and_replay_preserve_public_rows_and_key_cursors() {
    let (source, _, request) = person_work::tests::fixture();
    source.ask_person(&request).unwrap();
    custom(
        &source,
        "custom/garden/review/v1/persisted",
        "person/lichen",
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("attention.sqlite3");
    let target = Store::open(&path, "birch").unwrap();
    target
        .import_replication(&source.origin, &source.export_replication(0).unwrap())
        .unwrap();
    let registry = views(&target);
    install(&target, &registry);
    let people = [
        (PERSON_VIEW, "person/avery"),
        (CUSTOM_VIEW, "person/lichen"),
    ];
    let before: Vec<_> = people
        .iter()
        .map(|(view, person)| {
            (
                parity(&target, &registry, view, person, u128::MAX),
                events::capture(&target.readers.get(), &registry, view).unwrap(),
            )
        })
        .collect();
    assert!(before.iter().all(|(rows, _)| !rows.is_empty()));
    drop(registry);
    drop(target);
    let target = Store::open(&path, "birch").unwrap();
    let registry = views(&target);
    maintain(&target, &registry);
    for ((view, person), (rows, boundary)) in people.iter().zip(&before) {
        assert_eq!(parity(&target, &registry, view, person, u128::MAX), *rows);
        let after = events::capture(&target.readers.get(), &registry, view).unwrap();
        assert_eq!(after.identity, boundary.identity);
        assert_eq!(after.keys, boundary.keys);
    }
    target
        .connection
        .batched(replay_graph_from_nothing_tx)
        .unwrap()
        .unwrap();
    assert!(!clean(&target.readers.get(), None).unwrap());
    for (view, person) in people {
        assert!(
            window(
                &target.readers.get(),
                &registry,
                view,
                person,
                u128::MAX,
                501
            )
            .is_err()
        );
    }
    maintain(&target, &registry);
    for ((view, person), (rows, boundary)) in people.iter().zip(&before) {
        assert_eq!(parity(&target, &registry, view, person, u128::MAX), *rows);
        assert_eq!(
            events::capture(&target.readers.get(), &registry, view)
                .unwrap()
                .keys,
            boundary.keys
        );
    }
}

#[test]
fn checkpoint_trim_preserves_canonical_custom_episode_and_fences_indexed_reads() {
    let store = Store::open_memory("alder").unwrap();
    let registry = views(&store);
    let subject = "custom/garden/review/v1/checkpoint";
    custom(&store, subject, "person/lichen");
    // Give checkpoint rules a superseded disposable observation to trim.
    for number in 0..3 {
        let mut observation = input(
            "resource/garden/checkpoint",
            "resource.observed",
            "daemon/runtime",
            json!({"kind":"custom.garden.checkpoint","observer":"observer/garden","facts":{"number":number}}),
        );
        observation.actor = None;
        store.append_claim(&observation).unwrap();
    }
    install(&store, &registry);
    let before = parity(&store, &registry, CUSTOM_VIEW, "person/lichen", u128::MAX);
    let boundary = events::capture(&store.readers.get(), &registry, CUSTOM_VIEW).unwrap();
    let plan = checkpoint_rules::plan_drops(&store.checkpoint_sealed_set(now_ms() + 1000).unwrap());
    assert!(!plan.claims.is_empty());
    let source_ids: BTreeSet<_> = store
        .claims_for(subject, None)
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert!(plan.claims.iter().all(|c| !source_ids.contains(&c.id)));
    store
        .apply_checkpoint_drop("checkpoint/garden-attention", &plan.envelopes, &plan.claims)
        .unwrap();
    store
        .connection
        .batched(replay_graph_from_nothing_tx)
        .unwrap()
        .unwrap();
    assert!(
        window(
            &store.readers.get(),
            &registry,
            CUSTOM_VIEW,
            "person/lichen",
            u128::MAX,
            501
        )
        .is_err()
    );
    // Retention intentionally fences the primitive's finite registry. A surviving row does
    // not authorize flushing or reusing a serving lifetime; the installer owns replacement.
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    assert!(matches!(
        registry.readiness(&tx, CUSTOM_VIEW, 1).unwrap(),
        Readiness::Fenced
    ));
    assert!(!flush(&tx, &registry, u128::MAX, 128).unwrap());
    tx.commit().unwrap();
    drop(writer);
    let canonical =
        crate::api::client_attention_resources_at(&store, Some("person/lichen"), false, u128::MAX)
            .unwrap();
    assert_eq!(canonical, before);
    let after = events::capture(&store.readers.get(), &registry, CUSTOM_VIEW).unwrap();
    assert_eq!(after.keys, boundary.keys);
    assert!(after.status.sequence > boundary.status.sequence);
    assert!(matches!(after.availability.readiness, Readiness::Fenced));
}

#[test]
fn authored_person_assignee_matches_the_full_readers_lowercase_range() {
    let (store, _, _) = person_work::tests::fixture();
    // The full reader's indexed lowercase range excludes legacy uppercase assignees.
    // Neither an upper-case request nor a lower-case request may revive that row.
    let registry = views(&store);
    install(&store, &registry);
    store
        .connection
        .write()
        .execute(
            "UPDATE step_runs SET status='ready' WHERE assignee='person/avery'",
            [],
        )
        .unwrap();
    maintain(&store, &registry);
    assert!(!parity(&store, &registry, PERSON_VIEW, "person/avery", u128::MAX).is_empty());
    store
        .connection
        .write()
        .execute(
            "UPDATE step_runs SET assignee='PERSON/avery',status='ready' WHERE assignee='person/avery'",
            [],
        )
        .unwrap();
    maintain(&store, &registry);
    assert!(parity(&store, &registry, PERSON_VIEW, "PERSON/avery", u128::MAX).is_empty());
    assert!(parity(&store, &registry, PERSON_VIEW, "person/avery", u128::MAX).is_empty());
    assert!(
        window(
            &store.readers.get(),
            &registry,
            PERSON_VIEW,
            "person/avery",
            u128::MAX,
            501
        )
        .unwrap()
        .is_empty()
    );
}
