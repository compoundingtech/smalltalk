use super::*;
use crate::fleet::MemberKey;
use crate::model::PersonStepResponse;
use rusqlite::StatementStatus;

const FLEET: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";

fn claim(store: &Store, subject: &str, kind: &str, actor: Option<&str>, fields: Value) -> ClaimRecord {
    store.append_claim(&ClaimInput {
        subject: subject.into(), kind: kind.into(), actor: actor.map(str::to_owned),
        fields: serde_json::from_value(fields).unwrap(), evidence: Vec::new(),
        expected_subject: None, idempotency_key: None,
    }).unwrap()
}

fn advertise(store: &Store, supported: bool) {
    claim(store, &format!("daemon/{}", store.origin), "daemon.started", None,
        json!({"status":"running", "features":{"person_directive_note": if supported { 1 } else { 0 }}}));
}

fn enable(store: &Store) -> Arc<MemberKey> {
    let key = Arc::new(MemberKey::generate().unwrap().0);
    store.bind_fleet(FLEET).unwrap();
    store.pin_fleet_anchor(key.public()).unwrap();
    store.set_member_key(Some(key.clone())).unwrap();
    store.admit_fleet_anchor(FLEET, key.public(), "listening").unwrap();
    advertise(store, true);
    key
}

fn store() -> Store {
    let store = Store::open_memory("alder").unwrap();
    enable(&store);
    store
}

fn sync(from: &Store, to: &Store) -> ReplicationAdmission {
    let exchange = from.export_replication_exchange_answering(FLEET,
        &to.replication_inventory().unwrap(), &to.replication_signature_requests().unwrap()).unwrap();
    to.receive_replication_exchange(&from.origin, FLEET, &exchange).unwrap();
    let admission = to.validate_replication_backlog().unwrap();
    assert!(to.project_replication_backlog().unwrap());
    admission
}

fn declare(store: &Store, source: &str, actor: &str, key: &str) -> NormalizedIntent {
    let intent = crate::graph::parse_internal_intent(source, &store.origin).unwrap();
    let planned = store.mission(&intent, IntentInput { kdl: source.into(), source_name: None }).unwrap();
    store.apply_as(&intent, &planned.subject_tokens, key, Some(actor)).unwrap();
    intent
}

#[test]
fn directive_note_lifecycle_author_time_bounds_expiry_and_tombstones() {
    let store = store();
    assert!(store.directive_notes("person/avery").unwrap().is_empty());
    let first = store.set_directive_note("person/avery", "person/avery", Some("Prioritize the release"), None).unwrap().unwrap();
    assert_eq!(first.person, "person/avery");
    assert_eq!(first.author, "person/avery");
    let record = store.claim_by_id(&first.revision).unwrap().unwrap();
    assert_eq!(chrono::DateTime::parse_from_rfc3339(&first.time).unwrap().timestamp_millis() as u128, record.accepted_at_unix_ms);
    assert_eq!(store.directive_notes("person/avery").unwrap(), vec![first.clone()]);
    let replacement = store.set_directive_note("person/avery", "person/avery", Some(&"é".repeat(2048)), Some("2099-01-01T00:00:00+00:00")).unwrap().unwrap();
    assert_ne!(replacement.revision, first.revision);
    assert_eq!(replacement.text.len(), 4096);
    for text in [" \n\t".to_owned(), "é".repeat(2049)] {
        assert!(store.set_directive_note("person/avery", "person/avery", Some(&text), None).is_err());
    }
    for at in ["2099-02-30T00:00:00Z", "2099-01-01T00:00:00+01:00"] {
        assert!(store.set_directive_note("person/avery", "person/avery", Some("Context"), Some(at)).is_err());
    }
    assert_eq!(store.directive_notes("person/avery").unwrap(), vec![replacement]);
    assert!(store.set_directive_note("person/avery", "person/avery", None, None).unwrap().is_none());
    assert!(store.directive_notes("person/avery").unwrap().is_empty());
    let expiring = store.set_directive_note("person/avery", "person/avery", Some("Temporary focus"), Some("2099-01-01T00:00:00.500Z")).unwrap().unwrap();
    let expiry = st3_schema::directive_notes::expiry(expiring.expires_at.as_deref().unwrap()).unwrap().timestamp_millis() as u128;
    assert!(current(&store.readers.get(), "person/avery", expiry - 1).unwrap().is_some());
    assert!(current(&store.readers.get(), "person/avery", expiry).unwrap().is_none());
    assert!(current(&store.readers.get(), "person/avery", u128::MAX).unwrap().is_none());
    let precise = store.set_directive_note("person/avery", "person/avery", Some("Short focus"), Some("2099-01-01T00:00:00.0005Z")).unwrap().unwrap();
    let floor = st3_schema::directive_notes::expiry(precise.expires_at.as_deref().unwrap()).unwrap().timestamp_millis() as u128;
    assert!(current(&store.readers.get(), "person/avery", floor).unwrap().is_some());
    assert!(current(&store.readers.get(), "person/avery", floor + 1).unwrap().is_none());
    assert!(store.set_directive_note("person/avery", "person/avery", Some("Already expired"), Some("2000-01-01T00:00:00Z")).unwrap().is_none());
    assert!(store.directive_notes("person/avery").unwrap().is_empty());
    let rows: usize = store.readers.get().query_row("SELECT COUNT(*) FROM person_directive_notes WHERE person='person/avery'", [], |row| row.get(0)).unwrap();
    assert_eq!(rows, 1, "clear and expired revisions must remain current tombstones");
    store.replay_replication_graph().unwrap();
    assert!(store.directive_notes("person/avery").unwrap().is_empty());
}

