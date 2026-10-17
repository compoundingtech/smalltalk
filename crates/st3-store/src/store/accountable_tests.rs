use super::*;

const ROOT: &str = "agent/example/root";
const PERSON: &str = "person/avery";
const SOURCE: &str =
    "version 2\nagent \"example/root\" { accountable \"person/avery\"; harness \"codex\" {}; }";

fn intent() -> NormalizedIntent {
    crate::graph::parse_internal_intent(SOURCE, "alder").unwrap()
}

#[test]
fn accountable_root_publication_requires_same_person_and_round_trips() {
    let store = Store::open_memory("alder").unwrap();
    let intent = intent();
    let expected = BTreeMap::from([(ROOT.into(), Vec::new())]);
    for actor in [None, Some("agent/example/parent"), Some("person/robin")] {
        let error = store
            .apply_as(&intent, &expected, "refused", actor)
            .unwrap_err();
        assert_eq!(error.code, "accountable-root-forbidden");
        assert!(store.desired_subject_with_writer(ROOT).unwrap().is_none());
    }
    store
        .apply_as(&intent, &expected, "root", Some(PERSON))
        .unwrap();
    let (desired, author) = store.desired_subject_with_writer(ROOT).unwrap().unwrap();
    assert_eq!(desired, intent.subjects[ROOT]);
    assert_eq!(author.as_deref(), Some(PERSON));
    let claim = store
        .latest_claim(ROOT, Some("intent.desired"))
        .unwrap()
        .unwrap();
    assert_eq!(claim.body["accountable"], PERSON);
    let (_, tree) = store.agent_declaration(ROOT, None).unwrap().unwrap();
    let rendered = crate::graph::render_agent_desired_kdl(&tree).unwrap();
    assert_eq!(
        crate::graph::parse_internal_intent(&rendered, "alder")
            .unwrap()
            .subjects[ROOT]
            .accountable
            .as_deref(),
        Some(PERSON)
    );
    store.rebuild_claim_projections().unwrap();
    assert_eq!(
        store.desired_subject_with_writer(ROOT).unwrap().unwrap().0,
        desired
    );
}

#[test]
fn accountable_root_raw_and_replica_paths_reject_forgery_and_unknown_stays_unknown() {
    let store = Store::open_memory("alder").unwrap();
    let intent = intent();
    let body = serde_json::to_value(&intent.subjects[ROOT]).unwrap();
    for actor in [None, Some("agent/example/child"), Some("person/robin")] {
        let error = store
            .connection
            .batched(|tx| {
                append_claim_tx(tx, "alder", ROOT, "intent.desired", actor, &body, &[], None)
            })
            .unwrap()
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("only the named accountable person")
        );
    }
    let generic =
        json!({"fields": {"kind": "agent", "desired": body["desired"], "accountable": PERSON}});
    assert!(
        store
            .connection
            .batched(|tx| append_claim_tx(
                tx,
                "alder",
                ROOT,
                "intent.desired",
                Some(PERSON),
                &generic,
                &[],
                None
            ))
            .unwrap()
            .is_err()
    );
    let record = store
        .connection
        .batched(|tx| {
            append_claim_tx(
                tx,
                "alder",
                ROOT,
                "intent.desired",
                Some(PERSON),
                &body,
                &[],
                None,
            )
        })
        .unwrap()
        .unwrap();
    assert!(matches!(
        classify_replicated_claim_with_registry(&record, st3_schema::registry()).unwrap(),
        ReplicatedClaimAdmission::Valid
    ));
    for actor in [None, Some("agent/example/child"), Some("person/robin")] {
        let mut forged = record.clone();
        forged.actor = actor.map(str::to_owned);
        assert_eq!(
            classify_replicated_claim_with_registry(&forged, st3_schema::registry())
                .err()
                .unwrap()
                .code,
            "accountable-root-forbidden"
        );
    }
    for mutate in [
        "missing-field",
        "different-field",
        "missing-child",
        "mission-root",
    ] {
        let mut forged = record.clone();
        match mutate {
            "missing-field" => {
                forged.body.as_object_mut().unwrap().remove("accountable");
            }
            "different-field" => forged.body["accountable"] = json!("person/robin"),
            "missing-child" => forged.body["desired"]["children"]
                .as_array_mut()
                .unwrap()
                .retain(|v| v["name"] != "accountable"),
            _ => forged.body["owner_run"] = json!("mission-run/example/run"),
        }
        assert_eq!(
            classify_replicated_claim_with_registry(&forged, st3_schema::registry())
                .err()
                .unwrap()
                .code,
            "invalid-accountable-root",
            "{mutate}"
        );
    }
    let mut old = st3_schema::registry().clone();
    old.claims
        .get_mut("intent.desired")
        .unwrap()
        .fields
        .remove("accountable");
    assert!(matches!(
        classify_replicated_claim_with_registry(&record, &old).unwrap(),
        ReplicatedClaimAdmission::UnknownField
    ));
}

