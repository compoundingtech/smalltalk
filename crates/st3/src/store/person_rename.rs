//! One-shot repair of current person routing. Historical actors and published missions stay put.
use super::*;
use crate::model::{PersonRenameReport, PersonRenameRequest};

impl Store {
    /// Reassign current open work and pending reviews in one writer transaction. The completion
    /// claim is the durable idempotency receipt, including after reopen or replication.
    pub fn rename_person(
        &self,
        input: &PersonRenameRequest,
    ) -> Result<PersonRenameReport, St3Error> {
        for person in [&input.old_person, &input.new_person] {
            validate_claim_subject(person)?;
            if !person.starts_with("person/") || person.matches('/').count() != 1 || person.contains('*') {
                return Err(St3Error::new("invalid-person-rename", "OLD and NEW must be complete person subjects"));
            }
        }
        if input.old_person == input.new_person || input.idempotency_key.trim().is_empty() {
            return Err(St3Error::new("invalid-person-rename", "a rename needs distinct people and a nonempty idempotency key"));
        }
        validate_claim_subject(&input.actor)?;
        self.connection.batched(|tx| {
            let receipt = tx.query_row(&canonical_sql(
                "SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms
                 FROM claims WHERE kind='person.rename.completed'
                 AND json_extract(body,'$.idempotency_key')=?1 ORDER BY CANONICAL_ASC(claims) LIMIT 1"),
                [&input.idempotency_key], claim_from_row).optional().map_err(internal)?;
            if let Some(receipt) = receipt {
                if receipt.actor.as_deref() != Some(input.actor.as_str())
                    || receipt.body["fields"]["old_person"] != input.old_person
                    || receipt.body["fields"]["new_person"] != input.new_person
                {
                    return Err(St3Error::new("idempotency-conflict", "this rename key already names another request"));
                }
                if input.actor != input.old_person && input.actor != input.new_person {
                    return Err(St3Error::new("person-rename-forbidden", "only OLD or NEW may repair their person identity"));
                }
                if winner_person_tx(tx, &input.old_person).map_err(internal)?
                    .is_some_and(|winner| winner != input.new_person)
                {
                    return Err(St3Error::new("person-rename-conflict", "this repair lost to another canonical completed repair of OLD"));
                }
                return serde_json::from_value(receipt.body["fields"]["report"].clone()).map_err(internal);
            }
            if input.actor != input.old_person && input.actor != input.new_person {
                return Err(St3Error::new("person-rename-forbidden", "only OLD or NEW may repair their person identity"));
            }
            let competing: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM claims WHERE subject=?1 AND kind='person.rename.completed'
                 AND json_extract(body,'$.fields.new_person')<>?2)",
                params![input.old_person, input.new_person], |row| row.get(0)).map_err(internal)?;
            if competing {
                return Err(St3Error::new("person-rename-conflict", "OLD already has a completed repair to another person"));
            }
            if !known_person_tx(tx, &input.new_person).map_err(internal)? {
                return Err(St3Error::new("unknown-person", "NEW must already be known from stored person identity or routing evidence"));
            }
            let mut report = PersonRenameReport {
                old_person: input.old_person.clone(),
                new_person: input.new_person.clone(),
                reassigned_steps: Vec::new(),
                reassigned_reviews: Vec::new(),
                run_requesters: Vec::new(),
                missions_to_republish: Vec::new(),
                unchanged_attention_requests: Vec::new(),
                unchanged_fault_cards: Vec::new(),
                unchanged_revision_approvals: Vec::new(),
                unchanged_planning_launch_approvals: Vec::new(),
                live_run_references: BTreeMap::new(),
                published_mission_references: BTreeMap::new(),
                namespace_notice: "Client-derived custom/person namespaces are not automatically moved; no aliases or namespace migrations are created.".into(),
            };
            report.unchanged_attention_requests = unchanged_attention_requests_tx(tx, &input.old_person).map_err(internal)?;
            for fault in self.fault_snapshot(now_ms()).map_err(internal)? {
                if fault.item.person == input.old_person {
                    report.unchanged_fault_cards.push(fault.item.episode);
                }
            }
            for item in self.broken_gate_items(Some(&input.old_person), now_ms()).map_err(internal)? {
                report.unchanged_fault_cards.push(item.episode);
            }
            for item in self.mission_run_attention_items(Some(&input.old_person)).map_err(internal)? {
                match item.kind.as_str() {
                    "revision-approval" => report.unchanged_revision_approvals.push(item.subject),
                    "launch-approval" => report.unchanged_planning_launch_approvals.push(item.subject),
                    _ => {}
                }
            }
            let mut runs = tx.prepare(
                "SELECT m.id,m.requester,r.body FROM mission_runs m
                 JOIN run_generations g ON g.id=m.current_generation_id
                 JOIN mission_revisions r ON r.mission_id=m.mission_id AND r.revision=g.revision
                 WHERE m.status NOT IN ('completed','failed','cancelled') ORDER BY m.id").map_err(internal)?;
            for row in runs.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))).map_err(internal)? {
                let (run, requester, body) = row.map_err(internal)?;
                if !person_work::run_live(tx, &run, None, false).map_err(internal)? { continue; }
                let subject = format!("mission-run/{run}");
                let mut references = routing_references(&serde_json::from_str::<Value>(&body).map_err(internal)?, &input.old_person);
                if requester == input.old_person {
                    report.run_requesters.push(subject.clone());
                    references.push("/requester".into());
                }
                references.sort();
                if !references.is_empty() { report.live_run_references.insert(subject, references); }
            }
            let mut missions = tx.prepare(
                "SELECT r.body FROM mission_definitions d JOIN mission_revisions r
                 ON r.mission_id=d.mission_id AND r.revision=d.revision ORDER BY d.mission_id").map_err(internal)?;
            for body in missions.query_map([], |row| row.get::<_, String>(0)).map_err(internal)? {
                let mission: MissionSpec = serde_json::from_str(&body.map_err(internal)?).map_err(internal)?;
                let references = routing_references(&serde_json::to_value(&mission).map_err(internal)?, &input.old_person);
                if !references.is_empty() {
                    report.missions_to_republish.push(mission.subject.clone());
                    report.published_mission_references.insert(mission.subject, references);
                }
            }
            for subjects in [&mut report.unchanged_attention_requests, &mut report.unchanged_fault_cards,
                &mut report.unchanged_revision_approvals, &mut report.unchanged_planning_launch_approvals] {
                subjects.sort();
                subjects.dedup();
            }
            // The receipt fence above and both reassignments share this writer transaction:
            // another destination cannot observe or move only part of the repaired routing.
            report.reassigned_steps = person_work::reassign_person_steps_tx(tx, &self.origin, input)?;
            report.reassigned_reviews = human_review_reassignment::reassign_reviews_tx(tx, &self.origin, input)?;
            let mut remaining_steps = tx.prepare(
                "SELECT s.run_id,s.subject FROM step_runs s JOIN mission_runs m
                 ON m.id=s.run_id AND m.current_generation_id=s.generation_id
                 WHERE s.assignee=?1 AND m.status NOT IN ('completed','failed','cancelled')
                 ORDER BY s.run_id,s.subject").map_err(internal)?;
            for row in remaining_steps.query_map([&input.old_person], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).map_err(internal)? {
                let (run, step) = row.map_err(internal)?;
                if person_work::run_live(tx, &run, None, false).map_err(internal)? {
                    let segment = step.replace('~', "~0").replace('/', "~1");
                    report.live_run_references.entry(format!("mission-run/{run}")).or_default()
                        .push(format!("/step_runs/{segment}/assignee"));
                }
            }
            for references in report.live_run_references.values_mut() {
                references.sort();
                references.dedup();
            }
            append_claim_tx(tx, &self.origin, &input.old_person, "person.rename.completed", Some(&input.actor),
                &json!({"fields": {"old_person": input.old_person, "new_person": input.new_person, "report": report},
                    "idempotency_key": input.idempotency_key}), &[], None).map_err(claim_append_error)?;
            Ok(report)
        }).map_err(internal)?
    }
}