#[test]
fn directive_note_local_and_replicated_admission_require_subject_person() {
    let store = store();
    for actor in ["person/robin", "agent/worker"] {
        for text in [Some("Approved"), None] {
            assert_eq!(store.set_directive_note("person/avery", actor, text, None).unwrap_err().code, "directive-note-forbidden");
        }
    }
    let note = store.set_directive_note("person/avery", "person/avery", Some("Approved"), None).unwrap().unwrap();
    let original = store.claim_by_id(&note.revision).unwrap().unwrap();
    for actor in [None, Some("person/robin"), Some("agent/worker")] {
        let mut record = original.clone();
        record.actor = actor.map(str::to_owned);
        assert_eq!(classify_replicated_claim_with_registry(&record, st3_schema::registry()).err().unwrap().code, "directive-note-forbidden");
    }
    for text in [json!(""), json!("é".repeat(2049)), json!(42)] {
        let mut record = original.clone();
        record.body["fields"]["text"] = text;
        assert_eq!(classify_replicated_claim_with_registry(&record, st3_schema::registry()).err().unwrap().code, "invalid-replicated-claim");
    }
    let mut record = original.clone();
    record.body["fields"]["expires_at"] = json!("2099-01-01T00:00:00+01:00");
    assert_eq!(classify_replicated_claim_with_registry(&record, st3_schema::registry()).err().unwrap().code, "invalid-replicated-claim");
    assert!(matches!(classify_replicated_claim_with_registry(&original, st3_schema::registry()).unwrap(), ReplicatedClaimAdmission::Valid));
    let mut legacy = st3_schema::registry().clone();
    legacy.claims.remove(KIND);
    assert!(matches!(classify_replicated_claim_with_registry(&original, &legacy).unwrap(), ReplicatedClaimAdmission::UnknownKind));
}