#[test]
fn accountable_root_grammar_rejects_nonpersons_duplicates_and_nested_values() {
    for child in [
        "accountable \"avery\"",
        "accountable \"agent/example\"",
        "accountable \"person/a/b\"",
        "accountable \"person/\"",
        "accountable \"person/..\"",
        "accountable \"person/avery\" extra=\"x\"",
        "accountable \"person/avery\" { name \"x\"; }",
        "accountable \"person/avery\" \"person/robin\"",
        "accountable \"person/avery\"; accountable \"person/avery\"",
    ] {
        let source =
            format!("version 2\nagent \"example/root\" {{ {child}; harness \"codex\" {{}}; }}");
        assert!(
            crate::graph::parse_internal_intent(&source, "alder").is_err(),
            "{child}"
        );
    }
    let source = format!(
        "version 2\nagent \"example/root\" {{ accountable \"person/{}\"; command \"true\"; }}",
        "x".repeat(506)
    );
    assert!(
        crate::graph::parse_internal_intent(&source, "alder").is_err(),
        "person reference exceeds the schema subject bound"
    );
    let mission = "version 2\nmission \"example/work\" state=\"ready\" { goal \"Work.\"; agent \"worker\" { accountable \"person/avery\"; command \"true\"; }; step \"work\" { assigned-to \"agent/worker\"; goal \"Work.\"; }; }";
    assert_eq!(
        crate::graph::parse_internal_intent(mission, "alder")
            .unwrap_err()
            .code,
        "invalid-accountable-root"
    );
    let mut desired = intent().subjects[ROOT].clone();
    desired.owner_run = Some("mission-run/example/run".into());
    assert!(
        st3_schema::accountable::validate_claim(
            ROOT,
            "intent.desired",
            &serde_json::to_value(desired).unwrap(),
            Some(PERSON)
        )
        .is_err()
    );
}

#[test]
fn accountable_absent_preserves_previous_cbor_bytes_and_desired_revisions() {
    // The pre-accountable derived serializer is the compatibility oracle, including
    // its definite CBOR map and the field order used by desired_revision.
    #[derive(serde::Serialize)]
    struct PreviousDesiredSubject {
        subject: String,
        kind: String,
        desired: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        member: Option<crate::model::MemberSpec>,
        #[serde(skip_serializing_if = "Option::is_none")]
        owner_run: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        owner_generation: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        owner_step: Option<String>,
    }
    fn previous(desired: &DesiredSubject) -> PreviousDesiredSubject {
        PreviousDesiredSubject {
            subject: desired.subject.clone(),
            kind: desired.kind.clone(),
            desired: canonical_json_value(&desired.desired),
            member: desired.member.clone(),
            owner_run: desired.owner_run.clone(),
            owner_generation: desired.owner_generation.clone(),
            owner_step: desired.owner_step.clone(),
        }
    }
    fn bytes(value: &impl serde::Serialize) -> Vec<u8> {
        let mut bytes = Vec::new();
        ciborium::into_writer(value, &mut bytes).unwrap();
        bytes
    }

    let minimal = DesiredSubject {
        accountable: None,
        subject: ROOT.into(),
        kind: "stop".into(),
        desired: Value::Null,
        member: None,
        owner_run: None,
        owner_generation: None,
        owner_step: None,
    };
    let exact = hex::decode("a3677375626a656374726167656e742f6578616d706c652f726f6f74646b696e646473746f706764657369726564f6").unwrap();
    assert_eq!(bytes(&previous(&minimal)), exact);
    assert_eq!(bytes(&minimal), exact);
    assert_eq!(desired_revision(&minimal), "e9fcc02577cf489c512b41ff9998ade7db1e9c12a66d8269fe13e379757da944");
    assert_eq!(canonical_hash(&previous(&minimal)).unwrap(), desired_revision(&minimal));

    let fixture: Value = serde_json::from_str(include_str!(
        "../../../st3-daemon/tests/fixtures/harness-access-previous-build.json"
    )).unwrap();
    for body in fixture["desired_subjects"].as_array().unwrap() {
        let mut desired: DesiredSubject = serde_json::from_value(body.clone()).unwrap();
        desired.desired = canonical_json_value(&desired.desired);
        assert_eq!(bytes(&desired), bytes(&previous(&desired)), "{}", desired.subject);
        assert_eq!(desired_revision(&desired), fixture["revisions"][&desired.subject].as_str().unwrap());
        // Every combination of optional owner fields keeps the old encoding too.
        for present in 0..8 {
            desired.owner_run = (present & 1 != 0).then(|| "mission-run/example/run".into());
            desired.owner_generation = (present & 2 != 0).then(|| "run-generation/example".into());
            desired.owner_step = (present & 4 != 0).then(|| "work".into());
            assert_eq!(bytes(&desired), bytes(&previous(&desired)), "{} owners={present}", desired.subject);
            assert_eq!(desired_revision(&desired), canonical_hash(&previous(&desired)).unwrap());
        }
    }
}
