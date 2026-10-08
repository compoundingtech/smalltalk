//! Fold-level assertions adapted from Axe's decision_tree.rs; no verbs or storage.
use st_decision_fold::*;

fn request(id: &str, q: u64, guard: &[(&str, &str)]) -> Request {
    Request {
        id: id.into(), written_ms: 1000, q, kind: Kind::Blocker,
        subject: "Shard?".into(), asked_by: "agent".into(), about: None, parent: None,
        applies_when: guard.iter().map(|(decision, option)| Term {
            decision: (*decision).into(), option: (*option).into(),
        }).collect(),
        body: "## Options\n\n### yes\nMaterial.\n\n**Implications:** x.\n**Reversibility:** cheap.\n**Grounded by:** a prior result.\n\n### no\nOther material.\n\n**Implications:** y.\n**Reversibility:** cheap.\n**Not grounded:** nothing measured.\n".into(),
    }
}
fn answer(id: &str, choices: &[&str], supersedes: Option<&str>, body: &str) -> Answer {
    Answer { id: id.into(), written_ms: 1001, answers: "root01".into(),
        answered_by: "johannes".into(), provenance: AnswerProvenance::Native,
        choice: choices.iter().map(|s| (*s).into()).collect(),
        capture_key: None, supersedes: supersedes.map(str::to_string), body: body.into() }
}
fn store() -> Store {
    Store { requests: vec![request("root01", 1, &[])], ..Store::default() }
}

// Source: a_decision_is_asked_guarded_answered_superseded_and_revived.
#[test]
fn a_decision_is_guarded_answered_superseded_and_revived() {
    let mut store = store();
    store.requests.push(request("child1", 2, &[("root01", "yes")]));
    assert_eq!(fold(&store).resolution("child1"), Resolution::State(State::Gated));
    store.answers.push(answer("first1", &["no"], None, "Flat for now."));
    assert_eq!(fold(&store).resolution("child1"), Resolution::State(State::Moot));
    store.answers.push(answer("second", &["yes"], Some("first1"), "Changed my mind."));
    assert_eq!(current_answer(&store, "root01").unwrap().choice, ["yes"]);
    let computed = fold(&store);
    assert_eq!(computed.resolution("child1"), Resolution::State(State::Pending));
    assert!(computed.revived.contains("child1"));
    store.answers.push(answer("third1", &[], Some("second"), "REFRAME: retention is the real question."));
    let computed = fold(&store);
    assert_eq!(computed.resolution("child1"), Resolution::Undecidable);
    assert!(computed.defects.iter().any(|d| d.code == DefectCode::AnswerSelectsNoOption));
}

// Source: two_agents_proceeding_past_one_decision_both_leave_a_record.
#[test]
fn two_agents_proceeding_past_one_decision_both_leave_a_record() {
    let mut store = store();
    store.requests[0].kind = Kind::Refinement;
    store.assumptions = vec![
        Assumption { id: "first1".into(), written_ms: 1001, assumes: "root01".into(), assumed_by: "agent".into(), supersedes: None, body: "Assumed `--seat`; shipped the reader.".into() },
        Assumption { id: "second".into(), written_ms: 1001, assumes: "root01".into(), assumed_by: "agent".into(), supersedes: Some("first1".into()), body: "Successor assumed `--as` instead.".into() },
    ];
    let assumptions = assumptions_for(&store, "root01");
    assert_eq!(assumptions.len(), 2, "neither assumption overwrote the other");
    assert!(assumptions[0].body.contains("--seat"));
    assert!(assumptions[1].body.contains("--as"));
    assert_eq!(assumptions[1].supersedes.as_deref(), Some(assumptions[0].id.as_str()));
    assert_eq!(fold(&store).resolution("root01"), Resolution::State(State::Pending));
}

// Source: read_shows_the_whole_history_including_the_answer_that_was_superseded.
#[test]
fn history_includes_the_answer_that_was_superseded() {
    let mut store = store();
    store.answers = vec![answer("first1", &["no"], None, "Flat for now."), answer("second", &["yes"], Some("first1"), "Changed my mind.")];
    let answers = answer_history(&store, "root01");
    assert_eq!(answers.len(), 2);
    assert_eq!(answers[0].choice, ["no"]);
    assert_eq!(answers[0].body, "Flat for now.");
    assert_eq!(answers[1].choice, ["yes"]);
    assert_eq!(answers[1].body, "Changed my mind.");
    let options = parse_options(&store.requests[0].body);
    assert_eq!((&*options[0].id, &*options[0].key), ("A", "yes"));
    assert_eq!((&*options[1].id, &*options[1].key), ("B", "no"));
    assert_eq!(fold(&store).resolution("root01"), Resolution::State(State::Answered));
}