#[test]
fn directive_note_visibility_unions_account_owner_author_and_mission_requester() {
    let store = store();
    for person in ["person/owner", "person/avery", "person/requester", "person/intruder"] {
        store.set_directive_note(person, person, Some(person), None).unwrap();
    }
    let intent = declare(&store, r#"version 2
account "owner/one" { provider "anthropic"; owner "person/owner"; login "/tmp/login"; }
agent "note/parent" { workspace "/tmp"; harness "claude" { account "owner/one"; }; }
mission "notes" state="ready" {
  goal "Read only relevant person context."
  agent "worker" { workspace "/tmp"; harness "claude" { account-pool "person/owner"; }; }
  step "work" { assigned-to "agent/worker"; goal "Work."; }
}
"#, "person/avery", "visibility");
    let run = store.create_mission_run(&MissionRunRequest {
        mission: intent.missions["notes"].id.clone(), revision: None, workspace: "/tmp".into(),
        requester: Some("person/requester".into()), mode: Some("run".into()), inputs: BTreeMap::new(), idempotency_key: "notes-run".into(),
    }).unwrap();
    // Materialize the run-owned seat through the same execution parser as reconciliation.
    let source = intent.missions["notes"].declarations_kdl.as_ref().unwrap();
    let mut execution = crate::graph::parse_execution_intent(source, "alder", &run.id).unwrap();
    for desired in execution.subjects.values_mut() {
        desired.owner_generation = Some(run.generation.clone());
    }
    let planned = store.mission(&execution, IntentInput { kdl: source.clone(), source_name: None }).unwrap();
    store.apply_as(&execution, &planned.subject_tokens, "materialize-worker", Some(&run.requester)).unwrap();
    let worker = format!("agent/{}/worker", run.id);
    let people = |actor: &str| store.directive_notes(actor).unwrap().into_iter().map(|note| note.person).collect::<BTreeSet<_>>();
    assert_eq!(people("agent/note/parent"), BTreeSet::from(["person/avery".into(), "person/owner".into()]));
    assert_eq!(people(&worker), BTreeSet::from(["person/owner".into(), "person/requester".into()]));
    // The spawned declaration's author may itself be an agent owned by a different person.
    let child_kdl = "version 2\nagent \"note/child\" { workspace \"/tmp\"; command \"true\"; }";
    let mut child = crate::graph::parse_internal_intent(child_kdl, "alder").unwrap();
    let desired = child.subjects.get_mut("agent/note/child").unwrap();
    desired.owner_run = Some(run.subject.clone());
    desired.owner_generation = Some(run.generation.clone());
    let planned = store.mission(&child, IntentInput { kdl: child_kdl.into(), source_name: None }).unwrap();
    store.apply_as(&child, &planned.subject_tokens, "child-visibility", Some("agent/note/parent")).unwrap();
    assert_eq!(people("agent/note/child"), BTreeSet::from(["person/avery".into(), "person/owner".into(), "person/requester".into()]));
    assert_eq!(people("person/intruder"), BTreeSet::from(["person/intruder".into()]));
    assert!(people("agent/unknown").is_empty());
    assert!(people("host/alder").is_empty());
}

#[test]
fn directive_note_visibility_fails_closed_on_unknown_cycles_and_depth_limit() {
    let store = store();
    store.set_directive_note("person/avery", "person/avery", Some("Context"), None).unwrap();
    declare(&store, "version 2\nagent \"note/cycle-a\" { workspace \"/tmp\"; command \"true\"; }", "agent/note/cycle-b", "cycle-a");
    declare(&store, "version 2\nagent \"note/cycle-b\" { workspace \"/tmp\"; command \"true\"; }", "agent/note/cycle-a", "cycle-b");
    declare(&store, "version 2\nagent \"note/missing\" { workspace \"/tmp\"; command \"true\"; }", "agent/note/absent", "missing");
    for actor in ["agent/note/cycle-a", "agent/note/cycle-b", "agent/note/missing"] {
        assert!(store.directive_notes(actor).unwrap().is_empty());
    }
    for index in (0..17).rev() {
        let author = if index == 16 { "person/avery".into() } else { format!("agent/note/hop-{}", index + 1) };
        declare(&store, &format!("version 2\nagent \"note/hop-{index}\" {{ workspace \"/tmp\"; command \"true\"; }}"), &author, &format!("hop-{index}"));
    }
    assert!(store.directive_notes("agent/note/hop-0").unwrap().is_empty());
    assert_eq!(store.directive_notes("agent/note/hop-1").unwrap().len(), 1);
}

#[test]
fn directive_note_feature_barrier_covers_generic_append_active_peers_and_legacy_hole() {
    let store = Store::open_memory("alder").unwrap();
    let input = ClaimInput { subject: "person/avery".into(), kind: KIND.into(), actor: Some("person/avery".into()),
        fields: BTreeMap::from([("text".into(), json!("Context"))]), evidence: Vec::new(), expected_subject: None, idempotency_key: None };
    assert_eq!(store.append_client_claim(&input).unwrap_err().code, "directive-note-feature-barrier");
    let anchor = enable(&store);
    advertise(&store, false);
    assert_eq!(store.append_claim(&input).unwrap_err().code, "directive-note-feature-barrier");
    advertise(&store, true);
    let peer_key = Arc::new(MemberKey::generate().unwrap().0);
    claim(&store, "host/birch", "fleet.member-admitted", None, json!({"fleet_id":FLEET,"member_key":peer_key.public(),"via":"invite","sponsor":"host/alder","mode":"listening"}));
    assert_eq!(store.append_client_claim(&input).unwrap_err().code, "directive-note-feature-barrier");
    store.replication_snapshot().unwrap();
    assert_eq!(store.append_client_claim(&input).unwrap_err().code, "directive-note-feature-barrier");
    // Another origin cannot advertise on a missing peer's behalf.
    claim(&store, "daemon/birch", "daemon.started", None, json!({"status":"running","features":{"person_directive_note":1}}));
    assert_eq!(store.append_client_claim(&input).unwrap_err().code, "directive-note-feature-barrier");
    let peer = Store::open_memory("birch").unwrap();
    peer.bind_fleet(FLEET).unwrap();
    peer.pin_fleet_anchor(anchor.public()).unwrap();
    peer.set_member_key(Some(peer_key.clone())).unwrap();
    sync(&store, &peer);
    advertise(&peer, true);
    sync(&peer, &store);
    store.append_client_claim(&input).unwrap();
    advertise(&peer, false);
    sync(&peer, &store);
    assert_eq!(store.set_directive_note("person/avery", "person/avery", None, None).unwrap_err().code, "directive-note-feature-barrier");
    let high_water = writer_high_water(&peer.readers.get(), "birch").unwrap().unwrap();
    claim(&store, "host/birch", "fleet.member-removed", None, json!({"member_key":peer_key.public(),"high_water":high_water,"reason":"Unsupported peer fenced"}));
    store.replication_snapshot().unwrap();
    store.append_client_claim(&input).unwrap();
    // Membership advertisements cannot account for a legacy writer outside the membership fold.
    let legacy = Store::open_memory("legacy").unwrap();
    legacy.bind_fleet(FLEET).unwrap();
    advertise(&legacy, true);
    sync(&legacy, &store);
    let refusal = store.append_client_claim(&input).unwrap_err();
    assert_eq!(refusal.code, "directive-note-feature-barrier");
    assert!(refusal.message.contains("legacy") && refusal.message.contains("unfenced"));
    let legacy_high_water = writer_high_water(&legacy.readers.get(), "legacy").unwrap().unwrap();
    claim(&store, "host/legacy", "fleet.member-removed", None, json!({"high_water":legacy_high_water,"reason":"Legacy replication fenced"}));
    store.replication_snapshot().unwrap();
    store.append_client_claim(&input).unwrap();
    assert!(matches!(store.fleet_membership().unwrap().window("legacy", legacy_high_water + 1), crate::fleet::Window::Fenced));
}

#[test]
fn directive_note_replication_out_of_order_clear_replay_and_reopen_agree() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.db");
    let source = Store::open_memory("alder").unwrap();
    let anchor = enable(&source);
    let peer_key = Arc::new(MemberKey::generate().unwrap().0);
    claim(&source, "host/birch", "fleet.member-admitted", None, json!({"fleet_id":FLEET,"member_key":peer_key.public(),"via":"invite","sponsor":"host/alder","mode":"listening"}));
    let peer = Store::open_memory("birch").unwrap();
    peer.bind_fleet(FLEET).unwrap();
    peer.pin_fleet_anchor(anchor.public()).unwrap();
    peer.set_member_key(Some(peer_key)).unwrap();
    sync(&source, &peer);
    advertise(&peer, true);
    sync(&peer, &source);
    let set = source.set_directive_note("person/avery", "person/avery", Some("Old context"), None).unwrap().unwrap();
    peer.set_directive_note("person/avery", "person/avery", None, None).unwrap();
    let target = Store::open(&path, "cedar").unwrap();
    target.bind_fleet(FLEET).unwrap();
    target.pin_fleet_anchor(anchor.public()).unwrap();
    sync(&peer, &target); // Clear arrives before the older set.
    assert!(target.directive_notes("person/avery").unwrap().is_empty());
    sync(&source, &target);
    assert!(target.claim_by_id(&set.revision).unwrap().is_some());
    assert!(target.directive_notes("person/avery").unwrap().is_empty());
    let expected = target.readers.get().query_row("SELECT claim_id,head_key FROM person_directive_notes WHERE person='person/avery'", [], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))).unwrap();
    target.replay_replication_graph().unwrap();
    assert_eq!(target.readers.get().query_row("SELECT claim_id,head_key FROM person_directive_notes WHERE person='person/avery'", [], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))).unwrap(), expected);
    sync(&peer, &source);
    let replacement = source.set_directive_note("person/avery", "person/avery", Some("New context"), None).unwrap().unwrap();
    sync(&source, &target);
    assert_eq!(target.directive_notes("person/avery").unwrap(), vec![replacement.clone()]);
    drop(target);
    let reopened = Store::open(&path, "cedar").unwrap();
    assert_eq!(reopened.directive_notes("person/avery").unwrap(), vec![replacement]);
    reopened.rebuild_claim_projections().unwrap();
    assert_eq!(reopened.directive_notes("person/avery").unwrap()[0].text, "New context");
}

