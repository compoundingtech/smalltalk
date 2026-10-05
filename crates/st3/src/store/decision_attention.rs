//! Attention from bounded source-owned decision values, never private decision documents.
use super::*;
use st3_schema::decision_attention::{NativeAsk, Observation};

pub(crate) struct DecisionAttention {
    pub observation: Observation,
    pub requester: String,
    pub person: String,
    recipient_conflict: bool,
    pub revision: String,
    pub requested_at: u128,
    pub updated_at: u128,
    pub native_asks: BTreeMap<String, NativeAsk>,
    pub conflict: bool,
    pub retained_answer: Option<(String, String)>,
}

impl DecisionAttention {
    fn state(&self) -> &str {
        if self.conflict { "undecidable" } else { &self.observation.state }
    }

    pub(crate) fn item(&self) -> AttentionItemView {
        let mut metadata = serde_json::to_value(&self.observation).expect("decision metadata serializes");
        metadata["state"] = json!(self.state());
        metadata["person"] = json!(self.person);
        metadata["claim_id"] = json!(self.revision);
        metadata["native_asks"] = Value::Array(self.native_asks.values().map(|ask| json!(ask)).collect());
        metadata["source_conflict"] = json!(self.conflict);
        metadata["updated_at_unix_ms"] = json!(self.updated_at);
        if let Some((answer, revision)) = &self.retained_answer {
            metadata["answer_id"] = json!(answer);
            metadata["answer_source_revision"] = json!(revision);
        }
        AttentionItemView {
            episode: format!("{}:{}", self.requester, self.observation.request_id),
            priority: if self.observation.decision_kind == "blocker" { "high" } else { "normal" }.into(),
            kind: "decision".into(), review_mode: None,
            subject: self.observation.decision_id.clone(), person: self.person.clone(),
            requester_id: Some(self.requester.clone()), launch_id: None, variant_id: None, message_id: None,
            title: format!("Q{} needs your answer", self.observation.q),
            detail: format!("Source decision is {}. Axe owns its answer.", self.state()),
            request: Some(metadata), mission: None, mission_run: None, step: None,
            targets: vec![self.requester.clone(), self.observation.decision_id.clone()],
            requested_at_unix_ms: self.requested_at,
            actions: Vec::new(),
        }
    }
}

impl Store {
    pub(crate) fn decision_attention(&self, person: Option<&str>) -> Result<Vec<DecisionAttention>> {
        let connection = self.readers.get();
        let mut query = connection.prepare(&canonical_sql("SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms
            FROM claims WHERE kind='decision.observed'
            AND NOT EXISTS (SELECT 1 FROM replica_records WHERE claim_id=claims.id AND state='repaired')
            ORDER BY CANONICAL_ASC(claims)"))?;
        let claims = query.query_map([], claim_from_row)?;
        let mut decisions = BTreeMap::<(String, String), DecisionAttention>::new();
        for claim in claims {
            let mut claim = claim?;
            if claim.actor.as_deref() != Some(claim.subject.as_str()) { continue; }
            let fields = claim.body.get_mut("fields").map(Value::take).unwrap_or(Value::Null);
            let Ok(observation) = serde_json::from_value::<Observation>(fields) else { continue; };
            let key = (claim.subject.clone(), observation.decision_id.clone());
            let answer = observation.answer_id.as_ref().map(|answer| (answer.clone(), observation.source_revision.clone()));
            match decisions.entry(key) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    let mut native_asks = BTreeMap::new();
                    if let Some(ask) = &observation.native_ask { native_asks.insert(ask.subject.clone(), ask.clone()); }
                    entry.insert(DecisionAttention {
                        person: observation.person.clone(), recipient_conflict: false,
                        observation, requester: claim.subject, revision: claim.id,
                        requested_at: claim.accepted_at_unix_ms, updated_at: claim.accepted_at_unix_ms,
                        native_asks, conflict: false, retained_answer: answer,
                    });
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let current = entry.get_mut();
                    current.requested_at = current.requested_at.min(claim.accepted_at_unix_ms);
                    if let Some(ask) = &observation.native_ask { current.native_asks.insert(ask.subject.clone(), ask.clone()); }
                    current.recipient_conflict |= current.person != observation.person;
                    if answer.is_some() && (current.retained_answer.is_none()
                        || observation.source_sequence >= current.observation.source_sequence)
                    {
                        current.retained_answer = answer;
                    }
                    let lost_answer = current.retained_answer.is_some()
                        && observation.state == "pending" && observation.answer_id.is_none();
                    if observation.source_sequence > current.observation.source_sequence {
                        current.conflict = lost_answer || current.recipient_conflict;
                        current.observation = observation;
                        current.revision = claim.id;
                        current.updated_at = claim.accepted_at_unix_ms;
                    } else if observation.source_sequence == current.observation.source_sequence {
                        current.conflict |= !current.observation.same_source(&observation)
                            || lost_answer || current.recipient_conflict;
                        // Link repair changes the receipt, not the source resolution or card identity.
                        current.updated_at = current.updated_at.max(claim.accepted_at_unix_ms);
                    }
                    current.conflict |= current.recipient_conflict
                        || (current.retained_answer.is_some() && current.observation.state == "pending");
                }
            }
        }
        Ok(decisions.into_values().filter(|decision| person.is_none_or(|person| decision.person == person)).collect())
    }

