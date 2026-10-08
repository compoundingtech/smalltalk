use super::*;
use crate::parsing::*;

/// Builds a request body with the given option keys, well-formed enough to pass validation.
fn body_with(options: &[&str]) -> String {
    let mut body = String::from("\n## Context\nWhy.\n\n## Options\n");
    for key in options {
        body.push_str(&format!(
            "\n### {key} — {key}\nA sketch of {key}.\n\n\
             **Implications:** choosing {key} has consequences.\n\
             **Reversibility:** cheap.\n\
             **Grounded by:** a prior result.\n"
        ));
    }
    body
}

fn request(id: &str, q: u64, guard: &[(&str, &str)], options: &[&str]) -> Request {
    Request {
        id: id.to_string(),
        written_ms: 1,
        q,
        kind: Kind::Blocker,
        subject: format!("subject {id}"),
        asked_by: "dev3.test".to_string(),
        about: None,
        parent: None,
        applies_when: guard
            .iter()
            .map(|(decision, option)| Term {
                decision: decision.to_string(),
                option: option.to_string(),
            })
            .collect(),
        body: body_with(options),
    }
}

fn answer(id: &str, answers: &str, choice: &[&str], supersedes: Option<&str>) -> Answer {
    Answer {
        id: id.to_string(),
        written_ms: 1,
        answers: answers.to_string(),
        answered_by: "johannes".to_string(),
        provenance: AnswerProvenance::Native,
        choice: choice.iter().map(|c| c.to_string()).collect(),
        capture_key: None,
        supersedes: supersedes.map(str::to_string),
        body: String::new(),
    }
}

fn store_of(requests: Vec<Request>, answers: Vec<Answer>) -> Store {
    Store {
        requests,
        answers,
        ..Store::default()
    }
}

/// Every input ordering, including the single ordering of an empty input.
fn permutations(len: usize) -> Vec<Vec<usize>> {
    fn visit(prefix: &mut Vec<usize>, len: usize, result: &mut Vec<Vec<usize>>) {
        if prefix.len() == len {
            result.push(prefix.clone());
            return;
        }
        for index in 0..len {
            if !prefix.contains(&index) {
                prefix.push(index);
                visit(prefix, len, result);
                prefix.pop();
            }
        }
    }
    let mut result = Vec::new();
    visit(&mut Vec::new(), len, &mut result);
    result
}

fn codes(fold: &Fold) -> Vec<&'static str> {
    let mut codes: Vec<&'static str> = fold.defects.iter().map(|d| d.code.as_str()).collect();
    codes.sort_unstable();
    codes.dedup();
    codes
}

// -- frontmatter and the guard grammar ---------------------------------------------------

#[test]
fn a_bare_guard_scalar_and_a_one_term_list_are_the_same_guard() {
    let bare = parse_flow_sequence("aaaaaa = yes").expect("bare scalar parses");
    let listed = parse_flow_sequence("[aaaaaa = yes]").expect("one-item list parses");
    assert_eq!(bare, listed);
    assert_eq!(
        parse_term(&bare[0]).expect("term parses"),
        Term {
            decision: "aaaaaa".to_string(),
            option: "yes".to_string()
        }
    );
}

#[test]
fn a_conjunction_keeps_every_term() {
    let terms = parse_flow_sequence("[aaaaaa = yes, bbbbbb = no]").expect("list parses");
    assert_eq!(terms.len(), 2);
    assert_eq!(parse_term(&terms[1]).expect("term parses").option, "no");
}

#[test]
fn frontmatter_rejects_shapes_it_would_otherwise_misread() {
    assert!(split_document("no fence here\n").is_err());
    assert!(split_document("---\nrecord: request\n").is_err());
    assert!(parse_frontmatter("record: request\nrecord: answer\n").is_err());
    assert!(parse_frontmatter("record: request\n  continued: true\n").is_err());
}

#[test]
fn a_handle_is_distinguishable_from_a_record_id() {
    assert!(looks_like_handle("q18"));
    assert!(looks_like_handle("Q18"));
    assert!(!looks_like_handle("a1q018"));
    assert!(!looks_like_handle("q"));
    assert!(!looks_like_handle("qx1"));
}

// -- supportive material -----------------------------------------------------------------

#[test]
fn validation_reports_every_missing_element_by_option() {
    let body = "\n## Options\n\n### a\nMaterial.\n\n**Implications:** something.\n\
                \n### b — labelled\n**Implications:** no material above.\n\
                **Reversibility:** cheap.\n**Grounded by:** a prior result.\n";
    let problems = validate_request_body(body).expect_err("this body is not well-formed");
    assert!(problems
        .iter()
        .any(|p| p.contains("`a`") && p.contains("Reversibility")));
    assert!(problems
        .iter()
        .any(|p| p.contains("`a`") && p.contains("Grounded by")));
    assert!(problems
        .iter()
        .any(|p| p.contains("`b`") && p.contains("no supportive material")));
}