#[test]
fn directive_note_projection_rollback_and_canonical_metadata_corrections() {
    let store = store();
    let first = store.set_directive_note("person/avery", "person/avery", Some("First"), None).unwrap().unwrap();
    {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        append_claim_tx(&tx, "alder", "person/avery", KIND, Some("person/avery"), &json!({"fields":{"text":"Rolled back"}}), &[], None).unwrap();
        assert_eq!(current(&tx, "person/avery", now_ms()).unwrap().unwrap().text, "Rolled back");
    }
    assert_eq!(store.directive_notes("person/avery").unwrap(), vec![first.clone()]);
    let second = store.set_directive_note("person/avery", "person/avery", Some("Second"), None).unwrap().unwrap();
    store.connection.batched(|tx| -> Result<()> {
        tx.execute("UPDATE claims SET accepted_at_unix_ms=accepted_at_unix_ms+100000 WHERE id=?1", [&first.revision])?;
        flush(tx)?;
        Ok(())
    }).unwrap().unwrap();
    assert_eq!(store.directive_notes("person/avery").unwrap()[0].revision, first.revision);
    store.connection.batched(|tx| -> Result<()> {
        tx.execute("DELETE FROM claims WHERE id=?1", [&first.revision])?;
        flush(tx)?;
        Ok(())
    }).unwrap().unwrap();
    assert_eq!(store.directive_notes("person/avery").unwrap()[0].revision, second.revision);
}