// Source: promotion_is_written_after_the_answer_and_an_unresolvable_target_is_reported.
#[test]
fn promotion_record_is_the_authority_for_the_link() {
    let mut store = store();
    store.answers.push(answer("first1", &["yes"], None, "Yes."));
    store.push(parse_record("prom01", 1002, "---\nrecord: promotion\npromotes: root01\ntarget: context/.decisions/0001-sharding.md\n---\n").unwrap());
    let promotions = promotions_for(&store, "root01");
    assert_eq!(promotions.len(), 1, "the promotion record is the single authority for the link");
    assert_eq!(promotions[0].target, "context/.decisions/0001-sharding.md");
    assert_eq!(fold(&store).resolution("root01"), Resolution::State(State::Answered));
}

// Source: captured_retries_preserve_one_record_and_conflicting_key_or_content_cannot_supersede.
#[test]
fn captured_record_preserves_choice_key_provenance_and_body() {
    let mut store = store();
    store.push(parse_record("capt01", 1001, "---\nrecord: answer\nanswers: root01\nanswered-by: johannes\nprovenance: native\nchoice: [yes, no]\ncapture-key: a0b1\n---\nHuman chose both.\n").unwrap());
    let captured = current_answer(&store, "root01").unwrap();
    assert_eq!(captured.choice, ["yes", "no"]);
    assert_eq!(captured.answered_by, "johannes");
    assert_eq!(captured.provenance, AnswerProvenance::Native);
    assert_eq!(captured.capture_key.as_deref(), Some("a0b1"));
    assert_eq!(captured.supersedes, None);
    assert_eq!(captured.body, "Human chose both.\n");
}

// Source: concurrent_captured_free_text_retries_share_one_record_and_manual_answer_still_supersedes.
#[test]
fn captured_free_text_remains_in_history_after_manual_supersession() {
    let mut store = store();
    store.push(parse_record("capt01", 1001, "---\nrecord: answer\nanswers: root01\nanswered-by: johannes\ncapture-key: deadbeef\n---\nNone of these options.\n").unwrap());
    store.answers.push(answer("manual", &["no"], Some("capt01"), "New decision."));
    assert_eq!(current_answer(&store, "root01").unwrap().supersedes.as_deref(), Some("capt01"));
    let history = answer_history(&store, "root01");
    assert_eq!(history[0].capture_key.as_deref(), Some("deadbeef"));
    assert_eq!(history[0].body, "None of these options.\n");
    assert!(history[0].choice.is_empty());
    assert_eq!(history[1].choice, ["no"]);
}

// Source: material_is_stored_byte_identical_to_what_its_author_wrote.
#[test]
fn parsed_material_is_byte_identical_to_what_its_author_wrote() {
    let hostile = "\n## Options\n\n### a\n```text\nEOF\n$(echo CANARY | rev)  `date`  $HOME  \\\n┌─────────┐\n│ layout  │\n└─────────┘\n```\n\n**Implications:** x.\n**Reversibility:** cheap.\n**Grounded by:** the record shape in spec.md.\n";
    let document = format!("---\nrecord: request\nq: 1\nkind: blocker\nsubject: Material\nasked-by: agent\n---\n{hostile}");
    let Record::Request(request) = parse_record("root01", 1000, &document).unwrap() else {
        panic!("expected request");
    };
    assert_eq!(request.body.as_bytes(), hostile.as_bytes());
}

// Sources: captured_retries_preserve_one_record_and_conflicting_key_or_content_cannot_supersede;
// concurrent_captured_free_text_retries_share_one_record_and_manual_answer_still_supersedes.
#[test]
fn imported_capture_remains_readable_until_a_native_answer_supersedes_it() {
    let mut store = store();
    store.requests.push(request("child1", 2, &[("root01", "yes")]));
    store.push(parse_record("capt01", 1001, "---\nrecord: answer\nanswers: root01\nanswered-by: johannes\nprovenance: imported\nchoice: [no]\ncapture-key: historical\n---\nOriginal decision.\n").unwrap());
    let computed = fold(&store);
    assert_eq!(computed.resolution("root01"), Resolution::State(State::Answered));
    assert_eq!(computed.resolution("child1"), Resolution::Undecidable);
    assert!(computed.defects.iter().any(|d| d.code == DefectCode::ImportedAnswer));
    assert_eq!(current_answer(&store, "root01").unwrap().capture_key.as_deref(), Some("historical"));
    store.answers.push(answer("manual", &["yes"], Some("capt01"), "Native decision."));
    let computed = fold(&store);
    assert_eq!(computed.resolution("child1"), Resolution::State(State::Pending));
    assert!(!computed.revived.contains("child1"));
    let history = answer_history(&store, "root01");
    assert_eq!(history.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["capt01", "manual"]);
    assert_eq!(history[0].provenance, AnswerProvenance::Imported);
    assert_eq!(history[0].body, "Original decision.\n");
    assert_eq!(history[1].provenance, AnswerProvenance::Native);
}