#[test]
fn material_that_is_only_a_pointer_is_rejected() {
    let with = |material: &str| {
        format!(
            "\n## Options\n\n### a\n{material}\n\n**Implications:** x.\n\
             **Reversibility:** cheap.\n**Not grounded:** nothing measured.\n"
        )
    };
    assert!(validate_request_body(&with("https://example.invalid/design")).is_err());
    assert!(validate_request_body(&with("/etc/some/absolute/path")).is_err());
    assert!(validate_request_body(&with("[the design](./spec.md)")).is_err());
    assert!(validate_request_body(&with("A real sketch, see [spec](./spec.md).")).is_ok());
}

#[test]
fn a_hard_wrapped_element_does_not_read_as_material() {
    let options = parse_options(
        "## Options\n\n### a\n\n**Implications:** a consequence that\nwraps onto a second line.\n\
         **Reversibility:** cheap.\n**Grounded by:** a prior result.\n",
    );
    assert_eq!(options.len(), 1);
    assert_eq!(
        options[0].material, "",
        "the wrap belongs to the element, not the material"
    );
    assert_eq!(
        options[0].implications.as_deref(),
        Some("a consequence that wraps onto a second line.")
    );
}

#[test]
fn option_ids_follow_declaration_order() {
    let first = parse_options(
        "## Options\n\n### keep-flat\nFlat.\n\n**Implications:** one log.\n\
         **Reversibility:** cheap.\n**Grounded by:** current layout.\n\n\
         ### shard\nSharded.\n\n**Implications:** many logs.\n\
         **Reversibility:** expensive.\n**Not grounded:** no scale data.\n",
    );
    let reordered = parse_options(
        "## Options\n\n### shard\nSharded.\n\n**Implications:** many logs.\n\
         **Reversibility:** expensive.\n**Not grounded:** no scale data.\n\n\
         ### keep-flat\nFlat.\n\n**Implications:** one log.\n\
         **Reversibility:** cheap.\n**Grounded by:** current layout.\n",
    );

    assert_eq!(
        first
            .iter()
            .map(|option| (option.id.as_str(), option.key.as_str()))
            .collect::<Vec<_>>(),
        [("A", "keep-flat"), ("B", "shard")]
    );
    assert_eq!(
        reordered
            .iter()
            .map(|option| (option.id.as_str(), option.key.as_str()))
            .collect::<Vec<_>>(),
        [("A", "shard"), ("B", "keep-flat")],
        "the shorthand belongs to declaration order, not to the descriptive key"
    );
}

#[test]
fn option_ids_extend_after_z() {
    assert_eq!(option_id(0), "A");
    assert_eq!(option_id(25), "Z");
    assert_eq!(option_id(26), "AA");
    assert_eq!(option_id(27), "AB");
}

// -- the fold ----------------------------------------------------------------------------

#[test]
fn the_four_states_are_told_apart() {
    // r0 unanswered; r1 guards on r0 = yes; r2 guards on r0 = no; r3 has no guard, answered.
    let store = store_of(
        vec![
            request("r0", 1, &[], &["yes", "no"]),
            request("r1", 2, &[("r0", "yes")], &["a"]),
            request("r2", 3, &[("r0", "no")], &["a"]),
            request("r3", 4, &[], &["a"]),
        ],
        vec![answer("x0", "r3", &["a"], None)],
    );
    let computed = fold(&store);
    assert_eq!(computed.resolution("r0"), Resolution::State(State::Pending));
    assert_eq!(computed.resolution("r1"), Resolution::State(State::Gated));
    assert_eq!(computed.resolution("r2"), Resolution::State(State::Gated));
    assert_eq!(
        computed.resolution("r3"),
        Resolution::State(State::Answered)
    );

    // Answering r0 = yes makes one branch live and the other dead, which is the whole point
    // of DT-R14: it keys on WHICH WAY the parent was answered, not on it being answered.
    let mut answered = store.clone();
    answered.answers.push(answer("x1", "r0", &["yes"], None));
    let computed = fold(&answered);
    assert_eq!(computed.resolution("r1"), Resolution::State(State::Pending));
    assert_eq!(computed.resolution("r2"), Resolution::State(State::Moot));
}

#[test]
fn a_conjunction_is_gated_until_every_term_is_settled_and_moot_as_soon_as_one_fails() {
    let base = |answers: Vec<Answer>| {
        store_of(
            vec![
                request("r0", 1, &[], &["yes", "no"]),
                request("r1", 2, &[], &["yes", "no"]),
                request("r2", 3, &[("r0", "yes"), ("r1", "yes")], &["a"]),
            ],
            answers,
        )
    };
    assert_eq!(
        fold(&base(vec![answer("x", "r0", &["yes"], None)])).resolution("r2"),
        Resolution::State(State::Gated),
        "one term satisfied and one unanswered is still waiting"
    );
    assert_eq!(
        fold(&base(vec![answer("x", "r0", &["no"], None)])).resolution("r2"),
        Resolution::State(State::Moot),
        "one definitively-failed term moots the conjunction without waiting for the rest"
    );
    assert_eq!(
        fold(&base(vec![
            answer("x", "r0", &["yes"], None),
            answer("y", "r1", &["yes"], None),
        ]))
        .resolution("r2"),
        Resolution::State(State::Pending)
    );
}