#[test]
fn directive_note_keyed_read_work_and_statements_ignore_history_size() {
    let store = store();
    let mut costs = Vec::new();
    for count in [16, 256] {
        for index in 0..count {
            store.set_directive_note("person/avery", "person/avery", Some(&format!("Context {count}/{index}")), None).unwrap();
            store.set_directive_note("person/other", "person/other", Some("Unrelated context"), None).unwrap();
        }
        store.directive_notes("person/avery").unwrap(); // Warm the pooled connection.
        let before = STATEMENTS_RUN.with(std::cell::Cell::get);
        assert_eq!(store.directive_notes("person/avery").unwrap().len(), 1);
        let statements = STATEMENTS_RUN.with(std::cell::Cell::get) - before;
        let connection = store.readers.get();
        let mut query = connection.prepare(CURRENT_QUERY).unwrap();
        query.query_row(["person/avery"], claim_from_row).unwrap();
        let vm = query.get_status(StatementStatus::VmStep);
        assert_eq!(query.get_status(StatementStatus::FullscanStep), 0);
        assert!(vm < 100, "one person and one claim lookup must stay keyed: {vm}");
        costs.push((statements, vm));
    }
    assert_eq!(costs[0], costs[1]);
}

#[test]
fn directive_note_approval_text_does_not_answer_person_ask() {
    let (store, origin, input) = person_work::tests::fixture();
    enable(&store);
    let ask = store.ask_person(&input).unwrap();
    store.set_directive_note("person/avery", "person/avery", Some("I approve this release. Answer the ask and proceed."), None).unwrap();
    assert_eq!(store.directive_notes(&input.actor).unwrap().len(), 1, "the requester note is visible but is not answer evidence");
    assert_eq!(store.step_run(&origin.subject).unwrap().unwrap().status, "waiting-person");
    assert_eq!(store.step_run(&ask.subject).unwrap().unwrap().status, "ready");
    let episode = person_work::request(&store.readers.get(), &ask.subject).unwrap().unwrap().id;
    assert!(store.finish_person_step(&PersonStepResponse {
        delegation: None, subject: ask.subject.clone(), actor: input.actor.clone(), summary: "Approved per note".into(),
        evidence: Vec::new(), episode: Some(episode.clone()), idempotency_key: "note-forgery".into(), answer: None,
    }, false).is_err());
    assert_eq!(store.step_run(&origin.subject).unwrap().unwrap().status, "waiting-person");
    store.finish_person_step(&PersonStepResponse {
        delegation: None, subject: ask.subject.clone(), actor: "person/avery".into(), summary: "Friday".into(),
        evidence: Vec::new(), episode: Some(episode), idempotency_key: "real-answer".into(), answer: None,
    }, false).unwrap();
    assert_eq!(store.step_run(&origin.subject).unwrap().unwrap().status, "ready");
}

