use super::*;
use crate::external_sessions::{ExternalDriver, TimelineOrder};

fn validator() -> jsonschema::Validator {
    let schema_path = std::path::Path::new(test_env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/st3/client-v0/schemas/client-v0.schema.json");
    let mut schema: Value =
        serde_json::from_str(&std::fs::read_to_string(schema_path).unwrap()).unwrap();
    schema.as_object_mut().unwrap().remove("$id");
    schema.as_object_mut().unwrap().remove("oneOf");
    schema["$ref"] = json!("#/$defs/TimelineEntry");
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .should_validate_formats(true)
        .build(&schema)
        .unwrap()
}

fn conform(validator: &jsonschema::Validator, entries: &[Value]) {
    assert!(!entries.is_empty());
    for entry in entries {
        let errors: Vec<_> = validator.iter_errors(entry).map(|error| error.to_string()).collect();
        assert!(errors.is_empty(), "{errors:#?}\n{entry:#}");
    }
}

#[test]
fn every_native_reader_output_conforms_to_client_v0_timeline_entry() {
    let validator = validator();
    let fixtures = [
        (ExternalDriver::Omp, include_str!("../../../../../st3-daemon/fixtures/native-records/omp-parity.jsonl")),
        (ExternalDriver::Pi, include_str!("../../../../../st3-daemon/fixtures/native-records/omp-parity.jsonl")),
        (ExternalDriver::Claude, include_str!("../../../../../st3-daemon/fixtures/native-records/claude.jsonl")),
        (ExternalDriver::Codex, include_str!("../../../../../st3-daemon/fixtures/native-records/codex.jsonl")),
        (ExternalDriver::Omp, include_str!("../../../../../st3-daemon/fixtures/native-records/schema-conformance.jsonl")),
    ];
    let mut exported = Vec::new();
    for (driver, records) in fixtures {
        let root = tempfile::tempdir().unwrap();
        let mut native = super::tests::fixture(root.path(), json!([]));
        native.driver = driver;
        std::fs::write(&native.transcript, records).unwrap();
        let normalized = crate::external_sessions::normalized_timeline(&native).unwrap();
        for negotiated in [true, false] {
            let mut session = ClientSession::local(Some("person/ada")).unwrap();
            session.conversation_blocks = negotiated;
            let prepared = prepare(&native, &session, &native.id, normalized.clone()).unwrap();
            conform(&validator, &prepared);
            exported.extend(prepared);
            conform(&validator, &read(&native, &session, &native.id).unwrap());
            for order in [TimelineOrder::Sequence, TimelineOrder::TimestampSequence] {
                conform(&validator, &read_slice(&native, &session, &native.id, order, None, usize::MAX).unwrap().items);
            }
            for entry in &normalized {
                if let Some(locator) = entry.get("_source") {
                    let locator = serde_json::from_value(locator.clone()).unwrap();
                    let record = crate::external_sessions::normalized_record(&native, &locator).unwrap();
                    conform(&validator, &prepare(&native, &session, &native.id, record).unwrap());
                }
            }
        }
    }
    // Exercise the real bounded-reader truncation producer, not a manual body.
    let root = tempfile::tempdir().unwrap();
    let native = super::tests::fixture(root.path(), json!([]));
    let record = json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"Synthetic bounded history"}]}});
    std::fs::write(&native.transcript, format!("{record}\n").repeat(4_097)).unwrap();
    let session = ClientSession::local(Some("person/ada")).unwrap();
    let entries = read(&native, &session, &native.id).unwrap();
    let notice = entries.iter().find(|entry| entry["type"] == "truncation").unwrap();
    assert!(notice["body"].get("fetchable").is_none());
    assert!(notice["body"].get("limit_bytes").is_none());
    assert!(notice["body"]["reason"].as_str().unwrap().contains("not fetchable"));
    conform(&validator, &entries);
    exported.push(notice.clone());
    // The SDK gate consumes these actual producer outputs, not hand-written lookalikes.
    if let Some(path) = std::env::var_os("ST_TIMELINE_CONFORMANCE_OUTPUT") {
        std::fs::write(path, serde_json::to_vec(&exported).unwrap()).unwrap();
    }
}

#[test]
fn incomplete_native_views_preserve_raw_records_without_invalid_projections() {
    let root = tempfile::tempdir().unwrap();
    let native = super::tests::fixture(root.path(), json!([]));
    std::fs::write(&native.transcript, include_str!("../../../../../st3-daemon/fixtures/native-records/schema-conformance.jsonl")).unwrap();
    let entries = crate::external_sessions::normalized_timeline(&native).unwrap();
    let blocks: Vec<_> = entries.iter().flat_map(|entry| entry["body"]["blocks"].as_array().into_iter().flatten()).collect();
    assert!(blocks.iter().any(|block| block["view"]["wall_ms"] == 13));
    assert!(blocks.iter().any(|block| block["view"]["started_at"] == "2026-10-12T00:00:01.000Z"));
    assert!(blocks.iter().any(|block| block["payload"]["raw"]["customType"] == "tool_execution_start" && block.get("view").is_none()));
    let jobs: Vec<_> = blocks.iter().flat_map(|block| block["view"]["jobs"].as_array().into_iter().flatten()).collect();
    assert_eq!(jobs.len(), 2);
    assert!(jobs.iter().all(|job| job["id"].is_string() && job["state"] == "unknown"));
    assert!(jobs.iter().any(|job| job["duration_ms"] == 2));
    let thinking: Vec<_> = blocks
        .iter()
        .filter(|block| block["view"]["type"] == "thinking_level")
        .collect();
    assert_eq!(thinking.len(), 2);
    assert!(thinking[0]["view"].get("configured").is_none());
    assert_eq!(thinking[0]["payload"]["raw"]["configured"], "high");
    assert_eq!(thinking[1]["view"]["configured"], false);
}

#[test]
fn daemon_sender_is_an_explicit_known_schema_gap_issue_1957() {
    // https://github.com/compoundingtech/smalltalk/issues/1957
    // Pending intake amendment: genuine runtime-authored messages cannot satisfy
    // ParticipantRef. Never omit/relabel the sender or silently skip this case.
    let entry = json!({
        "id":"timeline-entry/ada", "sequence":0, "revision":1,
        "timestamp":"2026-10-12T00:00:00.000Z", "role":"system", "type":"message", "final":true,
        "body":{"message_id":"message/tester", "from":"daemon/runtime", "to":"agent/ada"}
    });
    let validator = validator();
    let errors: Vec<_> = validator.iter_errors(&entry).collect();
    assert_eq!(errors.len(), 1, "known gap changed; update the intake and remove this expected rejection: {errors:#?}");
    assert_eq!(errors[0].instance_path.as_str(), "/body/from");
    let mut valid = entry;
    valid["body"]["from"] = json!("person/ada");
    conform(&validator, &[valid]);
}
