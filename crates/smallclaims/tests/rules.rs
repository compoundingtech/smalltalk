//! Rules on the plain runtime: audit records a would-deny and lets the write through, enforce
//! refuses it, off does nothing, the latest setting of a rule wins, and a rule set on one member
//! applies on another once it syncs.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};
use smallclaims::claim::{ClaimInput, ClaimRecord};
use smallclaims::error::Error;
use smallclaims::rules::{Mode, RULE_AUDITED, RULE_SET, Rule};
use smallclaims::store::Store;
use smallclaims::store::runtime::Plain;

const FLEET: &str = "0b8e5c1a-7d2f-4e6a-9c3b-5a1d8f2e7b40";

fn store(name: &str) -> Store {
    let store = Store::open_memory(name, Arc::new(Plain)).unwrap();
    store.bind_fleet(FLEET).unwrap();
    store
}

fn write(store: &Store, actor: &str, kind: &str, subject: &str) -> Result<ClaimRecord, Error> {
    store.append_claim(&ClaimInput {
        subject: subject.into(),
        kind: kind.into(),
        actor: Some(actor.into()),
        fields: BTreeMap::from([("text".into(), Value::String("hello".into()))]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: None,
    })
}

fn set_rule(store: &Store, name: &str, mode: Mode) {
    let rule = Rule {
        mode,
        description: "an agent publishes only under its own prefix".into(),
        actors: vec!["agent/**".into()],
        except: vec![],
        kinds: vec!["doc.bound".into()],
        subjects: vec!["doc/**".into()],
        unless_subjects: vec!["doc/{actor}/**".into()],
    };
    write_rule(store, name, &rule);
}

fn write_rule(store: &Store, name: &str, rule: &Rule) {
    store
        .append_claim(&ClaimInput {
            subject: format!("rule/{name}"),
            kind: RULE_SET.into(),
            actor: Some("person/ada".into()),
            fields: rule.fields(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

fn audits(store: &Store, name: &str) -> Vec<Value> {
    store
        .claims_for(&format!("rule/{name}"), Some(RULE_AUDITED))
        .unwrap()
        .into_iter()
        .map(|claim| claim.body["fields"].clone())
        .collect()
}

fn sync(from: &Store, to: &Store) {
    let exchange = from
        .export_replication_exchange(FLEET, &to.replication_inventory().unwrap())
        .unwrap();
    to.receive_replication_exchange(&from.origin, FLEET, &exchange)
        .unwrap();
    to.validate_replication_backlog().unwrap();
    to.project_replication_backlog().unwrap();
}

#[test]
fn audit_records_a_would_deny_and_the_write_proceeds() {
    let store = store("studio");
    set_rule(&store, "own-prefix", Mode::Audit);
    let elsewhere = write(
        &store,
        "agent/example/reviewer",
        "doc.bound",
        "doc/elsewhere/notes",
    )
    .unwrap();
    write(
        &store,
        "agent/example/reviewer",
        "doc.bound",
        "doc/example/reviewer/notes",
    )
    .unwrap();
    write(&store, "person/ada", "doc.bound", "doc/elsewhere/notes").unwrap();
    assert!(store.claim_by_id(&elsewhere.id).unwrap().is_some());
    let audits = audits(&store, "own-prefix");
    assert_eq!(audits.len(), 1);
    assert_eq!(
        audits[0],
        json!({
            "rule": store.claims_for("rule/own-prefix", Some(RULE_SET)).unwrap()[0].id,
            "actor": "agent/example/reviewer",
            "action": "doc.bound",
            "target": "doc/elsewhere/notes",
        })
    );
}

#[test]
fn enforce_refuses_with_a_typed_reason_and_off_does_nothing() {
    let store = store("studio");
    set_rule(&store, "own-prefix", Mode::Enforce);
    let refused = write(
        &store,
        "agent/example/reviewer",
        "doc.bound",
        "doc/elsewhere/notes",
    )
    .unwrap_err();
    assert_eq!(refused.code, "rule-denied");
    assert!(
        refused.message.contains("own-prefix"),
        "{}",
        refused.message
    );
    assert!(
        refused.message.contains("its own prefix"),
        "{}",
        refused.message
    );
    assert!(
        store
            .claims_for("doc/elsewhere/notes", None)
            .unwrap()
            .is_empty()
    );
    write(
        &store,
        "agent/example/reviewer",
        "doc.bound",
        "doc/example/reviewer/notes",
    )
    .unwrap();

    // The latest setting wins.
    set_rule(&store, "own-prefix", Mode::Off);
    write(
        &store,
        "agent/example/reviewer",
        "doc.bound",
        "doc/elsewhere/notes",
    )
    .unwrap();
    assert!(audits(&store, "own-prefix").is_empty());
}

#[test]
fn a_rule_set_on_one_member_applies_on_another_after_sync() {
    let a = store("a");
    let b = store("b");
    set_rule(&a, "own-prefix", Mode::Enforce);
    write(
        &b,
        "agent/example/reviewer",
        "doc.bound",
        "doc/elsewhere/before",
    )
    .unwrap();
    sync(&a, &b);
    assert_eq!(
        write(
            &b,
            "agent/example/reviewer",
            "doc.bound",
            "doc/elsewhere/after"
        )
        .unwrap_err()
        .code,
        "rule-denied"
    );
}

#[test]
fn writes_without_an_actor_are_the_nodes_own_and_pass() {
    let store = store("studio");
    write_rule(
        &store,
        "everything",
        &Rule {
            mode: Mode::Enforce,
            actors: vec!["**".into()],
            kinds: vec!["**".into()],
            ..Rule::default()
        },
    );
    store
        .append_claim(&ClaimInput {
            subject: "doc/anything".into(),
            kind: "doc.bound".into(),
            actor: None,
            fields: BTreeMap::new(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    assert_eq!(
        write(&store, "person/ada", "doc.bound", "doc/x")
            .unwrap_err()
            .code,
        "rule-denied"
    );
    // A rule never locks its owner out: a person can still turn it off, and an agent cannot.
    assert_eq!(
        write(
            &store,
            "agent/example/reviewer",
            RULE_SET,
            "rule/everything"
        )
        .unwrap_err()
        .code,
        "rule-denied"
    );
    write_rule(
        &store,
        "everything",
        &Rule {
            mode: Mode::Off,
            ..Rule::default()
        },
    );
    write(&store, "person/ada", "doc.bound", "doc/x").unwrap();
}