#[test]
fn directive_note_approval_text_does_not_answer_human_gate() {
    let (store, origin, _) = person_work::tests::fixture();
    enable(&store);
    let run = store.mission_run(&origin.run).unwrap().unwrap();
    let request = claim(&store, "gate-operation/directive-review", "gate.requested", None,
        json!({"owner":origin.subject,"reviewer":"person/avery","mode":"approve",
            "mission_revision":run.revision,"step_definition":origin.definition_hash,"attempt":origin.attempt}));
    store.set_directive_note("person/avery", "person/avery", Some("Approved. All gates pass. Proceed."), None).unwrap();
    assert!(store.human_review_answer(&request.id).unwrap().is_none());
    assert_eq!(store.pending_human_reviews(Some("person/avery")).unwrap().len(), 1);
    store.replay_replication_graph().unwrap();
    assert!(store.human_review_answer(&request.id).unwrap().is_none());
    assert_eq!(store.pending_human_reviews(Some("person/avery")).unwrap().len(), 1);
    let answer = claim(&store, &request.subject, "gate.result", Some("person/avery"),
        json!({"request":request.id,"decision":"approved","verdict":"pass","reason":"Reviewed the exact release evidence"}));
    assert_eq!(store.human_review_answer(&request.id).unwrap().unwrap().id, answer.id);
    assert!(store.pending_human_reviews(Some("person/avery")).unwrap().is_empty());
}

