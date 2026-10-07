//! Native prompt sources, separate from mission steps. Exact actions stay on the owning host.
use super::*;
use st_drivers::prompts::{Answer, Prompt};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS local_harness_prompts (
 episode TEXT PRIMARY KEY, seat TEXT NOT NULL, person TEXT, runtime TEXT NOT NULL,
 prompt_id TEXT NOT NULL, body TEXT NOT NULL, state TEXT NOT NULL,
 requested_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, resolved_at INTEGER,
 answer TEXT, operation TEXT, attention_id TEXT, resolved_sequence INTEGER,
 UNIQUE(seat,runtime,prompt_id,episode)
);
CREATE INDEX IF NOT EXISTS local_harness_prompts_person ON local_harness_prompts(person,state,requested_at);
CREATE INDEX IF NOT EXISTS local_harness_prompts_runtime ON local_harness_prompts(seat,runtime,state);
CREATE INDEX IF NOT EXISTS local_harness_prompts_expiry ON local_harness_prompts(expires_at) WHERE state IN ('open','submitted');
CREATE INDEX IF NOT EXISTS local_harness_prompts_closed ON local_harness_prompts(person,resolved_at DESC,attention_id DESC) WHERE state NOT IN ('open','submitted');
CREATE INDEX IF NOT EXISTS local_harness_prompts_attention ON local_harness_prompts(attention_id,person);
";