#[test]
fn a_multi_select_answer_satisfies_a_term_naming_any_chosen_option() {
    let store = store_of(
        vec![
            request("r0", 1, &[], &["a", "b"]),
            request("r1", 2, &[("r0", "b")], &["x"]),
        ],
        vec![answer("x0", "r0", &["a", "b"], None)],
    );
    assert_eq!(
        fold(&store).resolution("r1"),
        Resolution::State(State::Pending)
    );
}

#[test]
fn a_decision_under_a_moot_parent_is_moot_and_under_a_gated_parent_is_gated() {
    let store = store_of(
        vec![
            request("r0", 1, &[], &["yes", "no"]),
            request("r1", 2, &[("r0", "yes")], &["a"]),
            request("r2", 3, &[("r1", "a")], &["z"]),
        ],
        vec![answer("x", "r0", &["no"], None)],
    );
    let computed = fold(&store);
    assert_eq!(computed.resolution("r1"), Resolution::State(State::Moot));
    assert_eq!(
        computed.resolution("r2"),
        Resolution::State(State::Moot),
        "a moot decision will never be answered, so a term on it is definitively false"
    );

    let gated = store_of(
        vec![
            request("r0", 1, &[], &["yes", "no"]),
            request("r1", 2, &[("r0", "yes")], &["a"]),
            request("r2", 3, &[("r1", "a")], &["z"]),
        ],
        vec![],
    );
    assert_eq!(
        fold(&gated).resolution("r2"),
        Resolution::State(State::Gated)
    );
}

#[test]
fn moot_outranks_answered_so_nothing_reads_as_live_under_a_dead_branch() {
    let store = store_of(
        vec![
            request("r0", 1, &[], &["yes", "no"]),
            request("r1", 2, &[("r0", "yes")], &["a"]),
            request("r2", 3, &[("r1", "a")], &["z"]),
        ],
        vec![
            answer("x0", "r0", &["no"], None),
            answer("x1", "r1", &["a"], None),
        ],
    );
    let fold = fold(&store);
    assert_eq!(
        fold.resolution("r1"),
        Resolution::State(State::Moot),
        "r1 was answered, but its own guard says the branch is dead"
    );
    assert_eq!(
        fold.resolution("r2"),
        Resolution::State(State::Moot),
        "r2 must not go live on r1's answer when r1 itself does not apply"
    );
    // The answer is never destroyed by the state it computes to.
    assert_eq!(
        current_answer(&store, "r1").map(|a| a.id.as_str()),
        Some("x1")
    );
}

// -- defects, which are never states ------------------------------------------------------

#[test]
fn an_invalid_guard_is_a_defect_and_never_collapses_into_gated() {
    let dangling = store_of(vec![request("r0", 1, &[("nope00", "yes")], &["a"])], vec![]);
    let fold = fold(&dangling);
    assert_eq!(fold.resolution("r0"), Resolution::Undecidable);
    assert_eq!(codes(&fold), vec!["dangling-reference"]);
    assert!(
        fold.resolution("r0").state().is_none(),
        "DT-R16: an unevaluable guard has no state at all"
    );
}

#[test]
fn a_guard_naming_a_handle_is_its_own_defect() {
    let store = store_of(
        vec![
            request("r0", 1, &[], &["yes"]),
            request("r1", 2, &[("q1", "yes")], &["a"]),
        ],
        vec![],
    );
    let fold = fold(&store);
    assert_eq!(codes(&fold), vec!["guard-references-handle"]);
    assert_eq!(fold.resolution("r1"), Resolution::Undecidable);
}

#[test]
fn a_guard_naming_an_option_the_parent_does_not_offer_is_a_defect() {
    let store = store_of(
        vec![
            request("r0", 1, &[], &["yes", "no"]),
            request("r1", 2, &[("r0", "maybe")], &["a"]),
        ],
        vec![],
    );
    assert_eq!(codes(&fold(&store)), vec!["unknown-option"]);
}

#[test]
fn a_guard_cycle_is_a_defect_rather_than_a_hang() {
    let store = store_of(
        vec![
            request("r0", 1, &[("r1", "a")], &["a"]),
            request("r1", 2, &[("r0", "a")], &["a"]),
        ],
        vec![],
    );
    let fold = fold(&store);
    assert!(codes(&fold).contains(&"cyclic-reference"));
    assert_eq!(fold.resolution("r0"), Resolution::Undecidable);
    assert_eq!(fold.resolution("r1"), Resolution::Undecidable);
}

#[test]
fn an_answer_selecting_no_option_makes_a_guard_on_it_a_defect_not_moot() {
    let store = store_of(
        vec![
            request("r0", 1, &[], &["yes", "no"]),
            request("r1", 2, &[("r0", "yes")], &["a"]),
        ],
        vec![answer("x", "r0", &[], None)],
    );
    let fold = fold(&store);
    assert_eq!(fold.resolution("r0"), Resolution::State(State::Answered));
    assert_eq!(
        fold.resolution("r1"),
        Resolution::Undecidable,
        "the reframe answered r0 without selecting an option, so r1's guard is unevaluable; \
         calling it moot would kill a branch the reframe did not kill"
    );
    assert_eq!(codes(&fold), vec!["answer-selects-no-option"]);
}

