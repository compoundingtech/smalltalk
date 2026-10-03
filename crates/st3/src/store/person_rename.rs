//! One-shot repair of current person routing. Historical actors and published missions stay put.
use super::*;
use crate::model::{GateSpec, PersonRenameReport, PersonRenameRequest};

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
        if (!input.actor.starts_with("person/") && !input.actor.starts_with("agent/"))
            || (input.actor.starts_with("person/") && input.actor.matches('/').count() != 1)
            || input.actor.contains('*')
        {
            return Err(St3Error::new("invalid-person-rename", "a rename actor must be a concrete person or agent"));
        }
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
                return serde_json::from_value(receipt.body["fields"]["report"].clone()).map_err(internal);
            }
            let mut report = PersonRenameReport {
                old_person: input.old_person.clone(),
                new_person: input.new_person.clone(),
                reassigned_steps: person_work::reassign_person_steps_tx(tx, &self.origin, input)?,
                reassigned_reviews: human_review_reassignment::reassign_reviews_tx(tx, &self.origin, input)?,
                run_requesters: Vec::new(),
                missions_to_republish: Vec::new(),
                namespace_notice: "Client-derived custom/person namespaces are not automatically moved; no aliases or namespace migrations are created.".into(),
            };
            let mut runs = tx.prepare("SELECT id FROM mission_runs WHERE requester=?1 ORDER BY id").map_err(internal)?;
            for run in runs.query_map([&input.old_person], |row| row.get::<_, String>(0)).map_err(internal)? {
                let run = run.map_err(internal)?;
                if person_work::run_live(tx, &run, None, false).map_err(internal)? {
                    report.run_requesters.push(format!("mission-run/{run}"));
                }
            }
            let mut missions = tx.prepare(
                "SELECT r.body FROM mission_definitions d JOIN mission_revisions r
                 ON r.mission_id=d.mission_id AND r.revision=d.revision ORDER BY d.mission_id").map_err(internal)?;
            for body in missions.query_map([], |row| row.get::<_, String>(0)).map_err(internal)? {
                let mission: MissionSpec = serde_json::from_str(&body.map_err(internal)?).map_err(internal)?;
                if mission_references_reviewer(&mission, &input.old_person) {
                    report.missions_to_republish.push(mission.subject);
                }
            }
            append_claim_tx(tx, &self.origin, &input.old_person, "person.rename.completed", Some(&input.actor),
                &json!({"fields": {"old_person": input.old_person, "new_person": input.new_person, "report": report},
                    "idempotency_key": input.idempotency_key}), &[], None).map_err(claim_append_error)?;
            Ok(report)
        }).map_err(internal)?
    }
}

fn gate_references_reviewer(gate: &GateSpec, person: &str) -> bool {
    matches!(gate, GateSpec::Human { reviewer, .. } if reviewer == person)
}

fn mission_references_reviewer(mission: &MissionSpec, person: &str) -> bool {
    mission.revision_reviewer.as_deref() == Some(person)
        || mission.gates.iter().any(|gate| gate_references_reviewer(gate, person))
        || mission.steps.values().any(|step| {
            step.revision_reviewer.as_deref() == Some(person)
                || step.gates.iter().any(|gate| gate_references_reviewer(gate, person))
                || step.nested_mission.as_deref().is_some_and(|nested| mission_references_reviewer(nested, person))
                || step.loop_spec.as_deref().is_some_and(|loop_spec| {
                    loop_spec.exhaustion_attention.as_ref().is_some_and(|attention| attention.reviewer == person)
                        || loop_spec.until.iter().any(|gate| gate_references_reviewer(gate, person))
                        || loop_spec.candidates.as_ref().is_some_and(|candidates| match &candidates.select {
                            crate::model::LoopCandidateSelector::Human { gate }
                            | crate::model::LoopCandidateSelector::Llm { gate } => gate_references_reviewer(gate, person),
                            _ => false,
                        })
                        || matches!(&loop_spec.on_exhausted, crate::model::LoopExhaustionSpec::Human { gate } if gate_references_reviewer(gate, person))
                        || mission_references_reviewer(&loop_spec.round, person)
                        || loop_spec.on_keep.as_deref().is_some_and(|nested| mission_references_reviewer(nested, person))
                        || loop_spec.on_discard.as_deref().is_some_and(|nested| mission_references_reviewer(nested, person))
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_lists_only_live_requesters_and_published_reviewer_references() {
        let store = Store::open_memory("alder").unwrap();
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
            actor: "agent/alder.operator".into(),
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
        let valid = PersonRenameRequest {
            old_person: "person/avery".into(),
            new_person: "person/robin".into(),
            actor: "person/operator".into(),
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
}
