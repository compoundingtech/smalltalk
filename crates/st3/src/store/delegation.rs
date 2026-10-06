//! Bounded recording of a person's prior instruction. The actor remains the agent.
use super::*;
use crate::model::{DelegationPolicyRequest, DelegationProof};

pub const ACTIONS: &[&str] = &["answer-ask", "close-item", "record-go-stop"];

fn denied(message: impl Into<String>) -> St3Error {
    St3Error::new("delegation-refused", message)
}

pub(super) fn policy_actions(fields: &BTreeMap<String, Value>) -> Result<Vec<String>, St3Error> {
    let actions: Vec<String> =
        serde_json::from_value(fields.get("actions").cloned().unwrap_or(Value::Null))
            .map_err(|_| denied("a policy needs an actions array"))?;
    let unique: BTreeSet<_> = actions.iter().collect();
    if unique.len() != actions.len()
        || actions
            .iter()
            .any(|action| !ACTIONS.contains(&action.as_str()))
    {
        return Err(denied(
            "only answer-ask, close-item and record-go-stop may be enabled",
        ));
    }
    Ok(actions)
}

impl Store {
    pub(crate) fn delegated_update_request(
        &self,
        proof: &DelegationProof,
    ) -> Result<ClaimRecord, St3Error> {
        let request = claim_by_id_tx(&self.readers.get(), &proof.episode)
            .map_err(internal)?
            .filter(|claim| {
                claim.kind == "work.person-asked"
                    && claim.body["fields"]["person"] == proof.person
                    && person_work::is_update(claim)
            })
            .ok_or_else(|| {
                denied("only an informational update may be closed through attention")
            })?;
        Ok(request)
    }

    pub(crate) fn delegation_evidence(
        &self,
        evidence: &mut Vec<String>,
        proof: &DelegationProof,
    ) -> Result<(), St3Error> {
        add_evidence(&self.readers.get(), evidence, proof)
    }
    pub fn set_delegation_policy(
        &self,
        input: &DelegationPolicyRequest,
    ) -> Result<ClaimRecord, St3Error> {
        self.append_claim(&ClaimInput {
            subject: input.person.clone(),
            kind: "person.delegation-set".into(),
            actor: Some(input.actor.clone()),
            fields: BTreeMap::from([("actions".into(), json!(input.actions))]),
            evidence: input.evidence.clone(),
            expected_subject: None,
            idempotency_key: Some(input.idempotency_key.clone()),
        })
    }
}

pub(crate) fn add_fields(fields: &mut BTreeMap<String, Value>, proof: &DelegationProof) {
    fields.insert("acted_for".into(), json!(proof.person));
    fields.insert("delegation".into(), json!(proof));
}

pub(super) fn add_evidence(
    connection: &Connection,
    evidence: &mut Vec<String>,
    proof: &DelegationProof,
) -> Result<(), St3Error> {
    let instruction = message_sent(connection, &proof.message)?;
    for reference in [&instruction.id, &proof.policy] {
        if !evidence.contains(reference) {
            evidence.push(reference.clone());
        }
    }
    Ok(())
}

fn message_sent(connection: &Connection, subject: &str) -> Result<ClaimRecord, St3Error> {
    connection.query_row(&canonical_sql("SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM claims WHERE subject=?1 AND kind='message.sent' ORDER BY CANONICAL_ASC(claims) LIMIT 1"), [subject], claim_from_row)
        .optional().map_err(internal)?.ok_or_else(|| denied("instruction and target messages need an original sent claim"))
}