#[test]
fn two_answers_naming_one_predecessor_are_a_reported_fork() {
    let store = store_of(
        vec![request("r0", 1, &[], &["a", "b"])],
        vec![
            answer("x0", "r0", &["a"], None),
            answer("x1", "r0", &["b"], Some("x0")),
            answer("x2", "r0", &["a"], Some("x0")),
        ],
    );
    assert!(codes(&fold(&store)).contains(&"forked-supersession"));
}

#[test]
fn a_duplicate_handle_is_reported_and_never_auto_resolved() {
    let store = store_of(
        vec![
            request("r0", 24, &[], &["a"]),
            request("r1", 24, &[], &["a"]),
        ],
        vec![],
    );
    let fold = fold(&store);
    assert!(codes(&fold).contains(&"duplicate-handle"));
    // Both still resolve: a handle collision confuses a human, it never corrupts a reference.
    assert_eq!(fold.resolution("r0"), Resolution::State(State::Pending));
    assert_eq!(fold.resolution("r1"), Resolution::State(State::Pending));
}

// -- revival ------------------------------------------------------------------------------

#[test]
fn revival_is_reported_from_the_answer_history_not_the_current_answer() {
    let store = store_of(
        vec![
            request("r0", 1, &[], &["yes", "no"]),
            request("r1", 2, &[("r0", "yes")], &["a"]),
        ],
        vec![
            answer("x0", "r0", &["no"], None),
            answer("x1", "r0", &["yes"], Some("x0")),
        ],
    );
    let fold = fold(&store);
    assert_eq!(fold.resolution("r1"), Resolution::State(State::Pending));
    assert!(
        fold.revived.contains("r1"),
        "r1 is live now and was moot under the earlier answer"
    );
}

#[test]
fn a_branch_that_was_never_moot_is_not_reported_as_revived() {
    let store = store_of(
        vec![
            request("r0", 1, &[], &["yes", "no"]),
            request("r1", 2, &[("r0", "yes")], &["a"]),
        ],
        vec![
            answer("x0", "r0", &["yes"], None),
            answer("x1", "r0", &["yes"], Some("x0")),
        ],
    );
    let fold = fold(&store);
    assert_eq!(fold.resolution("r1"), Resolution::State(State::Pending));
    assert!(fold.revived.is_empty());
}

#[test]
fn supersession_order_comes_from_the_records_and_not_from_the_filename() {
    // The later answer has the SMALLER timestamp and the alphabetically-earlier id, so
    // anything ordering by either would pick the wrong current answer (DT-R06).
    let mut later = answer("aaa111", "r0", &["yes"], Some("zzz999"));
    later.written_ms = 1;
    let mut earlier = answer("zzz999", "r0", &["no"], None);
    earlier.written_ms = 9_999;
    let store = store_of(
        vec![
            request("r0", 1, &[], &["yes", "no"]),
            request("r1", 2, &[("r0", "yes")], &["a"]),
        ],
        vec![later, earlier],
    );
    assert_eq!(
        current_answer(&store, "r0").map(|a| a.id.as_str()),
        Some("aaa111")
    );
    assert_eq!(
        fold(&store).resolution("r1"),
        Resolution::State(State::Pending)
    );
}

#[test]
fn assumption_order_comes_from_supersession_even_with_equal_timestamps() {
    let assumption = |id: &str, supersedes: Option<&str>| Assumption {
        id: id.to_string(),
        written_ms: 1,
        assumes: "r0".to_string(),
        assumed_by: "dev3.test".to_string(),
        supersedes: supersedes.map(str::to_string),
        body: String::new(),
    };
    let store = Store {
        // Filename order is successor, successor, root; all writes share one millisecond.
        assumptions: vec![
            assumption("aaaaaa", Some("zzzzzz")),
            assumption("mmmmmm", Some("aaaaaa")),
            assumption("zzzzzz", None),
        ],
        ..Store::default()
    };
    let ids = assumptions_for(&store, "r0")
        .iter()
        .map(|assumption| assumption.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["zzzzzz", "aaaaaa", "mmmmmm"]);
}

// -- handles ------------------------------------------------------------------------------

#[test]
fn the_next_handle_is_one_above_the_highest_and_never_fills_a_gap() {
    let store = store_of(
        vec![
            request("r0", 1, &[], &["a"]),
            request("r1", 2, &[], &["a"]),
            request("r2", 7, &[], &["a"]),
        ],
        vec![],
    );
    assert_eq!(store.next_handle().expect("handle available"), 8);
    assert_ne!(
        store.next_handle().expect("handle available"),
        store.requests.len() as u64 + 1,
        "a count-based allocator would return 4 and reuse a handle"
    );
}

#[test]
fn a_handle_resolves_to_its_request_and_an_ambiguous_one_refuses() {
    let store = store_of(
        vec![request("r0", 3, &[], &["a"]), request("r1", 4, &[], &["a"])],
        vec![],
    );
    assert_eq!(store.resolve("q3").expect("q3 resolves").id, "r0");
    assert_eq!(store.resolve("r1").expect("id resolves").id, "r1");
    assert!(store.resolve("q9").is_err());

    let duplicated = store_of(
        vec![request("r0", 3, &[], &["a"]), request("r1", 3, &[], &["a"])],
        vec![],
    );
    assert!(
        duplicated.resolve("q3").is_err(),
        "an ambiguous handle refuses rather than picking one"
    );
}

