use super::*;

pub(crate) fn example() -> Manifest {
    serde_json::from_str(include_str!(
        "../../../../../examples/st3/custom-review.json"
    ))
    .unwrap()
}
fn registration(store: &Store) -> Value {
    store
        .register_custom_kind(&RegistrationRequest {
            manifest: example(),
            actor: "agent/garden/seed".into(),
        })
        .unwrap()
}
fn request(store: &Store, id: &str) -> ClaimRecord {
    store.append_claim(&ClaimInput{subject:id.into(),kind:example().creation_kind,actor:Some("agent/garden/seed".into()),fields:serde_json::from_value(json!({"title":"Retain the seed history?","detail":"Choose Keep or Discard.","recipient":"person/lichen"})).unwrap(),..empty_input()}).unwrap()
}
fn reply(store: &Store, id: &str) -> ReplyRequest {
    let v = store.custom_subject(id).unwrap().unwrap();
    ReplyRequest {
        subject: id.into(),
        registration: v["registration"].as_str().unwrap().into(),
        revision: v["revision"].as_str().unwrap().into(),
        episode: v["attention"]["episode"].as_str().unwrap().into(),
        fields: serde_json::from_value(json!({"selection":"keep"})).unwrap(),
        actor: "person/lichen".into(),
        idempotency_key: "garden-review-answer-001".into(),
    }
}
fn legacy_sync(from: &Store, to: &Store) {
    to.import_replication(&from.origin, &from.export_replication(0).unwrap())
        .unwrap();
}