/// Runs under the writer lock on both specialized and generic local claim paths.
pub(super) fn validate_write(
    tx: &Transaction<'_>,
    subject: &str,
    kind: &str,
    actor: Option<&str>,
    fields: &BTreeMap<String, Value>,
    evidence: &[String],
) -> Result<(), St3Error> {
    if kind == "person.delegation-set" {
        if !subject.starts_with("person/")
            || subject.matches('/').count() != 1
            || actor != Some(subject)
        {
            return Err(denied(
                "only the person may establish or replace their delegation policy",
            ));
        }
        policy_actions(fields)?;
        if evidence.is_empty() {
            return Err(denied("a policy needs its decision evidence"));
        }
        return Ok(());
    }
    let Some(value) = fields.get("delegation") else {
        if fields.contains_key("acted_for")
            || (kind == "work.person-done"
                && actor.is_some_and(|actor| actor.starts_with("agent/")))
        {
            return Err(denied(
                "an agent completing person work needs explicit delegation evidence",
            ));
        }
        return Ok(());
    };
    let proof: DelegationProof =
        serde_json::from_value(value.clone()).map_err(|_| denied("invalid delegation proof"))?;
    if !actor.is_some_and(|actor| actor.starts_with("agent/"))
        || fields.get("acted_for") != Some(&json!(proof.person))
    {
        return Err(denied(
            "a delegated action records an agent actor and the person acted for",
        ));
    }
    let policy = tx.query_row(&canonical_sql("SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM claims WHERE subject=?1 AND kind='person.delegation-set' ORDER BY CANONICAL_DESC(claims) LIMIT 1"), [&proof.person], claim_from_row)
        .optional().map_err(internal)?.ok_or_else(|| denied("the person has no delegation policy"))?;
    if policy.id != proof.policy || policy.actor.as_deref() != Some(&proof.person) {
        return Err(denied(
            "the policy changed or was not established by this person",
        ));
    }
    let action = match kind {
        "work.person-done" => {
            let mut step = person_work::step(tx, subject)
                .map_err(internal)?
                .ok_or_else(|| denied("missing person step"))?;
            let ask = person_work::request(tx, subject)
                .map_err(internal)?
                .ok_or_else(|| denied("authored person steps are outside the allowed list"))?;
            if step.assigned_to.as_deref() != Some(&proof.person)
                || proof.episode != ask.id
                || fields.get("episode") != Some(&json!(ask.id))
                || !person_work::current(tx, &ask, now_ms()).map_err(internal)?
            {
                return Err(denied("the ask belongs to another person or episode"));
            }
            apply_effective_step_state(tx, &mut step, now_ms()).map_err(internal)?;
            if step.status != "ready"
                || fields.get("attempt").and_then(Value::as_u64) != Some(step.attempt as u64)
                || super::adhoc_work::pending_handoff(tx, &step)?
            {
                return Err(denied(
                    "the person step is not ready or its handoff is unread",
                ));
            }
            // Generic claim callers receive the same structured answer validation.
            person_work::validate_delegated_answer(&ask, fields)?;
            if person_work::is_update(&ask) {
                "close-item"
            } else {
                "answer-ask"
            }
        }
        "gate.result" => {
            if fields.get("decision").and_then(Value::as_str) == Some("rejected")
                && fields
                    .get("reason")
                    .and_then(Value::as_str)
                    .is_none_or(|reason| reason.trim().is_empty())
            {
                return Err(denied(
                    "a delegated stop decision needs its rejection reason",
                ));
            }
            let request_id = fields
                .get("request")
                .and_then(Value::as_str)
                .ok_or_else(|| denied("a delegated review needs its exact request"))?;
            let request = claim_by_id_tx(tx, request_id)
                .map_err(internal)?
                .ok_or_else(|| denied("missing review request"))?;
            let review = current_human_review(tx, request.clone())
                .map_err(internal)?
                .ok_or_else(|| denied("the review revision is stale"))?;
            if request.kind != "gate.requested"
                || request.subject != subject
                || review.reviewer != proof.person
                || proof.episode != request.id
                || review.mode != "approve"
                || human_review_answer_tx(tx, &request.id)
                    .map_err(internal)?
                    .is_some()
                || !matches!(
                    (
                        fields.get("decision").and_then(Value::as_str),
                        fields.get("verdict").and_then(Value::as_str)
                    ),
                    (Some("approved"), Some("pass")) | (Some("rejected"), Some("fail"))
                )
            {
                return Err(denied(
                    "only an unanswered go/stop review for this person and revision may be delegated",
                ));
            }
            "record-go-stop"
        }
        "message.closed" => {
            let target = message_sent(tx, subject)?;
            if target.body["fields"]["to"] != proof.person || proof.episode != target.id {
                return Err(denied("the message belongs to another person or episode"));
            }
            "close-item"
        }
        _ => {
            return Err(denied(
                "this operation is outside the allowed delegation list",
            ));
        }
    };
    let actions: Vec<String> =
        serde_json::from_value(policy.body["fields"]["actions"].clone()).map_err(internal)?;
    if !actions.iter().any(|allowed| allowed == action) {
        return Err(denied(format!("the person has not enabled {action}")));
    }
    let instruction = message_sent(tx, &proof.message)?;
    if instruction.actor.as_deref() != Some(&proof.person)
        || instruction.body["fields"]["from"] != proof.person
    {
        return Err(denied(
            "the instruction's recorded sender must be the person acted for",
        ));
    }
    let mut content = instruction.body["fields"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if content.starts_with("doc/") {
        let (name, hash) = split_document_ref(&content)?;
        let bytes: Vec<u8> = tx.query_row("SELECT b.bytes FROM documents d JOIN blobs b ON b.hash=d.hash WHERE d.name=?1 AND d.hash=?2", params![name,hash], |row| row.get(0)).map_err(internal)?;
        content = String::from_utf8(bytes).map_err(internal)?;
    }
    if proof.quote.trim().is_empty() || !content.contains(&proof.quote) {
        return Err(denied(
            "the quote must occur verbatim in the person's original message",
        ));
    }
    if !evidence.contains(&instruction.id) || !evidence.contains(&proof.policy) {
        return Err(denied(
            "the completion must cite the instruction and policy",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PersonStepResponse;

    fn sent(store: &Store, id: &str, from: &str, to: &str, text: &str) -> ClaimRecord {
        store
            .append_claim(&ClaimInput {
                subject: format!("message/{id}"),
                kind: "message.sent".into(),
                actor: Some(from.into()),
                fields: BTreeMap::from([
                    ("from".into(), json!(from)),
                    ("to".into(), json!(to)),
                    ("content".into(), json!(text)),
                    ("status".into(), json!("sent")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some(id.into()),
            })
            .unwrap()
    }

    fn policy(store: &Store, actions: &[&str], key: &str) -> ClaimRecord {
        let decision = sent(
            store,
            &format!("policy-{key}"),
            "person/avery",
            "agent/alder.asker",
            "Use this delegation list.",
        );
        store
            .set_delegation_policy(&DelegationPolicyRequest {
                person: "person/avery".into(),
                actor: "person/avery".into(),
                actions: actions.iter().map(|action| (*action).into()).collect(),
                evidence: vec![decision.id],
                idempotency_key: key.into(),
            })
            .unwrap()
    }

    fn proof(store: &Store, actions: &[&str], episode: String) -> DelegationProof {
        let policy = policy(store, actions, "allow");
        sent(
            store,
            "instruction",
            "person/avery",
            "agent/alder.asker",
            "Friday. Archive that message and read the update. Approve the release. Stop the release.",
        );
        DelegationProof {
            person: "person/avery".into(),
            policy: policy.id,
            message: "message/instruction".into(),
            quote: "Friday".into(),
            episode,
        }
    }

    #[test]
    fn delegated_ask_records_actor_person_instruction_and_replays() {
        let (store, origin, input) = person_work::tests::fixture();
        let ask = store.ask_person(&input).unwrap();
        let episode = store
            .claims_for(&ask.subject, Some("work.person-asked"))
            .unwrap()[0]
            .id
            .clone();
        let proof = proof(&store, ACTIONS, episode.clone());
        let mut response = PersonStepResponse {
            subject: ask.subject.clone(),
            actor: input.actor.clone(),
            summary: "Friday".into(),
            evidence: vec![],
            episode: Some(episode),
            answer: None,
            idempotency_key: "answer".into(),
            delegation: Some(proof.clone()),
        };
        assert_eq!(
            store.finish_person_step(&response, false).unwrap().status,
            "completed"
        );
        let record = &store
            .claims_for(&ask.subject, Some("work.person-done"))
            .unwrap()[0];
        assert_eq!(record.actor.as_deref(), Some(input.actor.as_str()));
        assert_eq!(record.body["fields"]["acted_for"], "person/avery");
        assert_eq!(record.body["fields"]["delegation"], json!(proof));
        assert_eq!(
            record.body["evidence"],
            json!([
                message_sent(&store.readers.get(), "message/instruction")
                    .unwrap()
                    .id,
                proof.policy
            ])
        );
        let resumed = store.step_run(&origin.subject).unwrap().unwrap();
        assert_eq!(resumed.status, "ready");
        assert_eq!(resumed.person_answers[0].respondent, input.actor);
        assert_eq!(
            resumed.person_answers[0].acted_for.as_deref(),
            Some("person/avery")
        );
        let peer = Store::open_memory("birch").unwrap();
        peer.project_replication_backlog().unwrap();
        person_work::tests::receive(&store, &peer);
        let replicated = peer.claim_by_id(&record.id).unwrap().unwrap();
        assert_eq!(replicated.actor, record.actor);
        assert_eq!(replicated.body, record.body);
        assert_eq!(
            peer.step_run(&origin.subject)
                .unwrap()
                .unwrap()
                .person_answers[0]
                .acted_for
                .as_deref(),
            Some("person/avery")
        );
        assert_eq!(
            store.finish_person_step(&response, false).unwrap().status,
            "completed"
        );
        response.delegation.as_mut().unwrap().quote = "Archive that message".into();
        assert_eq!(
            store.finish_person_step(&response, false).unwrap_err().code,
            "idempotency-conflict"
        );
    }

    #[test]
    fn delegated_document_instruction_quotes_the_original_pinned_body() {
        let (store, _, input) = person_work::tests::fixture();
        let ask = store.ask_person(&input).unwrap();
        let episode = store
            .claims_for(&ask.subject, Some("work.person-asked"))
            .unwrap()[0]
            .id
            .clone();
        let mut proof = proof(&store, ACTIONS, episode.clone());
        let document = store
            .put_document(
                "doc/example/instruction",
                b"Release Friday.",
                &None,
                "friday",
            )
            .unwrap();
        sent(
            &store,
            "document-instruction",
            "person/avery",
            "agent/alder.asker",
            &format!("{}@{}", document.name, document.hash),
        );
        store
            .put_document(
                "doc/example/instruction",
                b"Release Monday.",
                &Some(document.binding_claim_id),
                "monday",
            )
            .unwrap();
        proof.message = "message/document-instruction".into();
        let response = PersonStepResponse {
            subject: ask.subject.clone(),
            actor: input.actor,
            summary: "Friday".into(),
            evidence: vec![],
            episode: Some(episode),
            delegation: Some(proof),
            answer: None,
            idempotency_key: "document-answer".into(),
        };
        assert_eq!(
            store.finish_person_step(&response, false).unwrap().status,
            "completed"
        );
    }

    #[test]
    fn delegated_ask_refuses_bad_evidence_revoked_policy_and_authored_steps() {
        for invalid in [
            "absent-policy",
            "forged-quote",
            "wrong-person",
            "wrong-message",
            "stale-episode",
            "revoked",
            "authored-step",
            "cancel",
        ] {
            let (store, origin, input) = person_work::tests::fixture();
            let ask = store.ask_person(&input).unwrap();
            let episode = store
                .claims_for(&ask.subject, Some("work.person-asked"))
                .unwrap()[0]
                .id
                .clone();
            let mut proof = proof(&store, ACTIONS, episode.clone());
            let mut subject = ask.subject.clone();
            match invalid {
                "absent-policy" => proof.policy = "missing".into(),
                "forged-quote" => proof.quote = "Monday".into(),
                "wrong-person" => proof.person = "person/robin".into(),
                "wrong-message" => {
                    sent(
                        &store,
                        "fake",
                        "agent/alder.asker",
                        "person/avery",
                        "Friday",
                    );
                    proof.message = "message/fake".into();
                }
                "stale-episode" => proof.episode = "old".into(),
                "revoked" => {
                    policy(&store, &[], "revoke");
                }
                "authored-step" => {
                    subject = store
                        .mission_run(&origin.run)
                        .unwrap()
                        .unwrap()
                        .steps
                        .iter()
                        .find(|step| step.step == "review")
                        .unwrap()
                        .subject
                        .clone()
                }
                _ => {}
            }
            let response = PersonStepResponse {
                subject,
                actor: input.actor.clone(),
                summary: "Friday".into(),
                evidence: vec![],
                episode: Some(episode),
                answer: None,
                idempotency_key: "answer".into(),
                delegation: Some(proof),
            };
            assert!(
                store
                    .finish_person_step(&response, invalid == "cancel")
                    .is_err(),
                "{invalid}"
            );
            assert_eq!(
                store.step_run(&ask.subject).unwrap().unwrap().status,
                "ready",
                "{invalid}"
            );
            assert_eq!(
                store.step_run(&origin.subject).unwrap().unwrap().status,
                "waiting-person",
                "{invalid}"
            );
            assert!(
                store
                    .claims_for(&ask.subject, Some("work.person-done"))
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn delegated_message_closure_refuses_operations_outside_list_and_generic_forgery() {
        let (store, _, _) = person_work::tests::fixture();
        let target = sent(
            &store,
            "notice",
            "agent/alder.asker",
            "person/avery",
            "Notice",
        );
        let proof = proof(&store, &["answer-ask"], target.id.clone());
        let mut fields = BTreeMap::from([("status".into(), json!("closed"))]);
        add_fields(&mut fields, &proof);
        let mut evidence = vec![];
        add_evidence(&store.readers.get(), &mut evidence, &proof).unwrap();
        let mut input = ClaimInput {
            subject: target.subject.clone(),
            kind: "message.closed".into(),
            actor: Some("agent/alder.asker".into()),
            fields,
            evidence,
            expected_subject: None,
            idempotency_key: Some("close".into()),
        };
        assert_eq!(
            store.append_claim(&input).unwrap_err().code,
            "delegation-refused"
        );
        let policy = policy(&store, ACTIONS, "all");
        let mut allowed = proof.clone();
        allowed.policy = policy.id;
        allowed.quote = "Archive that message".into();
        add_fields(&mut input.fields, &allowed);
        input.evidence = vec![];
        add_evidence(&store.readers.get(), &mut input.evidence, &allowed).unwrap();
        assert_eq!(
            store.append_claim(&input).unwrap().actor.as_deref(),
            Some("agent/alder.asker")
        );
        assert_eq!(
            store.message(&target.subject).unwrap().unwrap().status,
            "closed"
        );
        assert_eq!(
            store.append_claim(&input).unwrap().body["fields"]["acted_for"],
            "person/avery"
        );
        let mut forged = input.clone();
        forged.subject = "message/other".into();
        forged.idempotency_key = Some("other".into());
        assert!(store.append_claim(&forged).is_err());
        // Supplying the same metadata on a deletion or external operation cannot authorize it.
        forged.kind = "glass.deleted".into();
        forged.subject = "glass/example".into();
        assert!(store.append_claim(&forged).is_err());
    }

    #[test]
    fn delegated_structured_ask_keeps_named_answer_validation() {
        let (store, _, mut input) = person_work::tests::fixture();
        input.request = Some(
            json!({"version":1,"type":"choice","question":"Choose a date","why_person":"Your date","answers":[{"id":"friday","label":"Friday","consequence":"Release Friday"},{"id":"monday","label":"Monday","consequence":"Release Monday"}]}),
        );
        let ask = store.ask_person(&input).unwrap();
        let episode = store
            .claims_for(&ask.subject, Some("work.person-asked"))
            .unwrap()[0]
            .id
            .clone();
        let proof = proof(&store, ACTIONS, episode.clone());
        let mut response = PersonStepResponse {
            subject: ask.subject.clone(),
            actor: input.actor,
            summary: String::new(),
            evidence: vec![],
            episode: Some(episode),
            delegation: Some(proof),
            answer: Some(crate::person_request::AnswerInput {
                id: Some("not-offered".into()),
                text: None,
            }),
            idempotency_key: "choice".into(),
        };
        assert!(store.finish_person_step(&response, false).is_err());
        response.answer.as_mut().unwrap().id = Some("friday".into());
        store.finish_person_step(&response, false).unwrap();
        let done = store.step_run(&ask.subject).unwrap().unwrap();
        assert_eq!(
            done.person_answers[0].answer.as_ref().unwrap()["id"],
            "friday"
        );
        assert_eq!(
            done.person_answers[0].acted_for.as_deref(),
            Some("person/avery")
        );
    }

    #[test]
    fn delegated_closure_reads_updates_without_answering_decisions() {
        let (store, origin, mut input) = person_work::tests::fixture();
        input.step = None;
        input.incarnation = None;
        input.request = Some(json!({"version":1,"type":"update","about":origin.run}));
        let update = store.ask_person(&input).unwrap();
        let episode = store
            .claims_for(&update.subject, Some("work.person-asked"))
            .unwrap()[0]
            .id
            .clone();
        let mut proof = proof(&store, &["close-item"], episode.clone());
        proof.quote = "read the update".into();
        let response = PersonStepResponse {
            subject: update.subject.clone(),
            actor: input.actor.clone(),
            summary: "Read".into(),
            evidence: vec![],
            episode: Some(episode),
            delegation: Some(proof.clone()),
            answer: None,
            idempotency_key: "read-update".into(),
        };
        assert_eq!(
            store.finish_person_step(&response, false).unwrap().status,
            "completed"
        );
        let mut decision_input = input;
        decision_input.request = None;
        decision_input.step = Some(origin.subject.clone());
        decision_input.incarnation = Some("asker-one".into());
        decision_input.idempotency_key = "date".into();
        let ask = store.ask_person(&decision_input).unwrap();
        let ask_episode = store
            .claims_for(&ask.subject, Some("work.person-asked"))
            .unwrap()[0]
            .id
            .clone();
        let mut bypass = response;
        bypass.subject = ask.subject.clone();
        bypass.episode = Some(ask_episode.clone());
        bypass.delegation.as_mut().unwrap().episode = ask_episode;
        bypass.idempotency_key = "bypass".into();
        assert_eq!(
            store.finish_person_step(&bypass, false).unwrap_err().code,
            "delegation-refused"
        );
        assert_eq!(
            store.step_run(&ask.subject).unwrap().unwrap().status,
            "ready"
        );
    }

    #[test]
    fn delegated_go_stop_answers_gate_for_real_reviewer_and_refuses_stale_revision() {
        for verdict in ["pass", "fail", "stale"] {
            let (store, origin, _) = person_work::tests::fixture();
            let run = store.mission_run(&origin.run).unwrap().unwrap();
            let gate = store
                .append_claim(&ClaimInput {
                    subject: "gate-operation/review".into(),
                    kind: "gate.requested".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("owner".into(), json!(origin.subject)),
                        ("reviewer".into(), json!("person/avery")),
                        ("mode".into(), json!("approve")),
                        ("mission_revision".into(), json!(run.revision)),
                        ("step_definition".into(), json!(origin.definition_hash)),
                        ("attempt".into(), json!(1)),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: Some("gate".into()),
                })
                .unwrap();
            let mut proof = proof(&store, ACTIONS, gate.id.clone());
            proof.quote = if verdict == "fail" {
                "Stop the release"
            } else {
                "Approve the release"
            }
            .into();
            let mut fields = BTreeMap::from([
                ("request".into(), json!(gate.id)),
                (
                    "reason".into(),
                    json!("The person instructed this decision"),
                ),
                (
                    "verdict".into(),
                    json!(if verdict == "fail" { "fail" } else { "pass" }),
                ),
                (
                    "decision".into(),
                    json!(if verdict == "fail" {
                        "rejected"
                    } else {
                        "approved"
                    }),
                ),
            ]);
            add_fields(&mut fields, &proof);
            let mut evidence = vec![gate.id.clone()];
            add_evidence(&store.readers.get(), &mut evidence, &proof).unwrap();
            let result = ClaimInput {
                subject: gate.subject.clone(),
                kind: "gate.result".into(),
                actor: Some("agent/alder.asker".into()),
                fields,
                evidence,
                expected_subject: None,
                idempotency_key: Some("go".into()),
            };
            if verdict == "stale" {
                store
                    .connection
                    .batched(|tx| -> Result<()> {
                        tx.execute(
                            "UPDATE step_runs SET definition_hash='changed' WHERE subject=?1",
                            [&origin.subject],
                        )?;
                        Ok(())
                    })
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    store.append_claim(&result).unwrap_err().code,
                    "delegation-refused"
                );
                assert!(store.human_review_answer(&gate.id).unwrap().is_none());
            } else {
                let accepted = store.append_claim(&result).unwrap();
                assert_eq!(
                    store.human_review_answer(&gate.id).unwrap().unwrap().id,
                    accepted.id
                );
                assert!(
                    store
                        .pending_human_reviews(Some("person/avery"))
                        .unwrap()
                        .is_empty()
                );
                assert_eq!(accepted.actor.as_deref(), Some("agent/alder.asker"));
                assert_eq!(accepted.body["fields"]["acted_for"], "person/avery");
            }
        }
    }

    #[test]
    fn policy_can_only_be_replaced_by_person_and_raw_completion_needs_proof() {
        let (store, _, input) = person_work::tests::fixture();
        let decision = sent(
            &store,
            "decision",
            "person/avery",
            "agent/alder.asker",
            "Allow answers",
        );
        let mut request = DelegationPolicyRequest {
            person: "person/avery".into(),
            actor: input.actor.clone(),
            actions: vec!["answer-ask".into()],
            evidence: vec![decision.id],
            idempotency_key: "policy".into(),
        };
        assert_eq!(
            store.set_delegation_policy(&request).unwrap_err().code,
            "delegation-refused"
        );
        request.actor = "person/avery".into();
        request.actions = vec!["delete".into()];
        assert_eq!(
            store.set_delegation_policy(&request).unwrap_err().code,
            "delegation-refused"
        );
        let ask = store.ask_person(&input).unwrap();
        let raw = ClaimInput {
            subject: ask.subject.clone(),
            kind: "work.person-done".into(),
            actor: Some(input.actor),
            fields: BTreeMap::from([
                ("attempt".into(), json!(1)),
                ("status".into(), json!("completed")),
                ("summary".into(), json!("Friday")),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some("raw".into()),
        };
        assert_eq!(
            store.append_claim(&raw).unwrap_err().code,
            "delegation-refused"
        );
        assert_eq!(
            store.step_run(&ask.subject).unwrap().unwrap().status,
            "ready"
        );
    }
}