fn known_person_tx(connection: &Connection, person: &str) -> Result<bool> {
    let known: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM claims
         WHERE kind NOT IN ('person.rename.completed','gate.reviewer-reassigned','work.person-reassigned')
         AND (actor=?1 OR subject=?1 OR json_extract(body,'$.fields.reviewer')=?1
           OR json_extract(body,'$.fields.person')=?1 OR json_extract(body,'$.fields.owner')=?1
           OR json_extract(body,'$.fields.requester')=?1 OR json_extract(body,'$.fields.assignee')=?1))
         OR EXISTS(SELECT 1 FROM mission_runs WHERE requester=?1)
         OR EXISTS(SELECT 1 FROM planning_sessions WHERE requester=?1)",
        [person], |row| row.get(0))?;
    if known { return Ok(true); }
    let mut missions = connection.prepare("SELECT body FROM mission_revisions ORDER BY mission_id,revision")?;
    for body in missions.query_map([], |row| row.get::<_, String>(0))? {
        if !routing_references(&serde_json::from_str::<Value>(&body?)?, person).is_empty() { return Ok(true); }
    }
    Ok(false)
}

fn unchanged_attention_requests_tx(connection: &Connection, person: &str) -> Result<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT DISTINCT request.subject FROM claims request
         WHERE request.kind='attention.requested' AND json_extract(request.body,'$.fields.reviewer')=?1
         AND NOT EXISTS(SELECT 1 FROM claims resolved WHERE resolved.subject=request.subject
           AND resolved.kind='attention.resolved') ORDER BY request.subject")?;
    let subjects = statement.query_map([person], |row| row.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(subjects)
}