#[test]
fn custom_round_trip_replicates_restarts_and_retains_human_actor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("replica.db");
    let a = Store::open_memory("alder").unwrap();
    let b = Store::open(&path, "birch").unwrap();
    registration(&a);
    let id = "custom/garden/review/v1/review-001";
    let q = request(&a, id);
    legacy_sync(&a, &b);
    assert_eq!(a.custom_subject(id).unwrap(), b.custom_subject(id).unwrap());
    let cards = b.attention_items(Some("person/lichen")).unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].episode, q.id);
    let r = reply(&b, id);
    let answer = b.reply_custom_subject(&r).unwrap();
    assert_eq!(answer.actor.as_deref(), Some("person/lichen"));
    assert!(b.attention_items(Some("person/lichen")).unwrap().is_empty());
    let closed = b
        .attention_history_test_page("person/lichen", 5)
        .unwrap()
        .items;
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["resolution"]["kind"], "answered");
    assert_eq!(closed[0]["resolution"]["by"], "person/lichen");
    assert_eq!(closed[0]["episode"], q.id);
    assert_eq!(b.reply_custom_subject(&r).unwrap().id, answer.id);
    let mut changed = r.clone();
    changed.fields.insert("selection".into(), json!("discard"));
    assert_eq!(
        b.reply_custom_subject(&changed).unwrap_err().code,
        "idempotency-conflict"
    );
    legacy_sync(&b, &a);
    assert_eq!(
        a.reply_custom_subject(&r).unwrap().id,
        answer.id,
        "a retry on another member recovers the replicated receipt"
    );
    assert_eq!(a.custom_subject(id).unwrap(), b.custom_subject(id).unwrap());
    assert_eq!(
        a.attention_history_test_page("person/lichen", 5)
            .unwrap()
            .items,
        closed
    );
    assert_eq!(
        projection_digest::oracle(&a.readers.get()).unwrap(),
        projection_digest::oracle(&b.readers.get()).unwrap()
    );
    drop(b);
    let b = Store::open(&path, "birch").unwrap();
    assert_eq!(
        b.custom_subject(id).unwrap().unwrap()["fields"]["selection"],
        "keep"
    );
    assert!(b.attention_items(Some("person/lichen")).unwrap().is_empty());
    assert_eq!(b.claims_for(id, None).unwrap().len(), 2);
    assert_eq!(
        b.attention_history_test_page("person/lichen", 5)
            .unwrap()
            .items,
        closed
    );
}
#[test]
fn custom_raw_writes_enforce_schema_namespace_and_human_authority() {
    let s = Store::open_memory("alder").unwrap();
    registration(&s);
    let id = "custom/garden/review/v1/review-002";
    let q = request(&s, id);
    let mut input = ClaimInput {
        subject: id.into(),
        kind: "custom.garden.review.v1.answered".into(),
        actor: Some("agent/garden/seed".into()),
        fields: serde_json::from_value(json!({"selection":"keep","request":q.id})).unwrap(),
        ..empty_input()
    };
    assert!(s.append_claim(&input).is_err());
    input.actor = Some("person/lichen".into());
    input.fields.insert("selection".into(), json!("bad"));
    assert!(s.append_claim(&input).is_err());
    input.kind = "custom.other.answered".into();
    assert!(s.append_claim(&input).is_err());
    input.subject = format!("{}fake", schema::REGISTRY_PREFIX);
    input.kind = schema::REGISTERED.into();
    assert_eq!(
        s.append_claim(&input).unwrap_err().code,
        "custom-registration-only"
    );
    assert_eq!(s.attention_items(Some("person/lichen")).unwrap().len(), 1);
    let mut r = reply(&s, id);
    r.actor = "agent/garden/seed".into();
    assert_eq!(s.reply_custom_subject(&r).unwrap_err().code, "forbidden");
    r.actor = "person/lichen".into();
    r.revision = "obsolete".into();
    assert_eq!(s.reply_custom_subject(&r).unwrap_err().code, "stale-fence");
}
#[test]
fn custom_bad_projection_isolated_from_other_sources_and_core_reads() {
    let s = Store::open_memory("alder").unwrap();
    registration(&s);
    request(&s, "custom/garden/review/v1/good");
    let mut m = example();
    m.version = 2;
    m.subject_prefix = "custom/garden/review/v2/".into();
    m.attention.as_mut().unwrap().title = Expr::Constant { value: json!(17) };
    s.register_custom_kind(&RegistrationRequest {
        manifest: m.clone(),
        actor: "agent/garden/seed".into(),
    })
    .unwrap();
    let id = "custom/garden/review/v2/bad";
    request(&s, id);
    assert_eq!(s.custom_subject(id).unwrap().unwrap()["state"], "invalid");
    assert_eq!(s.attention_items(Some("person/lichen")).unwrap().len(), 1);
    s.status(None).unwrap();
    s.append_claim(&ClaimInput {
        subject: "custom/legacy/fact".into(),
        kind: "custom.legacy.recorded".into(),
        fields: BTreeMap::from([("any".into(), json!({"open":true}))]),
        ..empty_input()
    })
    .unwrap();
    assert!(s.custom_subject("custom/legacy/fact").unwrap().is_none());
    m.slots.insert(
        "wrong".into(),
        schema::Slot {
            kind: "unknown".into(),
            select: Select::First,
        },
    );
    assert!(
        s.register_custom_kind(&RegistrationRequest {
            manifest: m,
            actor: "agent/garden/seed".into()
        })
        .is_err()
    );
}
#[test]
fn custom_registration_immutable_and_cannot_capture_legacy_prefix() {
    let s = Store::open_memory("alder").unwrap();
    let first = registration(&s);
    assert_eq!(registration(&s), first);
    let mut m = example();
    m.fields.insert(
        "extra".into(),
        Expr::Constant {
            value: json!("new"),
        },
    );
    assert!(
        s.register_custom_kind(&RegistrationRequest {
            manifest: m,
            actor: "agent/garden/seed".into()
        })
        .is_err()
    );
    let s = Store::open_memory("birch").unwrap();
    s.append_claim(&ClaimInput {
        subject: "custom/garden/review/v1/legacy".into(),
        kind: "custom.garden.legacy".into(),
        ..empty_input()
    })
    .unwrap();
    assert!(
        s.register_custom_kind(&RegistrationRequest {
            manifest: example(),
            actor: "agent/garden/seed".into()
        })
        .is_err()
    );
}
#[test]
fn custom_basis_invalidation_guard_revival_and_reframe_are_graph_facts() {
    let a = Store::open_memory("alder").unwrap();
    let b = Store::open_memory("birch").unwrap();
    let mut m = example();
    let derived = "custom.garden.review.v1.derived";
    let field: schema::Field = serde_json::from_value(
        json!({"value_type":"string","required":true,"values":["pending","gated","moot"]}),
    )
    .unwrap();
    m.claims.insert(
        derived.into(),
        schema::ClaimSchema {
            authority: Authority::Owner,
            fields: BTreeMap::from([("status".into(), field)]),
            additional_fields: false,
        },
    );
    m.slots.insert(
        "state".into(),
        schema::Slot {
            kind: derived.into(),
            select: Select::Last,
        },
    );
    m.attention.as_mut().unwrap().when = Predicate::Eq {
        left: Expr::Field {
            slot: "state".into(),
            field: "status".into(),
        },
        right: Expr::Constant {
            value: json!("pending"),
        },
    };
    m.attention.as_mut().unwrap().episode = Expr::ClaimId {
        slot: "state".into(),
    };
    // Zero/multiple selections and free text are ordinary typed answer fields.
    let selection = m
        .claims
        .get_mut("custom.garden.review.v1.answered")
        .unwrap()
        .fields
        .get_mut("selection")
        .unwrap();
    selection.value_type = st3_schema::ValueType::Array;
    m.attention
        .as_mut()
        .unwrap()
        .reply
        .fields
        .get_mut("selection")
        .unwrap()
        .value_type = st3_schema::ValueType::Array;
    a.register_custom_kind(&RegistrationRequest {
        manifest: m,
        actor: "agent/garden/seed".into(),
    })
    .unwrap();
    let parent = "custom/garden/review/v1/parent";
    let child = "custom/garden/review/v1/child";
    request(&a, parent);
    request(&a, child);
    let kinds = vec![
        "custom.garden.review.v1.requested".to_owned(),
        "custom.garden.review.v1.answered".to_owned(),
    ];
    let derive = |s: &Store, status: &str| {
        let basis = s.custom_basis_revision(parent, &kinds).unwrap();
        s.append_claim(&ClaimInput{subject:child.into(),kind:derived.into(),actor:Some("agent/garden/seed".into()),fields:serde_json::from_value(json!({"status":status,"_basis":[{"subject":parent,"kinds":kinds,"revision":basis},{"subject":child,"kinds":kinds,"revision":s.custom_basis_revision(child,&kinds).unwrap()}]})).unwrap(),..empty_input()}).unwrap()
    };
    derive(&a, "pending");
    assert_eq!(a.attention_items(Some("person/lichen")).unwrap().len(), 1);
    let episode = a.custom_subject(child).unwrap().unwrap()["attention"]["episode"].clone();
    // The person's raw answer is preserved and invalidates the descendant's derived view.
    a.append_claim(&ClaimInput{subject:parent.into(),kind:"custom.garden.review.v1.answered".into(),actor:Some("person/lichen".into()),fields:serde_json::from_value(json!({"request":a.claims_for(parent,None).unwrap()[0].id,"selection":[],"text":"Reframe: retain only recent history"})).unwrap(),..empty_input()}).unwrap();
    assert_eq!(a.custom_subject(child).unwrap().unwrap()["state"], "stale");
    assert!(a.attention_items(Some("person/lichen")).unwrap().is_empty());
    derive(&a, "gated");
    assert!(a.attention_items(Some("person/lichen")).unwrap().is_empty());
    derive(&a, "pending");
    assert_eq!(a.attention_items(Some("person/lichen")).unwrap().len(), 1);
    assert_ne!(
        a.custom_subject(child).unwrap().unwrap()["attention"]["episode"],
        episode
    );
    legacy_sync(&a, &b);
    assert_eq!(
        a.custom_subject(child).unwrap(),
        b.custom_subject(child).unwrap()
    );
    assert_eq!(
        b.claims_for(parent, None).unwrap().last().unwrap().body["fields"]["text"],
        "Reframe: retain only recent history"
    );
    let mut answer = reply(&b, child);
    answer.fields =
        serde_json::from_value(json!({"selection":[],"text":"Reframe the descendant"})).unwrap();
    let human = b.reply_custom_subject(&answer).unwrap();
    assert_eq!(human.actor.as_deref(), Some("person/lichen"));
    assert_eq!(human.body["fields"]["selection"], json!([]));
    assert_eq!(b.custom_subject(child).unwrap().unwrap()["state"], "stale");
    assert!(b.attention_items(Some("person/lichen")).unwrap().is_empty());
}