    pub(crate) fn project_decision_attention(&self, items: &mut Vec<AttentionItemView>, person: Option<&str>) -> Result<()> {
        let decisions = self.decision_attention(person)?;
        items.retain(|item| !decisions.iter().any(|decision| item.kind == "person-step"
            && item.person == decision.person
            && item.requester_id.as_deref() == Some(decision.requester.as_str())
            && decision.native_asks.contains_key(&item.subject)));
        items.extend(decisions.iter().filter(|decision| decision.state() == "pending").map(DecisionAttention::item));
        Ok(())
    }

    pub(crate) fn decision_attention_history(&self, person: Option<&str>) -> Result<Vec<AttentionItemView>> {
        Ok(self.decision_attention(person)?.iter().filter(|decision| decision.state() != "pending").map(DecisionAttention::item).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(sequence: u64, state: &str, answer: Option<&str>) -> Value {
        let mut value = json!({
            "decision_id": format!("resource/axe/decision/{}/q38req", "a".repeat(64)),
            "request_id": "q38req", "q": 38, "source_sequence": sequence,
            "source_revision": format!("{sequence:064x}"), "person": "person/example",
            "decision_kind": "blocker", "state": state, "revived": false,
            "activation": "0000000000000000"
        });
        if let Some(answer) = answer { value["answer_id"] = json!(answer); }
        value
    }

    fn publish(store: &Store, fields: Value) {
        store.append_client_claim(&ClaimInput {
            subject: "agent/example/author".into(), actor: Some("agent/example/author".into()),
            kind: "decision.observed".into(), fields: serde_json::from_value(fields).unwrap(),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
    }

    #[test]
    fn answered_replay_and_partial_source_restore_never_reopen() {
        let store = Store::open_memory("decision-test").unwrap();
        publish(&store, fields(1, "pending", None));
        let card = store.attention_items(Some("person/example")).unwrap().remove(0);
        publish(&store, fields(2, "answered", Some("ans001")));
        publish(&store, fields(1, "pending", None));
        publish(&store, fields(2, "answered", Some("ans001")));
        assert!(store.attention_items(Some("person/example")).unwrap().is_empty());
        let history = store.decision_attention_history(Some("person/example")).unwrap().remove(0);
        assert_eq!(history.episode, card.episode);
        assert_eq!(history.subject, card.subject);
        assert_eq!(history.request.as_ref().unwrap()["state"], "answered");
        assert_eq!(history.request.as_ref().unwrap()["answer_id"], "ans001");
        // Restoring an incomplete source log and appending unrelated records is not an answer reset.
        publish(&store, fields(10, "pending", None));
        assert!(store.attention_items(Some("person/example")).unwrap().is_empty());
        let restored = store.decision_attention_history(Some("person/example")).unwrap().remove(0);
        assert_eq!(restored.request.as_ref().unwrap()["state"], "undecidable");
        assert_eq!(restored.request.as_ref().unwrap()["answer_id"], "ans001");
    }

    #[test]
    fn same_source_link_recovery_is_not_a_semantic_conflict() {
        let store = Store::open_memory("decision-test").unwrap();
        let source = fields(1, "pending", None);
        publish(&store, source.clone());
        let original = store.attention_items(Some("person/example")).unwrap().remove(0);
        let mut recovered = source.clone();
        recovered["native_ask"] = json!({
            "key": format!("axe:decision:v1:{}:1-q38req:ask:0000000000000000", source["decision_id"].as_str().unwrap()),
            "subject": "step-run/example/ask", "run": "mission-run/example/ask",
            "episode": "request-episode", "status": "ready"
        });
        publish(&store, recovered);
        let recovered = store.attention_items(Some("person/example")).unwrap().remove(0);
        assert_eq!(recovered.episode, original.episode);
        assert_eq!(recovered.request.as_ref().unwrap()["state"], "pending");
        assert_eq!(recovered.request.as_ref().unwrap()["source_conflict"], false);
        assert_eq!(recovered.request.as_ref().unwrap()["native_asks"][0]["subject"], "step-run/example/ask");
        // A different source answer at that same version is not link repair.
        publish(&store, fields(1, "answered", Some("ans001")));
        assert!(store.attention_items(Some("person/example")).unwrap().is_empty());
        let conflict = store.decision_attention_history(Some("person/example")).unwrap().remove(0);
        assert_eq!(conflict.request.as_ref().unwrap()["state"], "undecidable");
    }

    #[test]
    fn unanswerable_guard_then_genuine_revival_retains_identity() {
        let store = Store::open_memory("decision-test").unwrap();
        publish(&store, fields(1, "pending", None));
        let first = store.attention_items(Some("person/example")).unwrap().remove(0);
        publish(&store, fields(2, "moot", None));
        assert!(store.attention_items(Some("person/example")).unwrap().is_empty());
        let mut revived = fields(3, "pending", None);
        revived["revived"] = json!(true);
        revived["activation"] = json!("1111111111111111");
        publish(&store, revived);
        let revived = store.attention_items(Some("person/example")).unwrap().remove(0);
        assert_eq!(revived.episode, first.episode);
        assert_eq!(revived.request.as_ref().unwrap()["state"], "pending");
        assert_eq!(revived.request.as_ref().unwrap()["revived"], true);
        assert!(store.attention_items(Some("person/other")).unwrap().is_empty());
    }

    #[test]
    fn recipient_change_neither_moves_the_card_nor_restores_actionability() {
        let store = Store::open_memory("decision-test").unwrap();
        publish(&store, fields(1, "pending", None));
        let first = store.attention_items(Some("person/example")).unwrap().remove(0);
        let mut moved = fields(2, "pending", None);
        moved["person"] = json!("person/other");
        publish(&store, moved);
        assert!(store.attention_items(None).unwrap().is_empty());
        assert!(store.decision_attention_history(Some("person/other")).unwrap().is_empty());
        let history = store.decision_attention_history(Some("person/example")).unwrap().remove(0);
        assert_eq!(history.person, first.person);
        assert_eq!(history.request.as_ref().unwrap()["person"], "person/example");
        assert_eq!(history.request.as_ref().unwrap()["state"], "undecidable");
        publish(&store, fields(3, "pending", None));
        assert!(store.attention_items(None).unwrap().is_empty(), "source recovery cannot silently undo a recipient conflict");
    }
}