// -- totality -----------------------------------------------------------------------------

#[test]
fn the_fold_is_total_and_deterministic_over_every_small_store() {
    // Exhaustive rather than sampled: three requests, each with one of five guard shapes
    // (including a self-cycle, a dangling reference and an unknown option) and one of three
    // answer shapes (including the reframe that selects nothing). 5^3 * 3^3 stores.
    let guard_shapes: [&[(&str, &str)]; 5] = [
        &[],
        &[("r0", "yes")],
        &[("r1", "no")],
        &[("gone00", "yes")],
        &[("r0", "not-an-option")],
    ];
    let answer_shapes: [Option<&[&str]>; 3] = [None, Some(&["yes"]), Some(&[])];
    let ids = ["r0", "r1", "r2"];

    let mut checked = 0usize;
    for g0 in 0..guard_shapes.len() {
        for g1 in 0..guard_shapes.len() {
            for g2 in 0..guard_shapes.len() {
                for a0 in 0..answer_shapes.len() {
                    for a1 in 0..answer_shapes.len() {
                        for a2 in 0..answer_shapes.len() {
                            let guards = [g0, g1, g2];
                            let picks = [a0, a1, a2];
                            let requests: Vec<Request> = ids
                                .iter()
                                .enumerate()
                                .map(|(index, id)| {
                                    // A self-guard on r2 exercises the cycle path.
                                    let guard = if index == 2 && guards[index] == 1 {
                                        &[("r2", "yes")][..]
                                    } else {
                                        guard_shapes[guards[index]]
                                    };
                                    request(id, index as u64 + 1, guard, &["yes", "no"])
                                })
                                .collect();
                            let answers: Vec<Answer> = ids
                                .iter()
                                .enumerate()
                                .filter_map(|(index, id)| {
                                    answer_shapes[picks[index]].map(|choice| {
                                        answer(&format!("x{index}"), id, choice, None)
                                    })
                                })
                                .collect();
                            let store = store_of(requests, answers);
                            let first = fold(&store);
                            for id in ids {
                                let resolution = first.resolution(id);
                                // Total: every request resolves, and to exactly one of the
                                // four states or to the explicit non-state.
                                assert!(matches!(
                                    resolution,
                                    Resolution::State(_) | Resolution::Undecidable
                                ));
                                if resolution == Resolution::Undecidable {
                                    assert!(
                                        first
                                            .defects
                                            .iter()
                                            .any(|d| d.about.as_deref() == Some(id)),
                                        "an undecidable decision always carries a defect \
                                         naming it, so a reader is never told `undecidable` \
                                         with no reason"
                                    );
                                }
                            }
                            for request_order in permutations(store.requests.len()) {
                                for answer_order in permutations(store.answers.len()) {
                                    let permuted = store_of(
                                        request_order.iter().map(|&i| store.requests[i].clone()).collect(),
                                        answer_order.iter().map(|&i| store.answers[i].clone()).collect(),
                                    );
                                    let second = fold(&permuted);
                                    assert_eq!(first.resolutions, second.resolutions);
                                    assert_eq!(first.defects, second.defects);
                                    assert_eq!(first.revived, second.revived);
                                }
                            }
                            checked += 1;
                        }
                    }
                }
            }
        }
    }
    assert_eq!(checked, 5 * 5 * 5 * 3 * 3 * 3);
}

#[test]
fn forked_roots_and_successors_never_offer_an_arbitrary_current_answer() {
    for answers in [
        vec![answer("x0", "r0", &["yes"], None), answer("x1", "r0", &["no"], None)],
        vec![
            answer("x0", "r0", &["yes"], None),
            answer("x1", "r0", &["no"], Some("x0")),
            answer("x2", "r0", &["yes"], Some("x0")),
        ],
    ] {
        for order in permutations(answers.len()) {
            let store = store_of(
                vec![request("r0", 1, &[], &["yes", "no"]),
                     request("r1", 2, &[("r0", "yes")], &["a"])],
                order.iter().map(|&i| answers[i].clone()).collect(),
            );
            let computed = fold(&store);
            assert_eq!(computed.resolution("r0"), Resolution::Undecidable);
            assert_eq!(computed.resolution("r1"), Resolution::Undecidable);
            assert!(codes(&computed).contains(&"forked-supersession"));
            assert!(current_answer(&store, "r0").is_none());
            assert!(answer_history(&store, "r0").is_empty());
        }
    }
}

#[test]
fn dangling_cyclic_and_disconnected_answer_chains_have_no_usable_history() {
    for answers in [
        vec![answer("x0", "r0", &["yes"], Some("missing"))],
        vec![answer("x0", "r0", &["yes"], Some("x1")),
             answer("x1", "r0", &["no"], Some("x0"))],
        vec![answer("x0", "r0", &["yes"], None),
             answer("x1", "r0", &["no"], Some("x2")),
             answer("x2", "r0", &["yes"], Some("x1"))],
    ] {
        for order in permutations(answers.len()) {
            let store = store_of(
                vec![request("r0", 1, &[], &["yes", "no"]),
                     request("r1", 2, &[("r0", "yes")], &["a"])],
                order.iter().map(|&i| answers[i].clone()).collect(),
            );
            let computed = fold(&store);
            for id in ["r0", "r1"] {
                assert_eq!(computed.resolution(id), Resolution::Undecidable);
                assert!(computed.defects.iter().any(|d| d.about.as_deref() == Some(id)));
            }
            assert!(current_answer(&store, "r0").is_none());
            assert!(answer_history(&store, "r0").is_empty());
        }
    }
}