fn empty_input() -> ClaimInput {
    ClaimInput {
        subject: String::new(),
        kind: String::new(),
        actor: None,
        fields: BTreeMap::new(),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    }
}

const FLEET: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
fn signed_pair() -> (Store, Store) {
    let keys = (0..2)
        .map(|_| Arc::new(crate::fleet::MemberKey::generate().unwrap().0))
        .collect::<Vec<_>>();
    let stores = ["alder", "birch"]
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let s = Store::open_memory(*name).unwrap();
            s.bind_fleet(FLEET).unwrap();
            s.pin_fleet_anchor(keys[0].public()).unwrap();
            s.set_member_key(Some(keys[i].clone())).unwrap();
            s
        })
        .collect::<Vec<_>>();
    for (i, s) in stores.iter().enumerate() {
        let mut f = json!({"fleet_id":FLEET,"member_key":keys[i].public(),"via":if i==0{"anchor"}else{"invite"},"mode":"listening"});
        if i != 0 {
            f["sponsor"] = json!("host/alder");
        }
        stores[0]
            .append_claim(&ClaimInput {
                subject: format!("host/{}", s.origin),
                kind: "fleet.member-admitted".into(),
                fields: serde_json::from_value(f).unwrap(),
                ..empty_input()
            })
            .unwrap();
    }
    signed_sync(&stores[0], &stores[1]);
    let mut iter = stores.into_iter();
    (iter.next().unwrap(), iter.next().unwrap())
}
fn signed_sync(from: &Store, to: &Store) {
    let e = from
        .export_replication_exchange_answering(
            FLEET,
            &to.replication_inventory().unwrap(),
            &to.replication_signature_requests().unwrap(),
        )
        .unwrap();
    to.receive_replication_exchange(&from.origin, FLEET, &e)
        .unwrap();
    let admitted = to.validate_replication_backlog().unwrap();
    assert_eq!(admitted.invalid, 0);
    assert_eq!(admitted.unknown, 0);
    assert!(to.project_replication_backlog().unwrap());
}
#[test]
fn custom_signed_round_trip_late_manifest_partition_answers_and_checkpoint_replay() {
    let (a, b) = signed_pair();
    registration(&a);
    let id = "custom/garden/review/v1/signed-example";
    let q = request(&a, id);
    let seq: u64 = a
        .readers
        .get()
        .query_row(
            "SELECT replica_sequence FROM batches WHERE id=?1",
            [&q.batch_id],
            |r| r.get(0),
        )
        .unwrap();
    let mut early = a
        .export_replication_exchange_answering(
            FLEET,
            &b.replication_inventory().unwrap(),
            &b.replication_signature_requests().unwrap(),
        )
        .unwrap();
    early
        .envelopes
        .retain(|e| e.writer == a.origin && e.sequence >= seq);
    b.receive_replication_exchange(&a.origin, FLEET, &early)
        .unwrap();
    b.validate_replication_backlog().unwrap();
    b.project_replication_backlog().unwrap();
    assert_eq!(
        b.custom_subject(id).unwrap().unwrap()["state"],
        "pending-dependencies"
    );
    assert!(b.attention_items(Some("person/lichen")).unwrap().is_empty());
    signed_sync(&a, &b);
    assert_eq!(a.custom_subject(id).unwrap(), b.custom_subject(id).unwrap());
    assert_eq!(b.attention_items(Some("person/lichen")).unwrap().len(), 1);
    // Both isolated members may accept the same current card. Preserve both answers.
    let ar = reply(&a, id);
    let mut br = reply(&b, id);
    br.fields.insert("selection".into(), json!("discard"));
    br.idempotency_key = "partition-answer-birch".into();
    let ac = a.reply_custom_subject(&ar).unwrap();
    let bc = b.reply_custom_subject(&br).unwrap();
    signed_sync(&a, &b);
    signed_sync(&b, &a);
    assert_eq!(a.claims_for(id, None).unwrap().len(), 3);
    assert!(
        a.claims_for(id, None)
            .unwrap()
            .iter()
            .any(|c| c.id == ac.id)
    );
    assert!(
        a.claims_for(id, None)
            .unwrap()
            .iter()
            .any(|c| c.id == bc.id)
    );
    assert_eq!(a.custom_subject(id).unwrap(), b.custom_subject(id).unwrap());
    let digest = projection_digest::oracle(&a.readers.get()).unwrap();
    assert_eq!(digest, projection_digest::oracle(&b.readers.get()).unwrap());
    let closed = a.attention_history_test_page("person/lichen", 5).unwrap().items;
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["resolution"]["kind"], "answered");
    assert_eq!(closed, b.attention_history_test_page("person/lichen", 5).unwrap().items);
    assert_eq!(a.custom_subject(id).unwrap().unwrap()["state"], "conflict");
    assert_eq!(
        a.custom_subject(id).unwrap().unwrap()["reply_conflicts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let before = a.custom_subject(id).unwrap();
    let plan = checkpoint_rules::plan_drops(&a.checkpoint_sealed_set(now_ms() + 1000).unwrap());
    let custom_ids = a
        .claims_for(id, None)
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect::<BTreeSet<_>>();
    assert!(plan.claims.iter().all(|c| !custom_ids.contains(&c.id)));
    a.apply_checkpoint_drop("checkpoint/garden", &plan.envelopes, &plan.claims)
        .unwrap();
    assert_eq!(closed, a.attention_history_test_page("person/lichen", 5).unwrap().items, "actual checkpoint trim preserves retained closure provenance");
    a.connection
        .batched(replay_graph_from_nothing_tx)
        .unwrap()
        .unwrap();
    assert_eq!(before, a.custom_subject(id).unwrap());
    assert_eq!(digest, projection_digest::oracle(&a.readers.get()).unwrap());
    assert_eq!(closed, a.attention_history_test_page("person/lichen", 5).unwrap().items);
}

#[test]
fn custom_history_rebuilds_recorded_reopened_episodes_without_local_transitions() {
    let a = Store::open_memory("alder").unwrap();
    let b = Store::open_memory("birch").unwrap();
    let mut manifest = example();
    let kind = "custom.garden.review.v1.state";
    manifest.claims.insert(
        kind.into(),
        schema::ClaimSchema {
            authority: Authority::Owner,
            fields: BTreeMap::from([(
                "status".into(),
                serde_json::from_value(
                    json!({"value_type":"string","required":true,"values":["pending","closed"]}),
                )
                .unwrap(),
            )]),
            additional_fields: false,
        },
    );
    manifest.slots.insert(
        "state".into(),
        schema::Slot {
            kind: kind.into(),
            select: Select::Last,
        },
    );
    let attention = manifest.attention.as_mut().unwrap();
    attention.when = Predicate::Eq {
        left: Expr::Field {
            slot: "state".into(),
            field: "status".into(),
        },
        right: Expr::Constant {
            value: json!("pending"),
        },
    };
    attention.episode = Expr::ClaimId {
        slot: "state".into(),
    };
    a.register_custom_kind(&RegistrationRequest {
        manifest,
        actor: "agent/garden/seed".into(),
    })
    .unwrap();
    let id = "custom/garden/review/v1/reopened-history";
    request(&a, id);
    let mut episodes = Vec::new();
    for (n, status) in ["pending", "closed", "pending", "closed"]
        .into_iter()
        .enumerate()
    {
        let c = a
            .append_claim(&ClaimInput {
                subject: id.into(),
                kind: kind.into(),
                actor: Some("agent/garden/seed".into()),
                fields: BTreeMap::from([("status".into(), json!(status))]),
                idempotency_key: Some(format!("custom-history-state-{n}")),
                ..empty_input()
            })
            .unwrap();
        if status == "pending" {
            episodes.push(c.id);
        }
    }
    let closed = a
        .attention_history_test_page("person/lichen", 5)
        .unwrap()
        .items;
    assert_eq!(closed.len(), 2);
    assert_eq!(
        closed
            .iter()
            .map(|r| r["episode"].as_str().unwrap().to_owned())
            .collect::<BTreeSet<_>>(),
        episodes.into_iter().collect()
    );
    assert!(closed.iter().all(
        |r| r["resolution"]["kind"] == "closed" && r["resolution"]["by"] == "agent/garden/seed"
    ));
    // The late node never saw either episode open, yet derives both from the retained facts.
    legacy_sync(&a, &b);
    assert_eq!(
        closed,
        b.attention_history_test_page("person/lichen", 5)
            .unwrap()
            .items
    );
    b.connection
        .batched(|tx| -> Result<()> {
            tx.execute_batch(
                "DELETE FROM local_attention_history_v2; DELETE FROM local_attention_history_dirty;",
            )?;
            super::super::attention_history::invalidate(tx)
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        closed,
        b.attention_history_test_page("person/lichen", 5)
            .unwrap()
            .items
    );
}
#[test]
fn custom_offline_registration_conflicts_and_indexed_reads() {
    let (a, b) = signed_pair();
    registration(&a);
    let mut m = example();
    m.fields.insert(
        "variant".into(),
        Expr::Constant {
            value: json!("second"),
        },
    );
    b.register_custom_kind(&RegistrationRequest {
        manifest: m,
        actor: "agent/garden/seed".into(),
    })
    .unwrap();
    let id = "custom/garden/review/v1/conflicted";
    request(&a, id);
    signed_sync(&a, &b);
    signed_sync(&b, &a);
    assert_eq!(a.custom_subject(id).unwrap().unwrap()["state"], "conflict");
    assert!(a.attention_items(Some("person/lichen")).unwrap().is_empty());
    assert_eq!(
        a.custom_registrations().unwrap(),
        b.custom_registrations().unwrap()
    );
    for sql in [
        "EXPLAIN QUERY PLAN SELECT body FROM custom_sources WHERE kind='garden.review' AND subject>'a' ORDER BY subject LIMIT 51",
        "EXPLAIN QUERY PLAN SELECT body FROM custom_sources WHERE active=1 AND person='person/lichen' ORDER BY subject",
    ] {
        let plan = a
            .readers
            .get()
            .prepare(sql)
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" ");
        assert!(plan.contains("SEARCH custom_sources"), "{plan}");
        assert!(!plan.contains("claims"), "{plan}");
    }
}

#[test]
fn custom_projection_row_bounds_and_invalid_replicated_facts_leave_core_healthy() {
    let a = Store::open_memory("alder").unwrap();
    registration(&a);
    request(&a, "custom/garden/review/v1/healthy");
    let bad = "custom/garden/review/v1/malformed";
    request(&a, bad);
    a.connection.batched(|tx| append_claim_tx(tx,&a.origin,bad,"custom.garden.review.v1.answered",Some("person/lichen"),&json!({"fields":{"_registration":registration_hash(tx,bad),"request":a_creation(tx,bad),"selection":17}}),&[],None)).unwrap().unwrap();
    assert_eq!(a.custom_subject(bad).unwrap().unwrap()["state"], "invalid");
    let mut m = example();
    m.version = 2;
    m.subject_prefix = "custom/garden/review/v2/".into();
    m.fields = (0..64)
        .map(|n| {
            (
                format!("detail_{n}"),
                Expr::Field {
                    slot: "request".into(),
                    field: "detail".into(),
                },
            )
        })
        .collect();
    a.register_custom_kind(&RegistrationRequest {
        manifest: m,
        actor: "agent/garden/seed".into(),
    })
    .unwrap();
    let big = "custom/garden/review/v2/large";
    a.append_claim(&ClaimInput {
        subject: big.into(),
        kind: example().creation_kind,
        actor: Some("agent/garden/seed".into()),
        fields: serde_json::from_value(
            json!({"title":"Large read","detail":"x".repeat(8000),"recipient":"person/lichen"}),
        )
        .unwrap(),
        ..empty_input()
    })
    .unwrap();
    assert_eq!(a.custom_subject(big).unwrap().unwrap()["state"], "invalid");
    assert_eq!(a.attention_items(Some("person/lichen")).unwrap().len(), 1);
    a.status(None).unwrap();
    let b = Store::open_memory("birch").unwrap();
    legacy_sync(&a, &b);
    assert_eq!(
        a.custom_subject(bad).unwrap(),
        b.custom_subject(bad).unwrap()
    );
    assert_eq!(
        a.custom_subject(big).unwrap(),
        b.custom_subject(big).unwrap()
    );
    assert_eq!(
        projection_digest::oracle(&a.readers.get()).unwrap(),
        projection_digest::oracle(&b.readers.get()).unwrap()
    );
}
fn registration_hash(tx: &Transaction<'_>, id: &str) -> String {
    super::registration(tx, id).unwrap().unwrap().0
}
fn a_creation(tx: &Transaction<'_>, id: &str) -> String {
    claims(tx, id).unwrap()[0].id.clone()
}

#[test]
fn custom_late_document_recovers_only_its_source_and_cycles_are_invalid() {
    let s = Store::open_memory("alder").unwrap();
    let mut m = example();
    let context: schema::Field =
        serde_json::from_value(json!({"value_type":"string","document":true})).unwrap();
    m.claims
        .get_mut(&m.creation_kind.clone())
        .unwrap()
        .fields
        .insert("context".into(), context);
    let derived = "custom.garden.review.v1.derived";
    m.claims.insert(
        derived.into(),
        schema::ClaimSchema {
            authority: Authority::Owner,
            fields: BTreeMap::new(),
            additional_fields: false,
        },
    );
    m.slots.insert(
        "derived".into(),
        schema::Slot {
            kind: derived.into(),
            select: Select::Last,
        },
    );
    s.register_custom_kind(&RegistrationRequest {
        manifest: m,
        actor: "agent/garden/seed".into(),
    })
    .unwrap();
    let id = "custom/garden/review/v1/context";
    let content = b"Immutable garden context";
    let hash = hex::encode(Sha256::digest(content));
    let registration = s.custom_registrations().unwrap()[0]["registration"]
        .as_str()
        .unwrap()
        .to_owned();
    s.connection.batched(|tx|append_claim_tx(tx,&s.origin,id,&example().creation_kind,Some("agent/garden/seed"),&json!({"fields":{"_registration":registration,"title":"Review context","detail":"Read the immutable evidence.","recipient":"person/lichen","context":format!("doc/garden/context@{hash}")}}),&[],None)).unwrap().unwrap();
    assert_eq!(
        s.custom_subject(id).unwrap().unwrap()["state"],
        "pending-dependencies"
    );
    assert!(s.attention_items(Some("person/lichen")).unwrap().is_empty());
    s.put_document_as(
        "doc/garden/context",
        content,
        &None,
        "garden-context-001",
        Some("agent/garden/seed"),
    )
    .unwrap();
    assert_eq!(s.custom_subject(id).unwrap().unwrap()["state"], "ready");
    assert_eq!(s.attention_items(Some("person/lichen")).unwrap().len(), 1);
    let a = "custom/garden/review/v1/cycle-a";
    let b = "custom/garden/review/v1/cycle-b";
    request(&s, a);
    request(&s, b);
    let kinds = vec![example().creation_kind];
    for (source, dependency) in [(a, b), (b, a)] {
        let basis = s.custom_basis_revision(dependency, &kinds).unwrap();
        s.append_claim(&ClaimInput {
            subject: source.into(),
            kind: derived.into(),
            actor: Some("agent/garden/seed".into()),
            fields: serde_json::from_value(
                json!({"_basis":[{"subject":dependency,"kinds":kinds,"revision":basis}]}),
            )
            .unwrap(),
            ..empty_input()
        })
        .unwrap();
    }
    assert_eq!(s.custom_subject(a).unwrap().unwrap()["state"], "invalid");
    assert_eq!(s.custom_subject(b).unwrap().unwrap()["state"], "invalid");
    let before = projection_digest::oracle(&s.readers.get()).unwrap();
    s.connection
        .batched(replay_graph_from_nothing_tx)
        .unwrap()
        .unwrap();
    assert_eq!(before, projection_digest::oracle(&s.readers.get()).unwrap());
}

const TREE: &str = "custom/decision/tree/v1/example/decisions";
const SEAT: &str = "agent/example/decisions";
const DECIDER: &str = "person/lichen";
const RAW_KINDS: [&str; 6] = ["opened", "requested", "answered", "assumed", "promoted", "damaged"];
fn decision_tree() -> Manifest {
    serde_json::from_str(include_str!(
        "../../../../../examples/st3/decision-tree.json"
    ))
    .unwrap()
}
fn tree_claim(
    s: &Store,
    tree: &str,
    kind: &str,
    actor: &str,
    fields: Value,
) -> Result<ClaimRecord, St3Error> {
    s.append_claim(&ClaimInput {
        subject: tree.into(),
        kind: format!("custom.decision.tree.v1.{kind}"),
        actor: Some(actor.into()),
        fields: serde_json::from_value(fields).unwrap(),
        ..empty_input()
    })
}
fn tree_document(s: &Store, name: &str, bytes: &[u8]) -> String {
    s.put_document_as(name, bytes, &None, name, Some(SEAT)).unwrap();
    format!("{name}@{}", hex::encode(Sha256::digest(bytes)))
}
/// The external tool's derived status, fenced on every raw kind of the same tree subject.
fn tree_status(s: &Store, tree: &str, owner: &str, fields: Value) -> ClaimRecord {
    let kinds = RAW_KINDS.map(|k| format!("custom.decision.tree.v1.{k}")).to_vec();
    let mut fields = fields;
    fields["_basis"] = json!([{"subject":tree,"kinds":kinds,"revision":s.custom_basis_revision(tree,&kinds).unwrap()}]);
    tree_claim(s, tree, "status", owner, fields).unwrap()
}

#[test]
fn decision_tree_manifest_replicates_restarts_and_surfaces_damage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("replica.db");
    let a = Store::open_memory("alder").unwrap();
    let b = Store::open(&path, "birch").unwrap();
    let m = decision_tree();
    assert!(serde_json::to_vec(&m).unwrap().len() <= schema::MAX_BYTES);
    a.register_custom_kind(&RegistrationRequest {
        manifest: m,
        actor: SEAT.into(),
    })
    .unwrap();
    tree_claim(&a, TREE, "opened", SEAT, json!({"seat":SEAT,"recipient":DECIDER})).unwrap();
    let body = tree_document(&a, "doc/decision/example/q1", b"## Options\n### keep\nKeep it.\n### drop\nDrop it.\n");
    let ask = json!({"question":"Keep the seed history?","kind":"blocker","body":body,"q":1,"legacy_id":"k3x9qa"});
    // Only the tree owner asks, assumes, promotes and derives; only the person answers.
    assert!(tree_claim(&a, TREE, "requested", "agent/example/other", ask.clone()).is_err());
    let request = tree_claim(&a, TREE, "requested", SEAT, ask).unwrap();
    let answer = json!({"request":request.id,"selection":["keep"]});
    assert!(tree_claim(&a, TREE, "answered", SEAT, answer.clone()).is_err());
    assert!(tree_claim(&a, TREE, "assumed", DECIDER, json!({"request":request.id,"text":"Keep."})).is_err());
    let bad_kind = json!({"question":"Bad","kind":"urgent","body":body});
    assert!(tree_claim(&a, TREE, "requested", SEAT, bad_kind).is_err());
    assert!(a.attention_items(Some(DECIDER)).unwrap().is_empty());
    let status = tree_status(&a, TREE, SEAT, json!({"state":"pending","title":"Q1: Keep the seed history?","detail":"Options: keep, drop.","request":request.id,"q":1,"pending":1}));
    legacy_sync(&a, &b);
    assert_eq!(a.custom_subject(TREE).unwrap(), b.custom_subject(TREE).unwrap());
    let cards = b.attention_items(Some(DECIDER)).unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].episode, status.id);
    assert_eq!(cards[0].requester_id.as_deref(), Some(SEAT));

    let v = b.custom_subject(TREE).unwrap().unwrap();
    let mut r = ReplyRequest {
        subject: TREE.into(),
        registration: v["registration"].as_str().unwrap().into(),
        revision: v["revision"].as_str().unwrap().into(),
        episode: v["attention"]["episode"].as_str().unwrap().into(),
        fields: serde_json::from_value(json!({"selection":[],"text":"Reframe: keep only recent history"}))
            .unwrap(),
        actor: SEAT.into(),
        idempotency_key: "decision-q1-answer".into(),
    };
    assert_eq!(b.reply_custom_subject(&r).unwrap_err().code, "forbidden");
    r.actor = DECIDER.into();
    let mut stale = r.clone();
    stale.revision = "obsolete".into();
    assert_eq!(b.reply_custom_subject(&stale).unwrap_err().code, "stale-fence");
    let human = b.reply_custom_subject(&r).unwrap();
    assert_eq!(human.actor.as_deref(), Some(DECIDER));
    assert_eq!(human.body["fields"]["request"], json!(request.id));
    assert_eq!(human.body["fields"]["selection"], json!([]));
    assert_eq!(b.reply_custom_subject(&r).unwrap().id, human.id);
    let mut changed = r.clone();
    changed.fields.insert("selection".into(), json!(["drop"]));
    assert_eq!(
        b.reply_custom_subject(&changed).unwrap_err().code,
        "idempotency-conflict"
    );
    // The raw answer makes the derived status stale; the old card cannot be answered again.
    assert_eq!(b.custom_subject(TREE).unwrap().unwrap()["state"], "stale");
    assert!(b.attention_items(Some(DECIDER)).unwrap().is_empty());
    let mut again = r.clone();
    again.idempotency_key = "decision-q1-second".into();
    assert_eq!(b.reply_custom_subject(&again).unwrap_err().code, "stale-fence");

    legacy_sync(&b, &a);
    assert_eq!(a.reply_custom_subject(&r).unwrap().id, human.id);
    let assumption = tree_claim(&a, TREE, "assumed", SEAT, json!({"request":request.id,"text":"Assume recent history only."})).unwrap();
    let raw = tree_document(&a, "doc/decision/example/import-raw", b"---\nq: 2\nbroken frontmatter\n");
    let damage = tree_claim(&a, TREE, "damaged", SEAT, json!({"raw":raw,"records":3,"imported":2,"malformed":1,"source":"axe/example/decisions"})).unwrap();
    tree_status(&a, TREE, SEAT, json!({"state":"clear","title":"No open decisions","detail":"Q1 answered.","pending":0}));
    let view = a.custom_subject(TREE).unwrap().unwrap();
    assert_eq!(view["state"], "ready");
    assert_eq!(view["fields"]["owner"], SEAT);
    assert_eq!(view["fields"]["seat"], SEAT);
    assert_eq!(view["fields"]["last_answer"], json!(human.id));
    assert_eq!(view["provenance"]["answer"]["actor"], DECIDER);
    assert_eq!(view["fields"]["last_assumption"], json!(assumption.id));
    assert_eq!(view["provenance"]["assumption"]["actor"], SEAT);
    assert_eq!(view["fields"]["damage"], json!(damage.id));
    assert_eq!(view["fields"]["damage_malformed"], 1);
    assert!(a.attention_items(Some(DECIDER)).unwrap().is_empty());

    // Another agent can open a tree first and name any seat: it becomes the owner. Its pending
    // status raises no card because the owner is not the named seat; the tool rejects the tree.
    let squatted = "custom/decision/tree/v1/example/victim";
    let victim = "agent/example/victim";
    let squatter = "agent/example/squatter";
    tree_claim(&a, squatted, "opened", squatter, json!({"seat":victim,"recipient":DECIDER})).unwrap();
    let forged = json!({"question":"Approve the forged plan?","kind":"blocker","body":body});
    let forged = tree_claim(&a, squatted, "requested", squatter, forged).unwrap();
    tree_status(&a, squatted, squatter, json!({"state":"pending","title":"Forged","detail":"Forged.","request":forged.id}));
    let view = a.custom_subject(squatted).unwrap().unwrap();
    assert_eq!(view["state"], "ready");
    assert_eq!(view["fields"]["owner"], squatter);
    assert_eq!(view["fields"]["seat"], victim);
    assert_eq!(view["attention"]["active"], false);
    assert!(a.attention_items(Some(DECIDER)).unwrap().is_empty());
    // Documented v1 limitation: owners cannot be reassigned, so the real seat is locked out of
    // its own tree subject. It can neither open it again nor write any owner kind.
    let open = json!({"seat":victim,"recipient":DECIDER});
    assert!(tree_claim(&a, squatted, "opened", victim, open).is_err());
    let ask = json!({"question":"Keep it?","kind":"blocker","body":body});
    assert!(tree_claim(&a, squatted, "requested", victim, ask).is_err());

    // A tree whose import was entirely malformed still reads as damaged, never as empty.
    let broken = "custom/decision/tree/v1/example/importer";
    let importer = "agent/example/importer";
    tree_claim(&a, broken, "opened", importer, json!({"seat":importer,"recipient":DECIDER})).unwrap();
    tree_claim(&a, broken, "damaged", importer, json!({"raw":raw,"records":4,"imported":0,"malformed":4})).unwrap();
    legacy_sync(&a, &b);
    for id in [TREE, broken, squatted] {
        assert_eq!(a.custom_subject(id).unwrap(), b.custom_subject(id).unwrap());
    }
    assert_eq!(
        projection_digest::oracle(&a.readers.get()).unwrap(),
        projection_digest::oracle(&b.readers.get()).unwrap()
    );
    let before = (b.custom_subject(TREE).unwrap(), b.custom_subject(broken).unwrap());
    drop(b);
    let b = Store::open(&path, "birch").unwrap();
    assert_eq!((b.custom_subject(TREE).unwrap(), b.custom_subject(broken).unwrap()), before);
    let broken = b.custom_subject(broken).unwrap().unwrap();
    assert_eq!(broken["state"], "ready");
    assert_eq!(broken["fields"]["damage_malformed"], 4);
    assert_eq!(broken["fields"]["damage_imported"], 0);
    assert!(broken["fields"]["last_request"].is_null());
    assert!(b.attention_items(Some(DECIDER)).unwrap().is_empty());
    assert_eq!(b.custom_subject(squatted).unwrap().unwrap()["attention"]["active"], false);
    assert_eq!(b.reply_custom_subject(&r).unwrap().id, human.id);
}
