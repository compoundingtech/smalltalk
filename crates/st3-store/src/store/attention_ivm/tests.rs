use super::*;

    

    use crate::store::custom::RegistrationRequest;

    use smallclaims::ivm::{Readiness, SourceCut, events};

    fn views(store: &Store) -> Views {
    let views = Views::new(definitions()).unwrap();
    views.create_schema(&store.connection.write()).unwrap();
    views
}

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