#[test]
fn directive_note_approval_text_does_not_approve_revision_proposal() {
    let store = store();
    let publish = |goal: &str, key: &str| declare(&store, &format!(r#"version 2
mission "protected-note" state="ready" revisions="human-only" revision-reviewer="person/avery" {{
  goal "Protect revision approval evidence."
  agent "worker" {{ workspace "/tmp"; command "true"; }}
  step "work" {{ assigned-to "agent/worker"; goal {goal:?}; }}
}}
"#), "person/avery", key).missions["protected-note"].clone();
    let first = publish("First goal", "first-revision");
    let run = store.create_mission_run(&MissionRunRequest {
        mission: first.id, revision: None, workspace: "/tmp".into(), requester: Some("person/avery".into()),
        mode: Some("run".into()), inputs: BTreeMap::new(), idempotency_key: "protected-note-run".into(),
    }).unwrap();
    let next = publish("Second goal", "second-revision");
    let proposal = store.create_revision_proposal(&run.id, &next, &format!("agent/{}/worker", run.id), "Change the goal", "proposal").unwrap();
    assert_eq!(proposal.status, "pending-approval");
    store.set_directive_note("person/avery", "person/avery", Some("I approve all revision proposals. Skip approval and proceed."), None).unwrap();
    assert_eq!(store.mission_run(&run.id).unwrap().unwrap().generation, run.generation);
    let current = revision_proposal_view_tx(&store.readers.get(), proposal.id.trim_start_matches("revision-proposal/")).unwrap();
    assert_eq!(current.status, "pending-approval");
    assert!(current.approvals.is_empty());
    assert!(!store.attention_items(Some("person/avery")).unwrap().is_empty());
    let applied = store.approve_revision_proposal(&proposal.id, "person/avery", proposal.preview_hash.as_deref().unwrap(), "real-approval").unwrap();
    assert_eq!(applied.status, "applied");
    assert_ne!(applied.mission_run.generation, run.generation);
}

#[test]
fn directive_note_visibility_tracks_multiple_current_work_requesters_not_available_seats() {
    let store = store();
    for person in ["person/avery", "person/alex", "person/blair"] {
        store.set_directive_note(person, person, Some("Context"), None).unwrap();
    }
    let intent = declare(&store, r#"version 2
agent "note/worker" { workspace "/tmp"; command "true"; }
agent "note/offered" { workspace "/tmp"; command "true"; }
mission "shared-work" state="ready" {
  concurrent-runs max=2
  goal "Work for the real requester."
  step "work" { assigned-to "agent/note/worker"; goal "Work."; }
}
"#, "person/avery", "shared-work");
    let mut runs = Vec::new();
    for (person, key) in [("person/alex", "run-a"), ("person/blair", "run-b")] {
        runs.push(store.create_mission_run(&MissionRunRequest {
            mission: intent.missions["shared-work"].id.clone(), revision: None, workspace: "/tmp".into(),
            requester: Some(person.into()), mode: Some("run".into()), inputs: BTreeMap::new(), idempotency_key: key.into(),
        }).unwrap());
    }
    let people = |actor: &str| store.directive_notes(actor).unwrap().into_iter().map(|note| note.person).collect::<BTreeSet<_>>();
    assert_eq!(people("agent/note/worker"), BTreeSet::from(["person/avery".into(), "person/alex".into(), "person/blair".into()]));
    store.connection.batched(|tx| -> Result<()> {
        tx.execute("UPDATE step_runs SET available_to='[\"agent/note/offered\"]' WHERE subject=?1", [&runs[0].steps[0].subject])?;
        Ok(())
    }).unwrap().unwrap();
    assert_eq!(people("agent/note/offered"), BTreeSet::from(["person/avery".into()]), "an offer is not a work-for relationship");
    for run in runs {
        store.set_mission_run_state(&run.id, "cancelled", "normal", None).unwrap();
    }
    assert_eq!(people("agent/note/worker"), BTreeSet::from(["person/avery".into()]));
}

#[test]
fn directive_note_visibility_relation_overflow_fails_closed() {
    let store = store();
    store.set_directive_note("person/avery", "person/avery", Some("Context"), None).unwrap();
    let intent = declare(&store, r#"version 2
agent "note/worker" { workspace "/tmp"; command "true"; }
mission "many-requesters" state="ready" {
  concurrent-runs max=65
  goal "Bound person context relations."
  step "work" { assigned-to "agent/note/worker"; goal "Work."; }
}
"#, "person/avery", "many-requesters");
    for index in 0..65 {
        store.create_mission_run(&MissionRunRequest {
            mission: intent.missions["many-requesters"].id.clone(), revision: None, workspace: "/tmp".into(),
            requester: Some("person/avery".into()), mode: Some("run".into()), inputs: BTreeMap::new(), idempotency_key: format!("run-{index}"),
        }).unwrap();
    }
    assert!(store.directive_notes("agent/note/worker").is_err(), "oversized ownership/work sets must fail explicitly, not look like no current notes");
}

#[test]
fn directive_note_signed_wire_admission_rejects_wrong_writer_bounds_and_expiry() {
    for mutation in ["wrong-person", "agent", "missing-actor", "blank", "oversized", "non-utc"] {
        let source = Store::open_memory("alder").unwrap();
        let key = enable(&source);
        let target = Store::open_memory("birch").unwrap();
        target.bind_fleet(FLEET).unwrap();
        target.pin_fleet_anchor(key.public()).unwrap();
        sync(&source, &target);
        source.set_directive_note("person/avery", "person/avery", Some("Context"), None).unwrap();
        let mut exchange = source.export_replication_exchange(FLEET, &target.replication_inventory().unwrap()).unwrap();
        exchange.envelopes.retain(|envelope| {
            let payload: ReplicaEnvelopePayload = ciborium::from_reader(envelope.payload.bytes().unwrap()).unwrap();
            payload.batch.claims.iter().any(|claim| claim.kind == KIND)
        });
        assert_eq!(exchange.envelopes.len(), 1);
        let envelope = &mut exchange.envelopes[0];
        let mut payload: ReplicaEnvelopePayload = ciborium::from_reader(envelope.payload.bytes().unwrap()).unwrap();
        let record = &mut payload.batch.claims[0];
        match mutation {
            "wrong-person" => record.actor = Some("person/robin".into()),
            "agent" => record.actor = Some("agent/worker".into()),
            "missing-actor" => record.actor = None,
            "blank" => record.body["fields"]["text"] = json!(" \n\t"),
            "oversized" => record.body["fields"]["text"] = json!("é".repeat(2049)),
            "non-utc" => record.body["fields"]["expires_at"] = json!("2099-01-01T00:00:00+01:00"),
            _ => unreachable!(),
        }
        record.id = claim_hash(&record.batch_id, &record.subject, &record.kind, &record.origin,
            record.actor.as_deref(), &record.body, &record.predecessors).unwrap();
        let id = record.id.clone();
        let mut bytes = Vec::new();
        ciborium::into_writer(&payload, &mut bytes).unwrap();
        envelope.hash = replica_envelope_hash(&envelope.writer, envelope.sequence,
            envelope.previous_hash.as_deref(), envelope.accepted_at_unix_ms, &bytes);
        envelope.payload = bytes.into();
        envelope.member_key = Some(key.public().into());
        envelope.signature = Some(key.sign(&crate::fleet::envelope_signature_message(
            FLEET, &envelope.writer, envelope.sequence, &envelope.hash)));
        exchange.inventory = ReplicationInventory {
            envelopes: vec![ReplicaEnvelopeId { writer: envelope.writer.clone(), sequence: envelope.sequence, hash: envelope.hash.clone() }],
            ..ReplicationInventory::default()
        };
        target.receive_replication_exchange("alder", FLEET, &exchange).unwrap();
        let admission = target.validate_replication_backlog().unwrap();
        assert_eq!(admission.invalid, 1, "{mutation}: a validly signed, rehashed record must fail semantic admission");
        assert_eq!(admission.valid, 0);
        assert!(target.project_replication_backlog().unwrap());
        assert!(target.claim_by_id(&id).unwrap().is_none());
        assert!(target.directive_notes("person/avery").unwrap().is_empty());
    }
}
