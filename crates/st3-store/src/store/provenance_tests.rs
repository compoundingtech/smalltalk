//! Provenance tests that publish through the store; they live here because the provenance module sits
//! below the store.

#![allow(unused_imports)]

use crate::model::{IntentInput, St3Error};
use std::collections::BTreeMap;
use crate::provenance::tests::*;
use crate::provenance::*;
use crate::{parse_intent, store::Store};

fn publish(store: &Store, source: &str, key: &str) -> Result<(), St3Error> {
    let intent = parse_intent(source, "orchard").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store.apply(&intent, &preview.subject_tokens, key)?;
    Ok(())
}

#[test]
fn provenance_never_changes_the_mission_or_step_hash() {
    let plain = parse_intent(&source("Build.", None), "orchard").unwrap();
    let with = parse_intent(&source("Build.", Some("Approved source.")), "orchard").unwrap();
    assert_eq!(plain.missions, with.missions);
    assert_eq!(
        serde_json::to_vec(&plain.missions).unwrap(),
        serde_json::to_vec(&with.missions).unwrap()
    );
    assert_ne!(plain.source_hash, with.source_hash);
    assert_eq!(with.mission_provenance["orchard"].decision.len(), 2);
}

#[test]
fn publication_fixes_provenance_per_revision_and_keeps_history() {
    let store = Store::open_memory("orchard").unwrap();
    let first = source("Build.", Some("First source."));
    publish(&store, &first, "first").unwrap();
    let revision = store
        .mission_spec("orchard", None)
        .unwrap()
        .unwrap()
        .revision;
    let original = store
        .mission_provenance("orchard", &revision)
        .unwrap()
        .unwrap();
    publish(&store, &first, "repeat").unwrap();
    publish(&store, &source("Build.", None), "omit").unwrap();
    let mutation = source("Build.", Some("Changed source."));
    assert_eq!(
        publish(&store, &mutation, "mutate").unwrap_err().code,
        "immutable-mission-provenance"
    );
    assert_eq!(
        store.mission_provenance("orchard", &revision).unwrap(),
        Some(original.clone())
    );
    publish(
        &store,
        &source("Build again.", Some("Second source.")),
        "second",
    )
    .unwrap();
    let next = store
        .mission_spec("orchard", None)
        .unwrap()
        .unwrap()
        .revision;
    assert_ne!(revision, next);
    assert_eq!(
        store.mission_provenance("orchard", &revision).unwrap(),
        Some(original)
    );
    assert_eq!(
        store
            .mission_provenance("orchard", &next)
            .unwrap()
            .unwrap()
            .reason,
        "Second source."
    );
    publish(&store, &source("Build without metadata.", None), "plain").unwrap();
    let plain = store.mission_spec("orchard", None).unwrap().unwrap();
    assert!(
        store
            .mission_provenance("orchard", &plain.revision)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        publish(
            &store,
            &source("Build without metadata.", Some("Late source.")),
            "late"
        )
        .unwrap_err()
        .code,
        "immutable-mission-provenance"
    );
}

#[test]
fn provenance_rejects_malformed_and_oversized_blocks() {
    let valid = source("Build.", Some("Source."));
    for invalid in [
        valid.replace("reason \"Source.\"", "reason \"\""),
        valid.replace(
            "reason \"Source.\"",
            "reason \"Source.\"; reason \"Duplicate.\"",
        ),
        valid.replace(
            "renderer \"orchard-renderer-v1\"",
            "command \"do-something\"",
        ),
        valid.replace("missions/orchard.kdl", "../orchard.kdl"),
        valid.replace(&"a".repeat(40), "deadbeef"),
        valid.replace("reason \"Source.\"", "reason \"Source.\" \"extra\""),
        valid.replace("source {", "source unexpected=\"value\" {"),
        source("Build.", Some(&"x".repeat(2049))),
        valid.replace(
            "decision \"decision/opaque-approved-scope\"",
            &"decision \"opaque\";".repeat(33),
        ),
        valid.replace(
            "decision \"decision/opaque-approved-scope\"",
            &format!("decision {:?};", "x".repeat(1025)),
        ),
    ] {
        assert!(parse_intent(&invalid, "orchard").is_err(), "{invalid}");
    }
    let full = valid.replace(
        "decision \"decision/opaque-approved-scope\"",
        &format!("decision {:?};", "x".repeat(1024)).repeat(16),
    );
    assert!(
        parse_intent(&full, "orchard")
            .unwrap_err()
            .message
            .contains("16 KiB")
    );
}

#[test]
fn renderer_and_references_are_inert_literals() {
    let source =
        source("Build.", Some("Source.")).replace("orchard-renderer-v1", "$(do-not-execute)");
    let intent = parse_intent(&source, "orchard").unwrap();
    assert!(intent.subjects.is_empty());
    assert!(intent.document_refs.is_empty());
    assert_eq!(
        intent.mission_provenance["orchard"].source.renderer,
        "$(do-not-execute)"
    );
    let bare = source.replace("doc/orchard/proof@opaque-reference", "doc/orchard/proof");
    let parsed = parse_intent(&bare, "orchard").unwrap();
    assert!(parsed.document_refs.is_empty());
    let resolved = crate::graph::resolve_document_references(
        &bare,
        &BTreeMap::from([("doc/orchard/proof".into(), "b".repeat(64))]),
    )
    .unwrap();
    assert_eq!(
        parse_intent(&resolved, "orchard")
            .unwrap()
            .mission_provenance,
        parsed.mission_provenance
    );
}