/// Match structured routes, never prose, goals, historical actors or arbitrary input strings.
/// Recursion covers retained nested missions and every loop round/branch/gate without rewriting them.
fn routing_references(value: &Value, person: &str) -> Vec<String> {
    fn visit(value: &Value, person: &str, path: &str, route: bool, found: &mut Vec<String>) {
        match value {
            Value::String(subject) if route && subject == person => found.push(path.to_owned()),
            Value::Object(fields) => for (name, child) in fields {
                if name == "inputs" { continue; }
                let segment = name.replace('~', "~0").replace('/', "~1");
                let route = matches!(name.as_str(), "assignee" | "assigned_to" | "available_to"
                    | "agent" | "agents" | "reviewer" | "revision_reviewer" | "revision_owners" | "report_to");
                visit(child, person, &format!("{path}/{segment}"), route, found);
            },
            Value::Array(values) => for (index, child) in values.iter().enumerate() {
                visit(child, person, &format!("{path}/{index}"), route, found);
            },
            _ => {}
        }
    }
    let mut found = Vec::new();
    visit(value, person, "", false, &mut found);
    found.sort();
    found
}

/// Both work and human-review reducers must choose the same replicated receipt winner.
/// Before any receipt arrives, existing episode fences still govern provisional assignments.
/// Receipt arrival invalidates affected run trees so canonical replay removes a losing route.
pub(super) fn accepted_reassignment_sql(alias: &str, previous: &str, destination: &str) -> String {
    let old = format!("json_extract({alias}.body,'$.fields.{previous}')");
    let new = format!("json_extract({alias}.body,'$.fields.{destination}')");
    format!(
        "(NOT EXISTS(SELECT 1 FROM claims rename_receipt
            WHERE rename_receipt.kind='person.rename.completed' AND rename_receipt.subject={old})
          OR {new}=(SELECT json_extract(rename_receipt.body,'$.fields.new_person')
            FROM claims rename_receipt
            WHERE rename_receipt.kind='person.rename.completed' AND rename_receipt.subject={old}
            ORDER BY {} LIMIT 1))",
        canonical::order_sql("rename_receipt", false),
    )
}