#[test]
fn duplicate_request_answer_and_cross_kind_ids_make_owners_and_dependents_undecidable() {
    let base = store_of(
        vec![request("r0", 1, &[], &["yes", "no"]),
             request("r1", 2, &[("r0", "yes")], &["a"])],
        vec![answer("x0", "r0", &["yes"], None)],
    );
    let mut duplicate_request = base.clone();
    duplicate_request.requests.push(request("r0", 3, &[], &["yes"]));
    let mut duplicate_answer = base.clone();
    duplicate_answer.answers.push(answer("x0", "r0", &["no"], Some("x0")));
    let mut cross_kind = base;
    cross_kind.answers[0].id = "r0".into();
    assert!(duplicate_request.request("r0").is_none());
    assert!(duplicate_request.resolve("r0").is_err());
    assert!(duplicate_request.resolve("Q1").is_err());
    assert!(cross_kind.request("r0").is_none());
    assert!(cross_kind.resolve("Q1").is_err());
    for store in [duplicate_request, duplicate_answer, cross_kind] {
        for request_order in permutations(store.requests.len()) {
            for answer_order in permutations(store.answers.len()) {
                let permuted = store_of(
                    request_order.iter().map(|&i| store.requests[i].clone()).collect(),
                    answer_order.iter().map(|&i| store.answers[i].clone()).collect(),
                );
                let computed = fold(&permuted);
                assert!(computed.defects.iter().any(|d| d.code == DefectCode::DuplicateId));
                assert_eq!(computed.resolution("r0"), Resolution::Undecidable);
                assert_eq!(computed.resolution("r1"), Resolution::Undecidable);
                assert!(current_answer(&permuted, "r0").is_none());
                assert!(answer_history(&permuted, "r0").is_empty());
            }
        }
    }
}

#[test]
fn an_unknown_selected_choice_invalidates_the_owner_and_its_dependents() {
    for choices in [&["maybe"][..], &["yes", "maybe"][..]] {
        let store = store_of(
            vec![request("r0", 1, &[], &["yes", "no"]),
                 request("r1", 2, &[("r0", "yes")], &["a"])],
            vec![answer("x0", "r0", choices, None)],
        );
        let computed = fold(&store);
        assert_eq!(computed.resolution("r0"), Resolution::Undecidable);
        assert_eq!(computed.resolution("r1"), Resolution::Undecidable);
        assert!(computed.defects.iter().any(|d|
            d.code == DefectCode::UnknownOption && d.about.as_deref() == Some("r0")));
        let mut repaired = store;
        repaired.answers.push(answer("x1", "r0", &["yes"], Some("x0")));
        let computed = fold(&repaired);
        assert_eq!(computed.resolution("r0"), Resolution::State(State::Answered));
        assert_eq!(computed.resolution("r1"), Resolution::State(State::Pending));
        assert!(computed.defects.iter().any(|d|
            d.code == DefectCode::UnknownOption && d.about.as_deref() == Some("x0")));
    }
}

#[test]
fn dangling_assumption_supersession_is_attributed_to_the_assumption() {
    let mut store = store_of(vec![request("r0", 1, &[], &["yes"])], vec![]);
    store.assumptions.push(Assumption {
        id: "assume".into(), written_ms: 1, assumes: "r0".into(),
        assumed_by: "agent".into(), supersedes: Some("missing".into()), body: String::new(),
    });
    let computed = fold(&store);
    assert!(computed.defects.iter().any(|d|
        d.code == DefectCode::DanglingSupersession && d.about.as_deref() == Some("assume")));
}

#[test]
fn handle_allocation_errors_at_saturation_instead_of_wrapping_or_reusing() {
    assert_eq!(Store::default().next_handle().expect("first handle"), 1);
    let mut store = store_of(vec![request("r0", u64::MAX - 1, &[], &["yes"])], vec![]);
    assert_eq!(store.next_handle().expect("last available handle"), u64::MAX);
    store.requests.push(request("r1", u64::MAX, &[], &["yes"]));
    assert!(store.next_handle().is_err());
    store.requests.reverse();
    assert!(store.next_handle().is_err());
}

fn answered_dependency_chain(edges: usize) -> Store {
    let mut store = Store::default();
    for index in 0..=edges {
        let id = format!("r{index}");
        let parent = format!("r{}", index.saturating_sub(1));
        let guard = if index == 0 { Vec::new() } else { vec![(parent.as_str(), "yes")] };
        store.requests.push(request(&id, index as u64 + 1, &guard, &["yes"]));
        store.answers.push(answer(&format!("x{index}"), &id, &["yes"], None));
    }
    store
}