/// Resolve ownership under the same writer lock that admits the source/action.
fn owner(connection: &Connection, seat: &str) -> Result<Option<String>> {
    let mut actor = seat.to_owned();
    let mut seen = BTreeSet::new();
    for _ in 0..16 {
        if actor.starts_with("person/") {
            return Ok(Some(actor));
        }
        if !seen.insert(actor.clone()) {
            return Ok(None);
        }
        let desired = connection.query_row(
            "SELECT desired.subject,desired.kind,desired.body,desired.member,desired.owner_run,desired.owner_generation,desired.owner_step,claims.actor
             FROM desired LEFT JOIN claims ON claims.id=desired.claim_id WHERE desired.subject=?1",
            [&actor], |row| Ok((desired_from_row(row)?, row.get::<_, Option<String>>(7)?)),
        ).optional()?;
        let Some((desired, writer)) = desired else {
            return Ok(None);
        };
        if actor == seat {
            let person = match crate::accounts::harness_binding(&desired.desired)
                .map(|binding| binding.binding)
            {
                Some(crate::accounts::Binding::Pool(person)) => Some(person),
                Some(crate::accounts::Binding::Account(account)) => connection
                    .query_row(
                        "SELECT body FROM desired WHERE subject=?1",
                        [format!("account/{account}")],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .and_then(|body| serde_json::from_str::<Value>(&body).ok())
                    .and_then(|body| {
                        crate::accounts::parse_account(&format!("account/{account}"), &body)
                    })
                    .and_then(|account| account.owner),
                None => None,
            };
            if person
                .as_deref()
                .is_some_and(|person| person.starts_with("person/"))
            {
                return Ok(person);
            }
        }
        let requester = desired
            .owner_run
            .as_deref()
            .map(|run| {
                connection
                    .query_row(
                        "SELECT requester FROM mission_runs WHERE id=?1",
                        [run.strip_prefix("mission-run/").unwrap_or(run)],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
            })
            .transpose()?
            .flatten();
        let Some(next) = requester.or(writer) else {
            return Ok(None);
        };
        actor = next;
    }
    Ok(None)
}

pub(super) fn observe(
    tx: &Transaction<'_>,
    input: &ClaimInput,
    at: u128,
    sequence: i64,
) -> Result<(), St3Error> {
    let mut prompt: Prompt =
        serde_json::from_value(input.fields["prompt"].clone()).map_err(internal)?;
    prompt.validate().map_err(|_| {
        St3Error::new(
            "invalid-prompt-source",
            "native prompt is malformed or exceeds capture bounds",
        )
    })?;
    if prompt.runtime_incarnation != input.fields["incarnation_id"] {
        return Err(St3Error::new(
            "invalid-permission-source",
            "prompt runtime differs from native envelope",
        ));
    }
    let old: Option<String> = tx
        .query_row(
            "SELECT body FROM local_harness_prompts WHERE episode=?1",
            [&prompt.episode],
            |row| row.get(0),
        )
        .optional()
        .map_err(internal)?;
    if let Some(old) = old {
        let old: Prompt = serde_json::from_str(&old).map_err(internal)?;
        if old.runtime_incarnation != prompt.runtime_incarnation
            || old.prompt_id != prompt.prompt_id
            || old.session_id != prompt.session_id
            || old.harness != prompt.harness
            || old.content != prompt.content
            || old.incarnation != prompt.incarnation
            || old.expires_at_ms != prompt.expires_at_ms
        {
            return Err(St3Error::new(
                "permission-identity-conflict",
                "permission episode identity changed",
            ));
        }
        // Later observations cannot reopen an answered, expired or superseded episode.
        tx.execute(
            "UPDATE local_harness_prompts SET body=?2,state=?3,resolved_at=?4,resolved_sequence=?5
            WHERE episode=?1 AND state IN ('open','submitted') AND ?3!='open'",
            params![
                prompt.episode,
                serde_json::to_string(&prompt).map_err(internal)?,
                prompt.state,
                at.min(i64::MAX as u128) as i64,
                sequence
            ],
        )
        .map_err(internal)?;
        return Ok(());
    }
    let person = owner(tx, &input.subject).map_err(internal)?;
    if person.is_none() && prompt.state == "open" {
        // This is routing unavailability, not a provider answer or timeout. No guessed
        // operator receives the private content or a native response capability.
        prompt.state = "unavailable".into();
        prompt.disposition = Some("owner-unavailable".into());
        prompt.endpoint = None;
        prompt.capability = "unavailable".into();
        prompt.how = Some("The requesting seat has no declared owning person.".into());
        prompt.next_action = Some(
            "Declare the seat's owning person, or answer in the native harness dialog.".into(),
        );
        tracing::warn!(seat=%input.subject,"native prompt cannot be routed: declare the seat's owning person or answer in the native harness dialog");
    }
    let attention_id = person
        .as_deref()
        .map(|person| {
            crate::api::client_attention_id(
                &format!("prompt/{}", prompt.episode),
                person,
                &prompt.episode,
            )
        })
        .transpose()
        .map_err(internal)?;
    tx.execute("INSERT INTO local_harness_prompts(episode,seat,person,runtime,prompt_id,body,state,requested_at,expires_at,resolved_at,attention_id,resolved_sequence)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![prompt.episode,input.subject,person,prompt.runtime_incarnation,prompt.prompt_id,
            serde_json::to_string(&prompt).map_err(internal)?,prompt.state,at.min(i64::MAX as u128) as i64,
            prompt.expires_at_ms, (prompt.state!="open").then_some(at.min(i64::MAX as u128) as i64),attention_id,(prompt.state!="open").then_some(sequence)],
    ).map_err(internal)?;
    Ok(())
}

/// Positive runtime/declaration transitions revoke this seat's live sources in the writer
/// transaction. Disappearance is labelled unavailable; it never becomes a denial or timeout.
pub(super) fn runtime_changed(
    tx: &Transaction<'_>,
    origin: &str,
    seat: &str,
    at: u128,
) -> Result<()> {
    let rows = {
        let mut query=tx.prepare("SELECT body FROM local_harness_prompts WHERE seat=?1 AND state IN ('open','submitted') LIMIT 256")?;
        query
            .query_map([seat], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for body in rows {
        let mut prompt: Prompt = serde_json::from_str(&body)?;
        if check_harness_event_runtime(tx, seat, Some(&prompt.runtime_incarnation)).is_err()
            || !person_work::declaration_live(tx, seat)?
        {
            prompt.state = "ended".into();
            prompt.disposition = Some("ended".into());
            prompt.how = Some("The requesting runtime stopped or was replaced.".into());
            prompt.endpoint = None;
            prompt.at_ms = Some(at.min(u64::MAX as u128) as u64);
            let input = ClaimInput {
                subject: seat.into(),
                kind: "harness.prompt".into(),
                actor: Some(seat.into()),
                fields: BTreeMap::from([
                    ("incarnation_id".into(), json!(prompt.runtime_incarnation)),
                    ("prompt".into(), json!(prompt)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("prompt-ended:{}", prompt.episode)),
            };
            insert_local_observation_tx(tx, origin, &input, at).map_err(anyhow::Error::new)?;
        }
    }
    Ok(())
}

type PromptHistoryCandidate = ((i64, String), Option<Value>);

impl Store {
    pub(crate) fn prompt_history_item(
        &self,
        person: &str,
        id: &str,
        at: u128,
    ) -> Result<Option<Value>> {
        let resolved:Option<i64>=self.readers.get().query_row("SELECT resolved_at FROM local_harness_prompts WHERE attention_id=?1 AND person=?2 AND state NOT IN ('open','submitted')",params![id,person],|row|row.get(0)).optional()?;
        let Some(resolved) = resolved else {
            return Ok(None);
        };
        let next = self.prompt_history_next(
            person,
            self.prompt_history_cut()?,
            at,
            Some(&(resolved, format!("{id}\0"))),
        )?;
        Ok(next
            .filter(|((_, found), _)| found == id)
            .and_then(|(_, item)| item))
    }

    pub(crate) fn prompt_history_cut(&self) -> Result<u64> {
        Ok(self.readers.get().query_row(
            "SELECT COALESCE(MAX(id),0) FROM local_observations",
            [],
            |row| row.get(0),
        )?)
    }

    /// One consumed candidate advances only the prompt frontier. The terminal row is
    /// immutable and bounded by its local observation sequence, independent of graph cut.
    pub(crate) fn prompt_history_next(
        &self,
        person: &str,
        cut: u64,
        at: u128,
        after: Option<&(i64, String)>,
    ) -> Result<Option<PromptHistoryCandidate>> {
        let connection = self.readers.get();
        let (ms, id) = after.cloned().unwrap_or((i64::MAX, "\u{10ffff}".into()));
        let row=connection.query_row("SELECT seat,body,requested_at,resolved_at,attention_id,answer FROM local_harness_prompts
            WHERE person=?1 AND state NOT IN ('open','submitted') AND resolved_sequence<=?2 AND resolved_at<=?3
              AND (resolved_at,attention_id)<(?4,?5) ORDER BY resolved_at DESC,attention_id DESC LIMIT 1",
            params![person,cut,at.min(i64::MAX as u128) as i64,ms,id],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,u64>(2)?,row.get::<_,i64>(3)?,row.get::<_,String>(4)?,row.get::<_,Option<String>>(5)?))).optional()?;
        let Some((seat, body, requested, resolved, id, answer)) = row else {
            return Ok(None);
        };
        let key = (resolved, id.clone());
        if owner(&connection, &seat)?.as_deref() != Some(person) {
            return Ok(Some((key, None)));
        }
        let prompt: Prompt = serde_json::from_str(&body)?;
        let metadata = self
            .prompt_metadata(&prompt.episode, person)?
            .context("prompt history metadata missing")?;
        let resolution_kind =
            if prompt.state == "answered" && answer.is_some() && answer == prompt.disposition {
                "answered"
            } else if prompt.state == "cancelled" {
                "cancelled"
            } else {
                "closed"
            };
        let mut resolution =
            json!({"kind":resolution_kind,"at":crate::api::client_timestamp(resolved as u128)});
        if let Some(how) = &prompt.how {
            resolution["reason"] = json!(how);
        }
        if resolution_kind == "answered"
            && let Some(label) = prompt
                .choices
                .iter()
                .find(|choice| Some(&choice.id) == answer.as_ref())
                .map(|choice| &choice.label)
        {
            resolution["answer_label"] = json!(label);
        }
        let item = json!({"id":id,"kind":"attention","attention_kind":"harness-prompt","source_kind":"harness-prompt",
            "source_id":format!("prompt/{}",prompt.episode),"person_id":person,"episode":prompt.episode,"revision":prompt.episode,
            "requested_at":crate::api::client_timestamp(requested.into()),"updated_at":crate::api::client_timestamp(resolved as u128),
            "title":format!("{} {} prompt",prompt.harness,prompt.kind),"detail":prompt.reason,"state":"resolved","priority":"high",
            "requester_id":seat,"targets":[seat],"actions":[],"prompt":metadata,"resolution":resolution,
            "operational":{"layer":"history","actionable":false,"reasons":["native-prompt-ended"]}});
        Ok(Some((key, Some(item))))
    }

    pub(crate) fn prompt_items(
        &self,
        person: Option<&str>,
        at: u128,
    ) -> Result<Vec<AttentionItemView>> {
        // An unscoped overview must never reveal commands belonging to arbitrary people.
        let Some(person) = person else {
            return Ok(Vec::new());
        };
        smallclaims::touched::note_read(|| "kind:harness.prompt".into());
        let connection = self.readers.get();
        let mut query = connection.prepare("SELECT seat,body,requested_at FROM local_harness_prompts
            WHERE person=?1 AND state IN ('open','submitted') AND expires_at>?2 ORDER BY requested_at DESC LIMIT 256")?;
        let rows = query
            .query_map(params![person, at.min(i64::MAX as u128) as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut items = Vec::new();
        for (seat, body, requested) in rows {
            let prompt: Prompt = serde_json::from_str(&body)?;
            if requested as u128 > at
                || owner(&connection, &seat)?.as_deref() != Some(person)
                || check_harness_event_runtime(
                    &connection,
                    &seat,
                    Some(&prompt.runtime_incarnation),
                )
                .is_err()
            {
                continue;
            }
            items.push(AttentionItemView {
                episode: prompt.episode.clone(), priority: "high".into(), kind: "harness-prompt".into(),
                review_mode: None, subject: format!("prompt/{}",prompt.episode), person: person.into(),
                requester_id: Some(seat.clone()), launch_id: None,variant_id: None,message_id: None,
                title: format!("{} {} prompt",prompt.harness,prompt.kind), detail: prompt.reason.clone(),
                request: prompt.can_respond().then(|| json!({"version":1,"type":"decision","question":"Allow this action?",
                    "why_person":"Only the requesting seat's owning person can answer.","summary":prompt.content,
                    "answers":[{"id":"approve","label":"Approve","consequence":"Send one native approval for this action.","outcome":"accept"},{"id":"deny","label":"Deny","consequence":"Deny this action through the native provider.","outcome":"decline"}]})),
                mission:None,mission_run:None,step:None,targets:vec![seat],requested_at_unix_ms:requested.into(),actions:Vec::new(),
            });
        }
        Ok(items)
    }

    pub(crate) fn prompt_metadata(&self, episode: &str, person: &str) -> Result<Option<Value>> {
        let connection = self.readers.get();
        let row: Option<(String,String,String,Option<u64>)> = connection.query_row(
            "SELECT seat,body,state,resolved_at FROM local_harness_prompts WHERE episode=?1 AND person=?2",
            params![episode,person], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
        ).optional()?;
        row.map(|(seat, body, state, resolved)| -> Result<Value> {
            let p: Prompt = serde_json::from_str(&body)?;
            let can_answer = state == "open" && p.can_respond();
            let fallback = p.state == "open" && !p.can_respond();
            let next_action = p.next_action.or_else(|| {
                fallback.then(|| "Use the native harness interface to continue.".into())
            });
            let mut metadata = json!({
                "kind":p.kind,"content":p.content,"choices":p.choices,
                "next_action":next_action,"how":p.how,
                "at":p.at_ms.map(|at|crate::api::client_timestamp(at.into())),"by":p.by,
                "provider":p.harness,"seat_id":seat,"prompt_id":p.prompt_id,
                "runtime_incarnation":p.runtime_incarnation,
                "expires_at":crate::api::client_timestamp(p.expires_at_ms.into()),
                "capability":p.capability,"state":p.state,"can_answer":can_answer,
                "disposition":p.disposition,"resolved_at_unix_ms":resolved
            });
            // The wire contract uses absent optional fields, not nullable strings.
            metadata
                .as_object_mut()
                .unwrap()
                .retain(|_, value| !value.is_null());
            Ok(metadata)
        })
        .transpose()
    }

    /// Reserve before I/O. A crash/uncertain write leaves submitted, so no second answer can
    /// replay a possibly delivered approval. The provider's terminal observation settles it.
    pub(crate) fn reserve_prompt(
        &self,
        actor: &str,
        answer: &Answer,
        operation: &str,
    ) -> Result<Prompt, St3Error> {
        if !actor.starts_with("person/") {
            return Err(St3Error::new(
                "forbidden",
                "permission response requires the owning person",
            ));
        }
        self.connection.batched(|tx| {
            let row: Option<(String,Option<String>,String,String)> = tx.query_row(
                "SELECT seat,person,body,state FROM local_harness_prompts WHERE episode=?1", [&answer.episode],
                |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
            ).optional().map_err(internal)?;
            let Some((seat,person,body,state)) = row else { return Err(St3Error::new("stale-permission-prompt","permission episode no longer exists")); };
            if person.as_deref()!=Some(actor) || owner(tx,&seat).map_err(internal)?.as_deref()!=Some(actor) {
                return Err(St3Error::new("forbidden","permission belongs to another person"));
            }
            let prompt:Prompt=serde_json::from_str(&body).map_err(internal)?;
            if prompt.runtime_incarnation!=answer.runtime_incarnation || prompt.prompt_id!=answer.prompt_id {
                return Err(St3Error::new("stale-permission-prompt","permission prompt or runtime incarnation changed"));
            }
            check_harness_event_runtime(tx,&seat,Some(&prompt.runtime_incarnation))?;
            if now_ms()>=prompt.expires_at_ms.into() { return Err(St3Error::new("permission-expired","permission response deadline passed")); }
            if state!="open" { return Err(St3Error::new("permission-already-answered","permission already has an answer or final disposition")); }
            if !prompt.can_respond() { return Err(St3Error::new("permission-unavailable","provider has no native response bridge")); }
            if !matches!(answer.answer_id.as_str(),"approve"|"deny") { return Err(St3Error::new("invalid-permission-answer","answer must be approve or deny")); }
            tx.execute("UPDATE local_harness_prompts SET state='submitted',answer=?2,operation=?3 WHERE episode=?1 AND state='open'",
                params![answer.episode,answer.answer_id,operation]).map_err(internal)?;
            // The provider is still open. Record a local source invalidation in the same
            // transaction so readers see can_answer=false before native I/O begins.
            let reserved=ClaimInput {subject:seat.clone(),kind:"harness.prompt".into(),actor:Some(seat),
                fields:BTreeMap::from([("incarnation_id".into(),json!(prompt.runtime_incarnation)),("prompt".into(),json!(prompt))]),
                evidence:Vec::new(),expected_subject:None,idempotency_key:Some(format!("prompt-reserved:{}:{operation}",answer.episode))};
            insert_local_observation_tx(tx,&self.origin,&reserved,now_ms())?;
            Ok(prompt)
        }).map_err(internal)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SEAT: &str = "agent/garden/orchard";
    fn fixture() -> Store {
        let store = Store::open_memory("amber").unwrap();
        let intent = crate::parse_intent(
            "version 2\nagent \"garden/orchard\" { command \"true\" }",
            "amber",
        )
        .unwrap();
        let plan = store
            .mission(
                &intent,
                IntentInput {
                    kdl: String::new(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply_as(
                &intent,
                &plan.subject_tokens,
                "declaration",
                Some("person/ada"),
            )
            .unwrap();
        runtime(&store, "running", "runtime-a", "running");
        store
    }
    fn runtime(store: &Store, status: &str, incarnation: &str, key: &str) {
        store
            .append_claim(&ClaimInput {
                subject: SEAT.into(),
                kind: "runtime.observed".into(),
                actor: Some(SEAT.into()),
                fields: BTreeMap::from([
                    ("status".into(), json!(status)),
                    ("incarnation_id".into(), json!(incarnation)),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some(key.into()),
            })
            .unwrap();
    }
    fn prompt() -> Prompt {
        serde_json::from_value(json!({
        "kind":"permission","choices":[{"id":"approve","label":"Approve","consequence":"Allow once"},{"id":"deny","label":"Deny","consequence":"Deny once"}],
        "next_action":null,"how":null,"at_ms":null,"by":null,"harness":"claude","incarnation":"provider-a","runtime_incarnation":"runtime-a",
        "session_id":"session-a","prompt_id":"hook:episode-a","episode":"episode-a","content":"printf private-fixture",
        "reason":"Bash","expires_at_ms":(now_ms() as u64+60_000),"endpoint":"/tmp/invented-native-prompt/reply",
        "capability":"claude-permission-hook","state":"open","disposition":null})).unwrap()
    }
    fn publish(store: &Store, p: &Prompt, sequence: u64) {
        store
            .append_harness_event(&crate::harness_events::Publication {
                runtime_incarnation: p.runtime_incarnation.clone(),
                sequence,
                claim: ClaimInput {
                    subject: SEAT.into(),
                    kind: "harness.prompt".into(),
                    actor: Some(SEAT.into()),
                    fields: BTreeMap::from([
                        ("incarnation_id".into(), json!(p.runtime_incarnation)),
                        ("prompt".into(), json!(p)),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: Some(format!("prompt:{sequence}")),
                },
            })
            .unwrap();
    }
    fn answer(p: &Prompt) -> Answer {
        Answer {
            episode: p.episode.clone(),
            prompt_id: p.prompt_id.clone(),
            runtime_incarnation: p.runtime_incarnation.clone(),
            answer_id: "approve".into(),
        }
    }
    #[test]
    fn unknown_kind_keeps_capture_but_offers_no_guessed_answer() {
        let store = fixture();
        let mut p = prompt();
        p.kind = "feedback".into();
        p.capability = "unknown".into();
        publish(&store, &p, 1);
        let rows = store.prompt_items(Some("person/ada"), now_ms()).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].request.is_none());
        let metadata = store
            .prompt_metadata(&p.episode, "person/ada")
            .unwrap()
            .unwrap();
        assert_eq!(metadata["content"], "printf private-fixture");
        assert_eq!(metadata["can_answer"], false);
        assert!(metadata["next_action"].is_string());
        assert_eq!(
            store
                .reserve_prompt("person/ada", &answer(&p), "unknown")
                .unwrap_err()
                .code,
            "permission-unavailable"
        );
    }
    #[test]
    fn owner_only_single_use_and_terminal_observation_closes_episode() {
        let store = fixture();
        let p = prompt();
        publish(&store, &p, 1);
        publish(&store, &p, 1);
        assert_eq!(
            store
                .prompt_items(Some("person/ada"), now_ms())
                .unwrap()
                .len(),
            1
        );
        assert!(store.prompt_items(None, now_ms()).unwrap().is_empty());
        assert!(
            store
                .prompt_items(Some("person/intruder"), now_ms())
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .prompt_metadata(&p.episode, "person/intruder")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .reserve_prompt("person/intruder", &answer(&p), "wrong")
                .unwrap_err()
                .code,
            "forbidden"
        );
        let mut stale = answer(&p);
        stale.runtime_incarnation = "runtime-old".into();
        assert_eq!(
            store
                .reserve_prompt("person/ada", &stale, "stale")
                .unwrap_err()
                .code,
            "stale-permission-prompt"
        );
        store
            .reserve_prompt("person/ada", &answer(&p), "one")
            .unwrap();
        assert_eq!(
            store
                .prompt_metadata(&p.episode, "person/ada")
                .unwrap()
                .unwrap()["can_answer"],
            false
        );
        assert_eq!(
            store
                .reserve_prompt("person/ada", &answer(&p), "two")
                .unwrap_err()
                .code,
            "permission-already-answered"
        );
        let mut closed = p.clone();
        closed.state = "answered".into();
        closed.disposition = Some("approve".into());
        closed.endpoint = None;
        publish(&store, &closed, 2);
        publish(&store, &p, 3);
        assert!(
            store
                .prompt_items(Some("person/ada"), now_ms())
                .unwrap()
                .is_empty()
        );
        let metadata = store
            .prompt_metadata(&p.episode, "person/ada")
            .unwrap()
            .unwrap();
        assert_eq!(metadata["state"], "answered");
        assert_eq!(metadata["disposition"], "approve");
        assert!(metadata.get("endpoint").is_none());
        assert!(
            !serde_json::to_string(&store.export_replication(0).unwrap())
                .unwrap()
                .contains("private-fixture")
        );
    }
    #[test]
    fn runtime_stop_closes_and_refuses_a_stale_answer() {
        let store = fixture();
        let p = prompt();
        publish(&store, &p, 1);
        runtime(&store, "stopped", "runtime-a", "stopped");
        assert!(
            store
                .prompt_items(Some("person/ada"), now_ms())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .prompt_metadata(&p.episode, "person/ada")
                .unwrap()
                .unwrap()["state"],
            "ended"
        );
        assert!(
            store
                .reserve_prompt("person/ada", &answer(&p), "late")
                .is_err()
        );
    }
    #[test]
    fn disappearance_is_unavailable_and_expiry_never_sends_a_decision() {
        let store = fixture();
        let mut p = prompt();
        publish(&store, &p, 1);
        p.state = "unavailable".into();
        p.endpoint = None;
        p.disposition = Some("unavailable".into());
        publish(&store, &p, 2);
        assert!(
            store
                .prompt_items(Some("person/ada"), now_ms())
                .unwrap()
                .is_empty()
        );
        let metadata = store
            .prompt_metadata(&p.episode, "person/ada")
            .unwrap()
            .unwrap();
        assert_eq!(metadata["state"], "unavailable");
        assert!(metadata["how"].is_null());
        let mut p = prompt();
        p.episode = "expired".into();
        p.prompt_id = "expired-id".into();
        p.expires_at_ms = 1;
        publish(&store, &p, 3);
        assert_eq!(
            store
                .reserve_prompt("person/ada", &answer(&p), "expired")
                .unwrap_err()
                .code,
            "permission-expired"
        );
    }

    #[test]
    fn missing_owner_records_routing_unavailable_without_guessing_a_person() {
        let store = Store::open_memory("amber").unwrap();
        let intent = crate::graph::parse_internal_intent(
            "version 2\nagent \"garden/orchard\" { command \"true\" }",
            "amber",
        )
        .unwrap();
        store.apply_internal(&intent, "unowned").unwrap();
        runtime(&store, "running", "runtime-a", "running");
        let p = prompt();
        publish(&store, &p, 1);
        assert!(store.prompt_items(None, now_ms()).unwrap().is_empty());
        assert!(
            store
                .prompt_items(Some("person/operator"), now_ms())
                .unwrap()
                .is_empty()
        );
        let row = store
            .connection
            .batched(|tx| {
                tx.query_row(
                    "SELECT person,body FROM local_harness_prompts WHERE episode=?1",
                    [&p.episode],
                    |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
                )
            })
            .unwrap()
            .unwrap();
        assert!(row.0.is_none());
        let recorded: Prompt = serde_json::from_str(&row.1).unwrap();
        assert_eq!(recorded.state, "unavailable");
        assert_eq!(recorded.disposition.as_deref(), Some("owner-unavailable"));
        assert!(recorded.endpoint.is_none());
        assert!(recorded.next_action.is_some());
        assert_eq!(
            store
                .reserve_prompt("person/operator", &answer(&p), "guess")
                .unwrap_err()
                .code,
            "forbidden"
        );
    }
}