fn winner_person_tx(connection: &Connection, old: &str) -> Result<Option<String>> {
    Ok(connection.query_row(
        &canonical_sql("SELECT json_extract(body,'$.fields.new_person') FROM claims
            WHERE kind='person.rename.completed' AND subject=?1
            ORDER BY CANONICAL_ASC(claims) LIMIT 1"),
        [old], |row| row.get(0)).optional()?)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn know(store: &Store, person: &str) {
        store.append_claim(&ClaimInput {
            subject: format!("message/known-{}", person.strip_prefix("person/").unwrap()),
            kind: "message.sent".into(),
            actor: Some(person.into()),
            fields: BTreeMap::from([
                ("from".into(), json!(person)),
                ("to".into(), json!("person/alex")),
                ("content".into(), json!("Identity evidence predates the repair.")),
                ("status".into(), json!("sent")),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("known:{person}")),
        }).unwrap();
    }

    #[test]
    fn report_lists_only_live_requesters_and_published_reviewer_references() {
        let store = Store::open_memory("alder").unwrap();
        know(&store, "person/robin");
        let intent = crate::graph::parse_internal_intent(
            r#"version 2
mission "protected" state="ready" revisions="human-only" revision-reviewer="person/avery" {
  goal "Protect the revision.";
  step "work" { agentless; }
}
mission "human" state="ready" {
  goal "Get a human review.";
  step "work" { agentless; gate "review" type="human" { reviewer "person/avery"; } }
}
mission "exhaustion" state="ready" {
  goal "Bound the automatic work.";
  loop "work" {
    max-rounds 2
    round { completion { when "all-steps-exhausted"; } }
    on-exhausted { fail; attention "Review failure" { reviewer "person/avery"; severity "error"; } }
  }
}
mission "unrelated" state="ready" {
  goal "Mention person/avery without treating prose as routing.";
  step "work" { agentless; }
}
"#, "alder").unwrap();
        store.apply_internal(&intent, "rename-report-definitions").unwrap();
        let run = |mission: &str| store.create_mission_run(&MissionRunRequest {
            mission: mission.into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/avery".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: format!("report-run:{mission}"),
        }).unwrap();
        let live = run("unrelated");
        let ended = run("protected");
        store.set_mission_run_state(&ended.subject, "cancelled", "terminal", Some("ended")).unwrap();
        let input = PersonRenameRequest {
            old_person: "person/avery".into(),
            new_person: "person/robin".into(),
            actor: "person/avery".into(),
            idempotency_key: "report-rename".into(),
        };
        let report = store.rename_person(&input).unwrap();
        assert_eq!(report.run_requesters, vec![live.subject.clone()]);
        assert_eq!(report.missions_to_republish, vec![
            "mission/exhaustion".to_string(), "mission/human".to_string(), "mission/protected".to_string(),
        ]);
        assert_eq!(store.mission_run(&live.subject).unwrap().unwrap().requester, "person/avery");
        for (name, original) in &intent.missions {
            assert_eq!(store.mission_spec(name, None).unwrap().unwrap(), *original);
        }
        store.replay_replication_graph().unwrap();
        assert_eq!(store.rename_person(&input).unwrap(), report);
    }

    #[test]
    fn malformed_people_actors_and_empty_keys_never_record_a_receipt() {
        let store = Store::open_memory("alder").unwrap();
        know(&store, "person/robin");
        let valid = PersonRenameRequest {
            old_person: "person/avery".into(),
            new_person: "person/robin".into(),
            actor: "person/avery".into(),
            idempotency_key: "rename-validation".into(),
        };
        for (old, new, actor, key) in [
            ("avery", "person/robin", "person/operator", "key"),
            ("person/avery/nested", "person/robin", "person/operator", "key"),
            ("person/avery", "person/", "person/operator", "key"),
            ("person/avery", "person/avery", "person/operator", "key"),
            ("person/avery", "person/robin", "requester", "key"),
            ("person/avery", "person/robin", "daemon/runtime", "key"),
            ("person/avery", "person/robin", "person/operator/nested", "key"),
            ("person/avery", "person/robin", "agent/*", "key"),
            ("person/avery", "person/robin", "person/operator", " "),
        ] {
            let input = PersonRenameRequest {
                old_person: old.into(), new_person: new.into(), actor: actor.into(), idempotency_key: key.into(),
            };
            assert!(store.rename_person(&input).is_err());
        }
        let report = store.rename_person(&valid).unwrap();
        assert_eq!(report.old_person, valid.old_person);
        assert_eq!(report.new_person, valid.new_person);
        let receipts: u64 = store.readers.get().query_row(
            "SELECT COUNT(*) FROM claims WHERE kind='person.rename.completed'", [], |row| row.get(0)).unwrap();
        assert_eq!(receipts, 1);
    }

    fn repair_input(new_person: &str, key: &str) -> PersonRenameRequest {
        PersonRenameRequest {
            old_person: "person/avery".into(),
            new_person: new_person.into(),
            actor: "person/avery".into(),
            idempotency_key: key.into(),
        }
    }

    fn claim_count(store: &Store) -> u64 {
        store.readers.get().query_row("SELECT COUNT(*) FROM claims", [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn only_old_or_known_new_can_authorize_and_refusals_write_nothing() {
        let store = Store::open_memory("alder").unwrap();
        let mut input = repair_input("person/robin", "authority");
        let before = claim_count(&store);
        for actor in ["person/avery", "person/robin"] {
            input.actor = actor.into();
            assert_eq!(store.rename_person(&input).unwrap_err().code, "unknown-person");
            assert_eq!(claim_count(&store), before);
        }
        know(&store, "person/robin");
        let before = claim_count(&store);
        for actor in ["agent/alder.operator", "person/operator", "person/avery/session/device"] {
            input.actor = actor.into();
            assert_eq!(store.rename_person(&input).unwrap_err().code, "person-rename-forbidden");
            assert_eq!(claim_count(&store), before);
        }
        input.actor = "person/robin".into();
        let report = store.rename_person(&input).unwrap();
        let before = claim_count(&store);
        assert_eq!(store.rename_person(&input).unwrap(), report);
        assert_eq!(claim_count(&store), before);
        input.actor = "person/operator".into();
        assert_eq!(store.rename_person(&input).unwrap_err().code, "idempotency-conflict");
        assert_eq!(claim_count(&store), before);
    }

    #[test]
    fn competing_destinations_are_atomic_and_receipt_survives_replay_and_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rename.sqlite3");
        let store = std::sync::Arc::new(Store::open(&path, "alder").unwrap());
        for person in ["person/robin", "person/blair"] { know(&store, person); }
        let intent = crate::graph::parse_internal_intent(
            r#"version 2
mission "routing" state="ready" {
  goal "Keep one repair destination.";
  step "one" { assigned-to "person/avery"; }
  step "two" { assigned-to "person/avery"; }
}"#, "alder").unwrap();
        store.apply_internal(&intent, "concurrent-mission").unwrap();
        let run = store.create_mission_run(&MissionRunRequest {
            mission: "routing".into(), revision: None, workspace: "/tmp".into(),
            requester: Some("person/avery".into()), mode: Some("run".into()),
            inputs: BTreeMap::new(), idempotency_key: "concurrent-run".into(),
        }).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles = ["person/robin", "person/blair"].map(|person| {
            let store = std::sync::Arc::clone(&store);
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                let input = repair_input(person, &format!("concurrent:{person}"));
                barrier.wait();
                let result = store.rename_person(&input);
                (input, result)
            })
        });
        let results = handles.map(|handle| handle.join().unwrap());
        assert_eq!(results.iter().filter(|(_, result)| result.is_ok()).count(), 1);
        let (winner, report) = results.iter().find_map(|(input, result)| result.as_ref().ok().map(|report| (input.clone(), report.clone()))).unwrap();
        let loser = results.iter().find(|(_, result)| result.is_err()).unwrap();
        assert_eq!(loser.1.as_ref().unwrap_err().code, "person-rename-conflict");
        assert_eq!(report.reassigned_steps.len(), 2);
        for step in &run.steps {
            assert_eq!(store.step_run(&step.subject).unwrap().unwrap().assigned_to.as_deref(), Some(winner.new_person.as_str()));
        }
        let before = claim_count(&store);
        assert_eq!(store.rename_person(&winner).unwrap(), report);
        assert_eq!(claim_count(&store), before);
        store.replay_replication_graph().unwrap();
        assert_eq!(store.rename_person(&winner).unwrap(), report);
        assert_eq!(store.rename_person(&loser.0).unwrap_err().code, "person-rename-conflict");
        drop(store);
        let reopened = Store::open(&path, "alder").unwrap();
        let before = claim_count(&reopened);
        assert_eq!(reopened.rename_person(&winner).unwrap(), report);
        assert_eq!(reopened.rename_person(&loser.0).unwrap_err().code, "person-rename-conflict");
        assert_eq!(claim_count(&reopened), before);
    }

    #[test]
    fn retained_run_routes_and_published_assignees_are_reported_without_mutation() {
        let store = Store::open_memory("alder").unwrap();
        know(&store, "person/robin");
        let original = crate::graph::parse_internal_intent(
            r#"version 2
mission "retained" state="ready" revisions="human-only" revision-reviewer="person/avery" {
  goal "Keep retained routing visible.";
  step "work" { assigned-to "person/avery"; gate "review" type="human" { reviewer "person/avery"; } }
}"#, "alder").unwrap();
        store.apply_internal(&original, "retained-original").unwrap();
        let run = store.create_mission_run(&MissionRunRequest {
            mission: "retained".into(), revision: None, workspace: "/tmp".into(),
            requester: Some("person/robin".into()), mode: Some("run".into()),
            inputs: BTreeMap::new(), idempotency_key: "retained-run".into(),
        }).unwrap();
        let updated = crate::graph::parse_internal_intent(
            r#"version 2
mission "retained" state="ready" {
  goal "Newly published routing.";
  step "work" { assigned-to "person/robin"; }
}
mission "future" state="ready" {
  goal "Report the future assignee.";
  step "work" { assigned-to "person/avery"; }
}"#, "alder").unwrap();
        store.apply_internal(&updated, "retained-updated").unwrap();
        let report = store.rename_person(&repair_input("person/robin", "retained-repair")).unwrap();
        assert!(report.run_requesters.is_empty());
        assert_eq!(report.live_run_references[&run.subject], vec![
            "/revision_reviewer", "/steps/work/gates/0/reviewer", "/steps/work/work_selector/agent",
        ]);
        assert_eq!(report.missions_to_republish, vec!["mission/future"]);
        assert_eq!(report.published_mission_references["mission/future"], vec!["/steps/work/work_selector/agent"]);
        assert_eq!(store.mission_run(&run.subject).unwrap().unwrap().requester, "person/robin");
        assert_eq!(store.mission_spec("retained", Some(&run.revision)).unwrap().unwrap(), original.missions["retained"]);
        assert_eq!(store.mission_spec("retained", None).unwrap().unwrap(), updated.missions["retained"]);
    }

    #[test]
    fn routing_paths_cover_nested_branches_and_ignore_prose_and_actors() {
        let references = routing_references(&json!({
            "revision_owners": ["person/avery"],
            "steps": {"a/b": {
                "work_selector": {"kind": "assigned", "agent": "person/avery"},
                "loop_spec": {
                    "round": {"steps": {"next": {"revision_reviewer": "person/avery"}}},
                    "until": [{"reviewer": "person/avery"}],
                    "candidates": {"select": {"gate": {"reviewer": "person/avery"}}},
                    "on_exhausted": {"gate": {"reviewer": "person/avery"}},
                    "exhaustion_attention": {"reviewer": "person/avery"},
                    "on_keep": {"report_to": "person/avery"},
                    "on_discard": {"gates": [{"reviewer": "person/avery"}]}
                },
                "nested_mission": {"revision_reviewer": "person/avery"}
            }},
            "goals": ["person/avery"], "actor": "person/avery",
            "inputs": {"text": "person/avery", "reviewer": "person/avery"}
        }), "person/avery");
        assert_eq!(references, vec![
            "/revision_owners/0",
            "/steps/a~1b/loop_spec/candidates/select/gate/reviewer",
            "/steps/a~1b/loop_spec/exhaustion_attention/reviewer",
            "/steps/a~1b/loop_spec/on_discard/gates/0/reviewer",
            "/steps/a~1b/loop_spec/on_exhausted/gate/reviewer",
            "/steps/a~1b/loop_spec/on_keep/report_to",
            "/steps/a~1b/loop_spec/round/steps/next/revision_reviewer",
            "/steps/a~1b/loop_spec/until/0/reviewer",
            "/steps/a~1b/nested_mission/revision_reviewer",
            "/steps/a~1b/work_selector/agent",
        ]);
    }

    #[test]
    fn historical_receipts_default_additive_report_fields() {
        let report: PersonRenameReport = serde_json::from_value(json!({
            "old_person": "person/avery", "new_person": "person/robin",
            "reassigned_steps": [], "reassigned_reviews": [], "run_requesters": [],
            "missions_to_republish": [], "namespace_notice": "original"
        })).unwrap();
        assert!(report.unchanged_attention_requests.is_empty());
        assert!(report.unchanged_fault_cards.is_empty());
        assert!(report.unchanged_revision_approvals.is_empty());
        assert!(report.unchanged_planning_launch_approvals.is_empty());
        assert!(report.live_run_references.is_empty());
        assert!(report.published_mission_references.is_empty());
    }

    #[test]
    fn report_pins_every_unchanged_inbox_category_and_excludes_closed_history() {
        let store = Store::open_memory("alder").unwrap();
        know(&store, "person/robin");
        let initial = crate::graph::parse_internal_intent(
            r#"version 2
mission "approvals" state="ready" revisions="human-only" revision-reviewer="person/avery" {
  goal "Approve the current revision.";
  step "work" { agentless; }
}"#, "alder").unwrap();
        store.apply_internal(&initial, "approval-initial").unwrap();
        let run = store.create_mission_run(&MissionRunRequest {
            mission: "approvals".into(), revision: None, workspace: "/tmp".into(),
            requester: Some("person/avery".into()), mode: Some("run".into()),
            inputs: BTreeMap::new(), idempotency_key: "approval-run".into(),
        }).unwrap();
        let candidate = crate::graph::parse_internal_intent(
            r#"version 2
mission "approvals" state="ready" revisions="human-only" revision-reviewer="person/avery" {
  goal "Approve the changed revision.";
  step "work" { agentless; }
}"#, "alder").unwrap();
        store.apply_internal(&candidate, "approval-candidate").unwrap();
        let proposal = store.create_revision_proposal(
            &run.subject, &candidate.missions["approvals"], "person/robin", "Review the revision.", "inbox-proposal",
        ).unwrap();
        assert_eq!(proposal.status, "pending-approval");
        let request = AttentionRequest {
            reviewer: "person/avery".into(), title: "Inspect".into(), reason: "Inspect unchanged routing.".into(),
            severity: "error".into(), targets: vec![run.subject.clone()],
            actor: "agent/alder.operator".into(), idempotency_key: "inbox-attention".into(),
        };
        store.request_attention("attention/unchanged", &request).unwrap();
        let closed = store.request_attention("attention/closed", &AttentionRequest {
            idempotency_key: "inbox-closed".into(), ..request.clone()
        }).unwrap();
        store.append_claim(&ClaimInput {
            subject: closed.subject.clone(), kind: "attention.resolved".into(), actor: Some("person/avery".into()),
            fields: BTreeMap::from([
                ("request".into(), json!(closed.request)),
                ("outcome".into(), json!("resolved")),
            ]),
            evidence: vec![closed.request], expected_subject: None, idempotency_key: Some("inbox-close".into()),
        }).unwrap();
        let fault = store.record_runtime_failure("inbox-broken-gate", &AttentionRequest {
            targets: vec![run.subject.clone(), run.generation.clone(), "gate-result/broken".into()],
            idempotency_key: "inbox-broken-gate".into(), ..request
        }, GATE_BROKEN_CONDITION).unwrap();
        let plan_kdl = "version 2\nmission \"planned\" state=\"ready\" { goal \"Plan the release.\" }\n";
        let plan = crate::graph::parse_internal_intent(plan_kdl, "alder").unwrap();
        let preview = store.mission(&plan, IntentInput { kdl: plan_kdl.into(), source_name: None }).unwrap();
        let planning_subject = "planning-session/inbox-plan";
        for (kind, fields) in [
            ("started", json!({"mission": "mission/planned", "request": "doc/request@hash",
                "workspace": "/tmp", "requester": "person/avery", "planner": "agent/alder.planner"})),
            ("candidate-submitted", json!({"candidate_revision": 1, "markdown": "doc/markdown@hash",
                "kdl": "doc/kdl@hash", "mission_revision": "planned-revision"})),
            ("previewed", json!({"candidate_revision": 1, "preview_hash": "inbox-preview",
                "store_index": preview.store_index, "graph": "planned", "diff": "new", "mission": preview})),
        ] {
            std::thread::sleep(std::time::Duration::from_millis(2));
            store.append_claim(&ClaimInput {
                subject: planning_subject.into(), kind: format!("planning-session.{kind}"),
                actor: Some(if kind == "previewed" { "daemon/runtime" } else { "person/avery" }.into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: Vec::new(), expected_subject: None, idempotency_key: Some(format!("inbox-plan:{kind}")),
            }).unwrap();
        }
        // Raw planning claims need the same projection as replicated planning events.
        store.replay_replication_graph().unwrap();
        let original_proposal = store.revision_proposal(&proposal.subject).unwrap().unwrap();
        let original_launch = store.planning_session(planning_subject).unwrap().unwrap();
        let original_fault = store.operational_failure("inbox-broken-gate").unwrap().unwrap();
        let input = repair_input("person/robin", "inbox-repair");
        let report = store.rename_person(&input).unwrap();
        assert_eq!(report.unchanged_attention_requests, vec!["attention/unchanged"]);
        assert_eq!(report.unchanged_fault_cards, vec![fault.request]);
        assert_eq!(report.unchanged_revision_approvals, vec![proposal.subject.clone()]);
        assert_eq!(report.unchanged_planning_launch_approvals, vec![planning_subject]);
        assert_eq!(store.attention_request("attention/unchanged").unwrap().unwrap().reviewer, "person/avery");
        assert_eq!(serde_json::to_value(store.revision_proposal(&proposal.subject).unwrap().unwrap()).unwrap(), serde_json::to_value(original_proposal).unwrap());
        assert_eq!(serde_json::to_value(store.planning_session(planning_subject).unwrap().unwrap()).unwrap(), serde_json::to_value(original_launch).unwrap());
        assert_eq!(serde_json::to_value(store.operational_failure("inbox-broken-gate").unwrap().unwrap()).unwrap(), serde_json::to_value(original_fault).unwrap());
        store.replay_replication_graph().unwrap();
        assert_eq!(store.rename_person(&input).unwrap(), report);
    }

    #[test]
    fn replicated_competing_receipts_choose_one_destination_for_work_and_reviews() {
        const FLEET: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
        fn receive(source: &Store, target: &Store, reverse: bool) {
            let mut exchange = source.export_replication_exchange_answering(
                FLEET, &target.replication_inventory().unwrap(), &[],
            ).unwrap();
            let mut envelopes = exchange.envelopes.clone();
            if reverse { envelopes.reverse(); }
            // Deliver each envelope separately: in forward order the completion receipt
            // arrives after its reassignment, so it must invalidate an already routed tree.
            for envelope in envelopes {
                exchange.envelopes = vec![envelope];
                target.receive_replication_exchange(&exchange.peer, FLEET, &exchange).unwrap();
                target.validate_replication_backlog().unwrap();
                target.project_replication_backlog().unwrap();
            }
        }
        let source = Store::open_memory("source").unwrap();
        know(&source, "person/robin");
        know(&source, "person/blair");
        let intent = crate::graph::parse_internal_intent(
            r#"version 2
mission "replicated-repair" state="ready" {
  goal "Route work and reviews together.";
  step "work" { assigned-to "person/avery"; }
  step "review" { agentless; }
}"#, "source").unwrap();
        source.apply_internal(&intent, "replicated-repair-mission").unwrap();
        let run = source.create_mission_run(&MissionRunRequest {
            mission: "replicated-repair".into(), revision: None, workspace: "/tmp".into(),
            requester: Some("person/avery".into()), mode: Some("run".into()),
            inputs: BTreeMap::new(), idempotency_key: "replicated-repair-run".into(),
        }).unwrap();
        let work = run.steps.iter().find(|step| step.step == "work").unwrap();
        let review = run.steps.iter().find(|step| step.step == "review").unwrap();
        let operation = "gate-operation/replicated-repair";
        source.append_claim(&ClaimInput {
            subject: operation.into(), kind: "gate.requested".into(), actor: Some("daemon/runtime".into()),
            fields: BTreeMap::from([
                ("owner".into(), json!(review.subject)), ("reviewer".into(), json!("person/avery")),
                ("mode".into(), json!("approve")), ("question".into(), json!("Approve?")),
                ("review_targets".into(), json!([])), ("decisions".into(), json!(["approved", "rejected"])),
                ("operation".into(), json!(operation)), ("mission_revision".into(), json!(run.revision)),
                ("step_definition".into(), json!(review.definition_hash)), ("attempt".into(), json!(review.attempt)),
            ]),
            evidence: Vec::new(), expected_subject: None, idempotency_key: Some("replicated-review".into()),
        }).unwrap();
        let alder = Store::open_memory("alder").unwrap();
        let birch = Store::open_memory("birch").unwrap();
        receive(&source, &alder, false);
        receive(&source, &birch, false);
        let first = repair_input("person/robin", "replicated-robin");
        let second = repair_input("person/blair", "replicated-blair");
        let first_report = alder.rename_person(&first).unwrap();
        let second_report = birch.rename_person(&second).unwrap();
        assert_eq!(first_report.reassigned_steps, vec![work.subject.clone()]);
        assert_eq!(second_report.reassigned_steps, vec![work.subject.clone()]);
        assert_eq!(first_report.reassigned_reviews, vec![review.subject.clone()]);
        assert_eq!(second_report.reassigned_reviews, vec![review.subject.clone()]);
        let mut destinations = Vec::new();
        for (origin, members, reverse) in [
            ("forward", [&alder, &birch], false),
            ("opposite", [&birch, &alder], false),
            ("reverse-envelopes", [&birch, &alder], true),
        ] {
            let target = Store::open_memory(origin).unwrap();
            for member in members { receive(member, &target, reverse); }
            let winner = winner_person_tx(&target.readers.get(), "person/avery").unwrap().unwrap();
            assert_eq!(target.step_run(&work.subject).unwrap().unwrap().assigned_to.as_deref(), Some(winner.as_str()));
            let reviews = target.pending_human_reviews(None).unwrap();
            assert_eq!(reviews.len(), 1);
            assert_eq!(reviews[0].reviewer, winner);
            let (winning_input, winning_report, losing_input) = if winner == first.new_person {
                (&first, &first_report, &second)
            } else {
                (&second, &second_report, &first)
            };
            let before = claim_count(&target);
            assert_eq!(target.rename_person(winning_input).unwrap(), *winning_report);
            assert_eq!(target.rename_person(losing_input).unwrap_err().code, "person-rename-conflict");
            assert_eq!(claim_count(&target), before);
            target.replay_replication_graph().unwrap();
            assert_eq!(target.step_run(&work.subject).unwrap().unwrap().assigned_to.as_deref(), Some(winner.as_str()));
            assert_eq!(target.pending_human_reviews(None).unwrap()[0].reviewer, winner);
            destinations.push(winner);
        }
        assert!(destinations.windows(2).all(|pair| pair[0] == pair[1]));
    }
}