#[test]
fn the_guard_depth_limit_accepts_the_boundary_and_rejects_the_next_edge() {
    let at_limit = answered_dependency_chain(MAX_GUARD_DEPTH);
    let computed = fold(&at_limit);
    assert_eq!(computed.resolution(&format!("r{MAX_GUARD_DEPTH}")), Resolution::State(State::Answered));
    assert!(!computed.defects.iter().any(|d| d.code == DefectCode::LimitExceeded));
    let beyond = answered_dependency_chain(MAX_GUARD_DEPTH + 1);
    let id = format!("r{}", MAX_GUARD_DEPTH + 1);
    let computed = fold(&beyond);
    assert_eq!(computed.resolution(&id), Resolution::Undecidable);
    assert!(computed.defects.iter().any(|d|
        d.code == DefectCode::LimitExceeded && d.about.as_deref() == Some(id.as_str())));
}

#[test]
fn twenty_thousand_answered_dependencies_are_safe_on_a_two_mebibyte_stack() {
    std::thread::Builder::new().stack_size(2 * 1024 * 1024).spawn(|| {
        let store = answered_dependency_chain(19_999);
        let computed = fold(&store);
        assert_eq!(computed.resolution("r19999"), Resolution::Undecidable);
        assert!(computed.defects.iter().any(|d|
            d.code == DefectCode::LimitExceeded && d.about.as_deref() == Some("r19999")));
    }).expect("spawn small-stack fold").join().expect("fold must not panic");
}

#[test]
fn the_guard_term_limit_accepts_the_boundary_and_rejects_one_more_term() {
    let mut store = Store::default();
    for index in 0..MAX_GUARD_TERMS + 1 {
        let id = format!("r{index}");
        store.requests.push(request(&id, index as u64 + 1, &[], &["yes"]));
        store.answers.push(answer(&format!("x{index}"), &id, &["yes"], None));
    }
    let mut child = request("child", MAX_GUARD_TERMS as u64 + 2, &[], &["yes"]);
    child.applies_when = (0..MAX_GUARD_TERMS).map(|index| Term {
        decision: format!("r{index}"), option: "yes".into(),
    }).collect();
    store.requests.push(child);
    assert_eq!(fold(&store).resolution("child"), Resolution::State(State::Pending));
    store.requests.last_mut().expect("child").applies_when.push(Term {
        decision: format!("r{MAX_GUARD_TERMS}"), option: "yes".into(),
    });
    let computed = fold(&store);
    assert_eq!(computed.resolution("child"), Resolution::Undecidable);
    assert!(computed.defects.iter().any(|d|
        d.code == DefectCode::LimitExceeded && d.about.as_deref() == Some("child")));
}

#[test]
fn the_record_limit_counts_every_record_vector_and_parse_defects() {
    let mut store = store_of(vec![request("r0", 1, &[], &["yes"])],
                             vec![answer("x0", "r0", &["yes"], None)]);
    store.assumptions.push(Assumption {
        id: "assume".into(), written_ms: 1, assumes: "r0".into(),
        assumed_by: "agent".into(), supersedes: None, body: String::new(),
    });
    store.promotions.push(Promotion {
        id: "promote".into(), written_ms: 1, promotes: "r0".into(),
        target: "context/decision.md".into(), body: String::new(),
    });
    store.parse_defects = (0..MAX_RECORDS - 4).map(|index| Defect {
        about: Some(format!("bad{index}")), code: DefectCode::MalformedRecord,
        detail: "invalid frontmatter".into(),
    }).collect();
    let computed = fold(&store);
    assert_eq!(computed.resolution("r0"), Resolution::State(State::Answered));
    assert!(!computed.defects.iter().any(|d| d.code == DefectCode::LimitExceeded));
    store.parse_defects.push(Defect {
        about: Some("overflow".into()), code: DefectCode::MalformedRecord,
        detail: "invalid frontmatter".into(),
    });
    let computed = fold(&store);
    assert_eq!(computed.resolution("r0"), Resolution::Undecidable);
    assert!(computed.defects.iter().any(|d| d.code == DefectCode::LimitExceeded));
}

#[test]
fn imported_yes_no_and_free_text_answers_preserve_history_but_cannot_settle_guards() {
    for choices in [&["yes"][..], &["no"][..], &[][..]] {
        let mut imported = answer("import", "r0", choices, None);
        imported.provenance = AnswerProvenance::Imported;
        imported.body = "Historical human answer.".into();
        let mut store = store_of(
            vec![request("r0", 1, &[], &["yes", "no"]),
                 request("r1", 2, &[("r0", "yes")], &["a"])],
            vec![imported],
        );
        let computed = fold(&store);
        assert_eq!(computed.resolution("r0"), Resolution::State(State::Answered));
        assert_eq!(computed.resolution("r1"), Resolution::Undecidable);
        assert!(computed.defects.iter().any(|d|
            d.code == DefectCode::ImportedAnswer && d.about.as_deref() == Some("r1")));
        assert_eq!(current_answer(&store, "r0").expect("import remains current").id, "import");
        let history = answer_history(&store, "r0");
        assert_eq!(history[0].provenance, AnswerProvenance::Imported);
        assert_eq!(history[0].body, "Historical human answer.");
        store.answers.push(answer("native", "r0", &["yes"], Some("import")));
        let computed = fold(&store);
        assert_eq!(computed.resolution("r1"), Resolution::State(State::Pending));
        assert!(!computed.defects.iter().any(|d| d.code == DefectCode::ImportedAnswer));
        assert!(!computed.revived.contains("r1"), "an imported no is not evidence of earlier mootness");
        assert_eq!(current_answer(&store, "r0").expect("native successor").provenance, AnswerProvenance::Native);
        let history = answer_history(&store, "r0");
        assert_eq!(history.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["import", "native"]);
        assert_eq!(history[0].provenance, AnswerProvenance::Imported);
    }
}

#[test]
fn provenance_parser_defaults_unknown_accepts_known_values_and_rejects_invalid_values() {
    for (field, expected) in [
        ("", AnswerProvenance::Unknown),
        ("provenance: native\n", AnswerProvenance::Native),
        ("provenance: imported\n", AnswerProvenance::Imported),
        ("provenance: unknown\n", AnswerProvenance::Unknown),
    ] {
        let document = format!("---\nrecord: answer\nanswers: r0\nanswered-by: human\n{field}---\nOriginal body.\n");
        let Record::Answer(parsed) = parse_record("x0", 1, &document).expect("valid provenance") else {
            panic!("expected answer");
        };
        assert_eq!(parsed.provenance, expected);
        assert_eq!(parsed.body, "Original body.\n");
    }
    for value in ["invalid", "Native", "IMPORTED", ""] {
        let document = format!("---\nrecord: answer\nanswers: r0\nanswered-by: human\nprovenance: {value}\n---\n");
        assert!(parse_record("x0", 1, &document).is_err(), "reject provenance {value:?}");
    }
}

#[test]
fn an_imported_current_answer_cannot_settle_a_guard_even_when_its_owner_is_moot() {
    let mut imported = answer("x1", "r1", &["yes"], None);
    imported.provenance = AnswerProvenance::Imported;
    let store = store_of(
        vec![request("r0", 1, &[], &["yes", "no"]),
             request("r1", 2, &[("r0", "yes")], &["yes"]),
             request("r2", 3, &[("r1", "yes")], &["a"])],
        vec![answer("x0", "r0", &["no"], None), imported],
    );
    let computed = fold(&store);
    assert_eq!(computed.resolution("r1"), Resolution::State(State::Moot));
    assert_eq!(computed.resolution("r2"), Resolution::Undecidable);
    assert!(computed.defects.iter().any(|d|
        d.code == DefectCode::ImportedAnswer && d.about.as_deref() == Some("r2")));
    assert_eq!(current_answer(&store, "r1").expect("history preserved").id, "x1");
}

#[test]
fn revival_and_supersession_history_are_invariant_under_all_record_permutations() {
    let requests = vec![request("r0", 1, &[], &["yes", "no"]),
                        request("r1", 2, &[("r0", "yes")], &["a"]),
                        request("r2", 3, &[("r1", "a")], &["z"])];
    let answers = vec![answer("x0", "r0", &["no"], None),
                       answer("x1", "r0", &["yes"], Some("x0")),
                       answer("x2", "r1", &["a"], None)];
    let expected = fold(&store_of(requests.clone(), answers.clone()));
    assert!(expected.revived.contains("r2"));
    assert!(!expected.revived.contains("r1"), "answered requests are not revival obligations");
    for request_order in permutations(requests.len()) {
        for answer_order in permutations(answers.len()) {
            let store = store_of(
                request_order.iter().map(|&i| requests[i].clone()).collect(),
                answer_order.iter().map(|&i| answers[i].clone()).collect(),
            );
            let computed = fold(&store);
            assert_eq!(computed.resolutions, expected.resolutions);
            assert_eq!(computed.defects, expected.defects);
            assert_eq!(computed.revived, expected.revived);
            assert_eq!(answer_history(&store, "r0").iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["x0", "x1"]);
            assert_eq!(current_answer(&store, "r0").expect("chain tip").id, "x1");
        }
    }
}

#[test]
fn missing_provenance_never_settles_a_guard_or_manufactures_revival() {
    for choice in ["yes", "no", ""] {
        let mut store = store_of(
            vec![request("r0", 1, &[], &["yes", "no"]),
                 request("r1", 2, &[("r0", "yes")], &["a"])],
            vec![],
        );
        store.push(parse_record("legacy", 1, &format!(
            "---\nrecord: answer\nanswers: r0\nanswered-by: human\nchoice: [{choice}]\n---\nLegacy answer.\n"
        )).expect("legacy history parses without authority"));
        let computed = fold(&store);
        assert_eq!(computed.resolution("r0"), Resolution::State(State::Answered));
        assert_eq!(computed.resolution("r1"), Resolution::Undecidable);
        assert!(computed.defects.iter().any(|d|
            d.code == DefectCode::UnknownAnswerProvenance && d.about.as_deref() == Some("r1")));
        assert_eq!(current_answer(&store, "r0").expect("history retained").provenance,
            AnswerProvenance::Unknown);
        store.answers.push(answer("native", "r0", &["yes"], Some("legacy")));
        let computed = fold(&store);
        assert_eq!(computed.resolution("r1"), Resolution::State(State::Pending));
        assert!(!computed.revived.contains("r1"), "unknown history is not evidence of earlier mootness");
        assert_eq!(answer_history(&store, "r0").iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["legacy", "native"]);
    }
}
