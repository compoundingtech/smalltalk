//! Terminal agent identity and first-send admission share the serialized writer.
use super::*;
use crate::model::{LifecycleUnavailableReason, MessageSendRefusal};

pub const SCHEMA: &str = "CREATE INDEX IF NOT EXISTS st3_replica_lifecycle_unavailable
    ON replica_records(subject_hint,kind_hint) WHERE state IN ('unknown','invalid');
    CREATE INDEX IF NOT EXISTS st3_lifecycle_operation_index
    ON claims(subject,json_extract(body,'$._operation.id'))
    WHERE kind IN ('agent.retired','intent.desired') AND json_extract(body,'$._operation.id') IS NOT NULL;";

pub const KIND: &str = "agent.retired";

pub fn writer_required() -> St3Error {
    St3Error::new(
        "invalid-retirement",
        "terminal retirement must be written through agent.retire",
    )
    .with_detail("applied", "none")
}

/// Every declaration selector, including mission materialization, obeys the same terminal
/// reducer. The trigger probes only the changed subject; it never scans the fleet on writes.
pub fn create_selection_triggers(connection: &Connection) -> Result<()> {
    for (name, event) in [
        ("insert", "INSERT"),
        ("update", "UPDATE OF kind,member,body"),
    ] {
        connection.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS st3_retired_desired_{name} AFTER {event} ON desired
             WHEN (NEW.kind != 'stop' OR NEW.member IS NOT NULL OR json_extract(NEW.body,'$.retirement') IS NULL)
               AND EXISTS(SELECT 1 FROM claims WHERE subject=NEW.subject AND kind='agent.retired'
                 AND NOT EXISTS(SELECT 1 FROM replica_records WHERE claim_id=claims.id AND state='repaired'))
             BEGIN UPDATE desired SET kind='stop',member=NULL,
               body=json_object('arguments',json_array(NEW.subject),'children',json_array(), 'name','stop',
                 'properties',json_object(),'retirement',(SELECT claims.id FROM claims JOIN batches ON batches.id=claims.batch_id
                  WHERE claims.subject=NEW.subject AND claims.kind='agent.retired'
                   AND NOT EXISTS(SELECT 1 FROM replica_records WHERE claim_id=claims.id AND state='repaired')
                  ORDER BY {CANONICAL_ORDER_DESC} LIMIT 1)) WHERE subject=NEW.subject; END;"
        ))?;
    }
    Ok(())
}

pub fn record(
    connection: &Connection,
    subject: &str,
    at: Option<u64>,
) -> Result<Option<ClaimRecord>, St3Error> {
    connection.prepare_cached(&format!(
        "SELECT {CLAIM_COLUMNS} FROM claims JOIN batches ON batches.id=claims.batch_id
         WHERE claims.subject=?1 AND claims.kind='agent.retired' AND (?2 IS NULL OR claims.store_index<=?2)
         AND NOT EXISTS(SELECT 1 FROM replica_records WHERE replica_records.claim_id=claims.id AND state='repaired')
         ORDER BY {CANONICAL_ORDER_DESC} LIMIT 1"
    )).map_err(internal)?.query_row(params![subject,at], claim_from_row).optional().map_err(internal)
}

fn retired(recipient: &str, claim: &ClaimRecord) -> St3Error {
    MessageSendRefusal::Retired {
        recipient: recipient.into(),
        successor: claim.body["fields"]["successor"]
            .as_str()
            .map(str::to_owned),
        transfer: claim.body["fields"]["transfer"].as_str().map(str::to_owned),
    }
    .into_error()
}

pub fn refuse_declaration(
    connection: &Connection,
    desired: &DesiredSubject,
) -> Result<(), St3Error> {
    if desired.kind == "agent"
        && let Some(claim) = record(connection, &desired.subject, None)?
    {
        return Err(retired(&desired.subject, &claim));
    }
    Ok(())
}

/// Terminal identity supersedes owned membership too. An effect prepared from the old
/// declaration must fail its authority check; only the canonical terminal stop is current.
pub fn terminal_selection(
    connection: &Connection,
    desired: &DesiredSubject,
) -> Result<Option<bool>, St3Error> {
    if !desired.subject.starts_with("agent/") {
        return Ok(None);
    }
    Ok(record(connection, &desired.subject, None)?.map(|claim| {
        desired.kind == "stop"
            && desired.member.is_none()
            && desired.desired
                == json!({"name":"stop", "arguments":[desired.subject],
                "properties":{}, "children":[], "retirement":claim.id})
    }))
}

pub fn admit_recipient(
    connection: &Connection,
    recipient: Option<&str>,
) -> Result<(), St3Error> {
    let Some(recipient) = recipient.filter(|to| to.starts_with("agent/")) else {
        return Ok(());
    };
    let pending: bool = connection.prepare_cached("SELECT EXISTS(SELECT 1 FROM replica_records INDEXED BY st3_replica_lifecycle_unavailable
        WHERE subject_hint=?1 AND state IN ('unknown','invalid') AND (kind_hint IN ('agent.retired','intent.desired') OR kind_hint IS NULL))")
        .map_err(internal)?.query_row([recipient], |row| row.get(0)).map_err(internal)?;
    if pending {
        return Err(unavailable(
            recipient,
            LifecycleUnavailableReason::Unavailable,
        ));
    }
    let conflict: bool = connection.prepare_cached("SELECT EXISTS(SELECT 1 FROM claims INDEXED BY st3_lifecycle_operation_index
        CROSS JOIN operations ON operations.id=json_extract(claims.body,'$._operation.id')
        WHERE claims.subject=?1 AND claims.kind IN ('agent.retired','intent.desired')
          AND json_extract(claims.body,'$._operation.id') IS NOT NULL AND operations.state='conflict'
          AND NOT EXISTS(SELECT 1 FROM replica_records WHERE claim_id=claims.id AND state='repaired'))")
        .map_err(internal)?.query_row([recipient], |row| row.get(0)).map_err(internal)?;
    if conflict {
        return Err(unavailable(recipient, LifecycleUnavailableReason::Conflict));
    }
    if let Some(claim) = record(connection, recipient, None)? {
        let another: bool = connection.prepare_cached("SELECT EXISTS(SELECT 1 FROM claims
            WHERE subject=?1 AND kind='agent.retired' AND id<>?2
              AND NOT EXISTS(SELECT 1 FROM replica_records WHERE claim_id=claims.id AND state='repaired'))")
            .map_err(internal)?.query_row(params![recipient,claim.id],|row|row.get(0)).map_err(internal)?;
        if another {
            return Err(unavailable(recipient, LifecycleUnavailableReason::Conflict));
        }
        let token = claim.body["fields"]["declaration"]
            .as_str()
            .ok_or_else(|| unavailable(recipient, LifecycleUnavailableReason::Conflict))?;
        let declared = declaration(connection, recipient, token)?;
        if declared
            .member
            .as_ref()
            .is_none_or(|member| member.host != claim.origin)
            || claim.body["fields"]["owner_host"].as_str() != Some(&claim.origin)
        {
            return Err(unavailable(recipient, LifecycleUnavailableReason::Conflict));
        }
        return Err(retired(recipient, &claim));
    }
    Ok(())
}

/// Runtime selection lowers terminal identity to a stop, retaining the declaration token and
/// ownership history. The terminal claim remains separate and can never be overwritten by KDL.
pub fn project_subject(connection: &Connection, subject: &str) -> Result<(), St3Error> {
    if let Some(claim) = record(connection, subject, None)? {
        let body = json!({"name":"stop", "arguments":[subject], "properties":{}, "children":[], "retirement":claim.id});
        connection
            .execute(
                "UPDATE desired SET kind='stop',member=NULL,body=?2 WHERE subject=?1",
                params![subject, canonical_json_text(&body).map_err(internal)?],
            )
            .map_err(internal)?;
    }
    Ok(())
}

pub fn overlay_row(
    connection: &Connection,
    subject: &str,
    at: Option<u64>,
    mut row: Option<DesiredRow>,
) -> Result<Option<DesiredRow>> {
    if let Some(retirement) = record(connection, subject, at).map_err(anyhow::Error::new)?
        && let Some(row) = row.as_mut()
    {
        row.kind = "stop".into();
        row.member = None;
        row.body = canonical_json_text(
            &json!({"name":"stop", "arguments":[subject], "properties":{}, "children":[], "retirement":retirement.id}),
        )?;
    }
    Ok(row)
}

fn unavailable(subject: &str, reason: LifecycleUnavailableReason) -> St3Error {
    MessageSendRefusal::LifecycleUnavailable {
        recipient: subject.into(),
        reason,
    }
    .into_error()
}

/// A stop preserves its original declaration through causal predecessors, including a seat
/// stopped before retirement. Ambiguous ownership is never guessed from a runtime observation.
fn declaration(
    connection: &Connection,
    subject: &str,
    token: &str,
) -> Result<DesiredSubject, St3Error> {
    let mut claim = claim_by_id_tx(connection, token)
        .map_err(internal)?
        .ok_or_else(|| unavailable(subject, LifecycleUnavailableReason::Unavailable))?;
    let mut visited = BTreeSet::new();
    loop {
        if !visited.insert(claim.id.clone()) {
            return Err(unavailable(subject, LifecycleUnavailableReason::Conflict));
        }
        if claim.subject != subject || claim.kind != "intent.desired" {
            return Err(unavailable(subject, LifecycleUnavailableReason::Conflict));
        }
        let desired: DesiredSubject =
            serde_json::from_value(claim.body.clone()).map_err(internal)?;
        if desired.kind == "agent" {
            return Ok(desired);
        }
        if desired.kind != "stop" || claim.predecessors.len() != 1 {
            return Err(unavailable(subject, LifecycleUnavailableReason::Conflict));
        }
        claim = claim_by_id_tx(connection, &claim.predecessors[0])
            .map_err(internal)?
            .ok_or_else(|| unavailable(subject, LifecycleUnavailableReason::Unavailable))?;
    }
}

fn check_registry(
    connection: &Connection,
    origin: &str,
    configured_peers: &[String],
    digest: &str,
) -> Result<(), St3Error> {
    let membership = fleet_membership_tx(connection).map_err(internal)?;
    let peers = configured_peers
        .iter()
        .cloned()
        .chain(
            membership
                .incarnations()
                .filter(|member| member.end.is_none())
                .map(|member| member.name.clone()),
        )
        .collect::<BTreeSet<_>>();
    for peer in peers.into_iter().filter(|peer| peer != origin) {
        let known: Option<String> = connection
            .query_row(
                "SELECT schema_digest FROM replication_peers WHERE peer=?1",
                [&peer],
                |row| row.get(0),
            )
            .optional()
            .map_err(internal)?
            .flatten();
        if known.as_deref() != Some(digest) {
            return Err(St3Error::new("retirement-upgrade-required", format!("terminal retirement requires every member to run the new registry; `{peer}` has not advertised it"))
                .with_detail("applied", "none").with_detail("peer", peer));
        }
    }
    Ok(())
}

impl Store {
    /// Write terminal identity only at its declaring host. Exact repeats return before admission,
    /// including the revision and rollout checks; changed requests keep idempotency-mismatch.
    #[allow(clippy::too_many_arguments)]
    pub fn retire_agent(
        &self,
        subject: &str,
        actor: &str,
        reason: &str,
        token: &str,
        incarnation: Option<&str>,
        key: &str,
        peers: &[String],
    ) -> Result<(ClaimRecord, bool), St3Error> {
        if reason.trim().is_empty() {
            return Err(St3Error::new(
                "invalid-retirement",
                "retirement needs a reason",
            ));
        }
        let input = ClaimInput {
            subject: subject.into(),
            kind: KIND.into(),
            actor: Some(actor.into()),
            fields: BTreeMap::from([
                ("reason".into(), json!(reason)),
                ("retired_by".into(), json!(actor)),
                ("declaration".into(), json!(token)),
                ("owner_host".into(), json!(self.origin())),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some(key.into()),
        };
        self.graph.ensure_principal_key(actor)?;
        let admission = |connection: &Connection| {
            admit_recipient(connection, Some(subject))?;
            let current: Option<String> = connection
                .query_row(
                    "SELECT claim_id FROM desired WHERE subject=?1",
                    [subject],
                    |row| row.get(0),
                )
                .optional()
                .map_err(internal)?;
            if current.as_deref() != Some(token) {
                return Err(St3Error::new(
                    "stale-subject",
                    format!("agent `{subject}` declaration changed"),
                ));
            }
            let declared = declaration(connection, subject, token)?;
            let owner = declared
                .member
                .as_ref()
                .map(|member| member.host.as_str())
                .ok_or_else(|| unavailable(subject, LifecycleUnavailableReason::Unavailable))?;
            if owner != self.origin() {
                return Err(St3Error::new(
                    "runtime-not-local",
                    format!("agent `{subject}` must be retired at its owning host `{owner}`"),
                )
                .with_detail("applied", "none")
                .with_detail("owner_host", owner));
            }
            if let Some(incarnation) = incarnation {
                let actual = latest_actual(connection, subject).map_err(internal)?;
                if actual
                    .as_ref()
                    .and_then(|body| body.get("fields").unwrap_or(body).get("incarnation_id"))
                    .and_then(Value::as_str)
                    != Some(incarnation)
                {
                    return Err(St3Error::new(
                        "stale-incarnation",
                        format!("agent `{subject}` incarnation changed"),
                    ));
                }
            }
            check_registry(
                connection,
                self.origin(),
                peers,
                &self.smalltalk.schema_digest(),
            )
        };
        append_claim_with_commit_context(
            &self.graph,
            &input,
            None,
            None,
            None,
            None,
            ClaimCommitContext {
                admission: Some(&admission),
                signature: None,
                retirement_writer: true,
                assistant: None,
            },
        )
    }

    pub fn agent_retirement(&self, subject: &str) -> Result<Option<ClaimRecord>, St3Error> {
        record(&self.readers.get(), subject, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    const SUBJECT: &str = "agent/example/retiring";
    const ACTOR: &str = "person/avery";

    fn declare(store: &Store) -> String {
        let source = "version 2\nagent \"example/retiring\" { command \"true\"; workspace \"/tmp\"; restart \"never\"; }\n";
        let intent = crate::parse_intent(source, store.origin()).unwrap();
        store.apply_internal(&intent, "retirement-fixture").unwrap();
        store.selected_desired_token(SUBJECT).unwrap().unwrap()
    }
    fn send(key: &str) -> ClaimInput {
        ClaimInput {
            subject: format!("message/{key}"),
            kind: "message.sent".into(),
            actor: Some(ACTOR.into()),
            fields: BTreeMap::from([
                ("from".into(), json!(ACTOR)),
                ("to".into(), json!(SUBJECT)),
                ("content".into(), json!("An invented note.")),
                ("status".into(), json!("sent")),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some(key.into()),
        }
    }
    fn retire(store: &Store, token: &str) -> ClaimRecord {
        store
            .retire_agent(
                SUBJECT,
                ACTOR,
                "Work finished",
                token,
                None,
                "retirement-fixture-operation",
                &[],
            )
            .unwrap()
            .0
    }

    #[test]
    fn generic_local_writers_cannot_create_terminal_identity() {
        let store = Store::open_memory("orchid").unwrap();
        let token = declare(&store);
        let input = ClaimInput {
            subject: SUBJECT.into(),
            kind: KIND.into(),
            actor: Some(ACTOR.into()),
            fields: BTreeMap::from([
                ("reason".into(), json!("Work finished")),
                ("retired_by".into(), json!(ACTOR)),
                ("declaration".into(), json!(token)),
                ("owner_host".into(), json!("orchid")),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some("generic-retirement".into()),
        };
        let error = store.append_claim(&input).unwrap_err();
        assert_eq!(error.code, "invalid-retirement");
        assert_eq!(error.details["applied"], "none");
        let error = store
            .connection
            .batched(|tx| {
                append_claim_tx(
                    tx,
                    "orchid",
                    SUBJECT,
                    KIND,
                    Some(ACTOR),
                    &json!({"fields":input.fields}),
                    &[],
                    None,
                )
                .map_err(claim_append_error)
            })
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, "invalid-retirement");
        assert!(store.agent_retirement(SUBJECT).unwrap().is_none());
        assert!(
            store
                .operation_claim("generic-retirement")
                .unwrap()
                .is_none()
        );
        let terminal = retire(&store, &token);
        let peer = Store::open_memory("birch").unwrap();
        peer.import_replication("orchid", &store.export_replication(0).unwrap())
            .unwrap();
        assert_eq!(
            peer.agent_retirement(SUBJECT).unwrap().unwrap().id,
            terminal.id
        );
    }

    #[test]
    fn first_sends_are_fenced_after_replay_and_key_body_validation() {
        let store = Store::open_memory("orchid").unwrap();
        let token = declare(&store);
        let input = send("before");
        let accepted = store.append_claim(&input).unwrap();
        let terminal = retire(&store, &token);
        assert!(accepted.store_index < terminal.store_index);
        let (repeat, new) = store.append_claim_outcome(&input).unwrap();
        assert!(!new);
        assert_eq!(repeat.id, accepted.id);
        let mut changed = input.clone();
        changed
            .fields
            .insert("content".into(), json!("Changed note"));
        assert_eq!(
            store.append_claim(&changed).unwrap_err().code,
            "idempotency-mismatch"
        );
        let after = send("after");
        let error = store.append_claim(&after).unwrap_err();
        assert_eq!(error.code, "recipient-retired");
        assert_eq!(error.details["applied"], "none");
        assert_eq!(error.details["refusal"]["variant"], "Retired");
        assert!(store.operation_claim("after").unwrap().is_none());
        assert!(store.message(&after.subject).unwrap().is_none());
        let (repeat, new) = store
            .retire_agent(
                SUBJECT,
                ACTOR,
                "Work finished",
                &token,
                None,
                "retirement-fixture-operation",
                &[],
            )
            .unwrap();
        assert!(!new);
        assert_eq!(repeat.id, terminal.id);
    }

    #[test]
    fn stopping_keeps_the_inbox_and_can_be_followed_by_permanent_retirement() {
        let store = Store::open_memory("orchid").unwrap();
        declare(&store);
        let intent =
            crate::parse_intent("version 2\nstop \"agent/example/retiring\"\n", "orchid").unwrap();
        store
            .apply_internal(&intent, "stop-before-retirement")
            .unwrap();
        store.append_claim(&send("stopped-inbox-send")).unwrap();
        let token = store.selected_desired_token(SUBJECT).unwrap().unwrap();
        retire(&store, &token);
        assert_eq!(
            store
                .append_claim(&send("permanently-retired-send"))
                .unwrap_err()
                .code,
            "recipient-retired"
        );
    }

    #[test]
    fn simultaneous_send_and_retire_follow_the_writer_order() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&root.path().join("graph.db"), "orchid").unwrap());
        let token = declare(&store);
        let barrier = Arc::new(Barrier::new(3));
        let sender = {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.append_claim(&send("racing"))
            })
        };
        let retiring = {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                retire(&store, &token)
            })
        };
        barrier.wait();
        let sent = sender.join().unwrap();
        let retired = retiring.join().unwrap();
        match sent {
            Ok(sent) => assert!(sent.store_index < retired.store_index),
            Err(error) => assert_eq!(error.code, "recipient-retired"),
        }
        assert_eq!(
            store.append_claim(&send("after-race")).unwrap_err().code,
            "recipient-retired"
        );
    }

    #[test]
    fn terminal_identity_survives_replay_reopen_and_a_later_replicated_declaration() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("graph.db");
        let store = Store::open(&path, "orchid").unwrap();
        let token = declare(&store);
        let retired = retire(&store, &token);
        let source =
            "version 2\nagent \"example/retiring\" { command \"false\"; workspace \"/tmp\"; }\n";
        let intent = crate::parse_intent(source, "orchid").unwrap();
        assert_eq!(
            store.apply_internal(&intent, "revive").unwrap_err().code,
            "recipient-retired"
        );
        let peer = Store::open_memory("birch").unwrap();
        peer.import_replication("orchid", &store.export_replication(0).unwrap())
            .unwrap();
        // A stale member's history may contain a later declaration; its order cannot revive an identity.
        let local = Store::open_memory("birch").unwrap();
        local.apply_internal(&intent, "later-declaration").unwrap();
        store
            .import_replication("birch", &local.export_replication(0).unwrap())
            .unwrap();
        assert!(
            store
                .desired_subjects()
                .unwrap()
                .iter()
                .all(|desired| desired.subject != SUBJECT || desired.member.is_none())
        );
        store.rebuild_claim_projections().unwrap();
        drop(store);
        let reopened = Store::open(&path, "orchid").unwrap();
        assert_eq!(
            reopened.agent_retirement(SUBJECT).unwrap().unwrap().id,
            retired.id
        );
        assert_eq!(
            reopened
                .append_claim(&send("after-reopen"))
                .unwrap_err()
                .code,
            "recipient-retired"
        );
        assert!(
            reopened
                .desired_subjects()
                .unwrap()
                .iter()
                .all(|desired| desired.subject != SUBJECT || desired.member.is_none())
        );
    }

    #[test]
    fn direct_transaction_producers_cannot_bypass_admission_but_import_is_history() {
        let store = Store::open_memory("orchid").unwrap();
        let token = declare(&store);
        let early = Store::open_memory("birch").unwrap();
        early
            .append_claim(&send("already-accepted-elsewhere"))
            .unwrap();
        retire(&store, &token);
        let error=store.connection.batched(|tx| {
            append_claim_tx(tx,"orchid","message/direct","message.sent",Some("daemon/runtime"),
                &json!({"fields":{"from":"daemon/runtime","to":SUBJECT,"content":"Direct daemon producer","status":"sent"}}),&[],None).map_err(claim_append_error)
        }).unwrap().unwrap_err();
        assert_eq!(error.code, "recipient-retired");
        assert!(store.message("message/direct").unwrap().is_none());
        store
            .import_replication("birch", &early.export_replication(0).unwrap())
            .unwrap();
        assert!(
            store
                .message("message/already-accepted-elsewhere")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn unavailable_local_lifecycle_is_a_refusal_and_never_guessed_active() {
        let store = Store::open_memory("orchid").unwrap();
        declare(&store);
        store.connection.write().execute("INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,raw,state,subject_hint,kind_hint,updated_at_unix_ms)
            VALUES('record/pending','birch',1,'pending',0,'{}','unknown',?1,'agent.retired','1')",[SUBJECT]).unwrap();
        let error = store.append_claim(&send("incomplete")).unwrap_err();
        assert_eq!(error.code, "lifecycle-unavailable");
        assert_eq!(error.details["refusal"]["reason"], "unavailable");
        assert!(store.message("message/incomplete").unwrap().is_none());
    }

    #[test]
    fn lagging_peers_queue_early_sends_and_refuse_after_retirement_replicates() {
        let owner = Store::open_memory("orchid").unwrap();
        let token = declare(&owner);
        let peer = Store::open_memory("birch").unwrap();
        peer.import_replication("orchid", &owner.export_replication(0).unwrap())
            .unwrap();
        let remote = peer
            .retire_agent(
                SUBJECT,
                ACTOR,
                "Work finished",
                &token,
                None,
                "remote-retirement-request",
                &[],
            )
            .unwrap_err();
        assert_eq!(remote.code, "runtime-not-local");
        assert_eq!(remote.details["applied"], "none");
        assert!(peer.agent_retirement(SUBJECT).unwrap().is_none());
        owner
            .append_claim(&ClaimInput {
                subject: SUBJECT.into(),
                kind: "delivery.hold".into(),
                actor: Some(ACTOR.into()),
                fields: BTreeMap::from([
                    ("held".into(), json!(true)),
                    ("until_unix_ms".into(), json!((now_ms() + 60_000) as u64)),
                    ("reason".into(), json!("Quiet time")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        owner.append_claim(&send("held-inbox-send")).unwrap();
        let retired = retire(&owner, &token);
        let early = send("lagging-offline-owner-send");
        let accepted = peer.append_claim(&early).unwrap();
        peer.import_replication("orchid", &owner.export_replication(0).unwrap())
            .unwrap();
        assert_eq!(
            peer.agent_retirement(SUBJECT).unwrap().unwrap().id,
            retired.id
        );
        assert_eq!(peer.append_claim(&early).unwrap().id, accepted.id);
        assert_eq!(
            peer.append_claim(&send("after-replication"))
                .unwrap_err()
                .code,
            "recipient-retired"
        );
        owner
            .import_replication("birch", &peer.export_replication(0).unwrap())
            .unwrap();
        assert!(
            owner.message(&early.subject).unwrap().is_some(),
            "early accepted history must remain durable"
        );
    }

    #[test]
    fn all_runtime_selection_writers_obey_terminal_precedence() {
        let store = Store::open_memory("orchid").unwrap();
        let token = declare(&store);
        let stale = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|d| d.subject == SUBJECT)
            .unwrap();
        retire(&store, &token);
        assert_eq!(
            store.owned_desired_guard(&stale).unwrap_err().code,
            "stale-set-member"
        );
        store.connection.write().execute("UPDATE desired SET kind='agent',member=json_object('host','orchid'),body='{}' WHERE subject=?1",[SUBJECT]).unwrap();
        let desired = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|subject| subject.subject == SUBJECT)
            .unwrap();
        assert_eq!(desired.kind, "stop");
        assert!(desired.member.is_none());
        assert!(desired.desired.get("retirement").is_some());
        store.owned_desired_guard(&desired).unwrap();
    }

    #[test]
    fn admission_work_does_not_grow_with_unrelated_operation_conflicts() {
        let store = Store::open_memory("orchid").unwrap();
        declare(&store);
        let connection = store.readers.get();
        let measure = || {
            let scope = smallclaims::sqlite::work::SqliteWorkScope::start();
            admit_recipient(&connection, Some(SUBJECT)).unwrap();
            scope.finish()
        };
        let small = measure();
        assert!(small.vm_steps > 0);
        for index in 0..200 {
            let mut input = send(&format!("unrelated-conflict-{index}"));
            input
                .fields
                .insert("to".into(), json!("agent/example/elsewhere"));
            store.append_claim(&input).unwrap();
        }
        store.connection.write().execute("UPDATE operations SET state='conflict' WHERE canonical_claim_id IN (SELECT id FROM claims WHERE kind='message.sent')",[]).unwrap();
        let large = measure();
        assert_eq!(large.vm_steps, small.vm_steps);
        assert_eq!(large.fullscan_steps, 0);
    }

    #[test]
    fn local_producer_inventory_cannot_call_the_admitted_primitive() {
        fn scan(path: &std::path::Path, calls: &mut Vec<String>) {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    scan(&path, calls);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    let source = std::fs::read_to_string(&path).unwrap();
                    // The primitive is private. Only the common wrapper and the commit-context
                    // writer may call it; a newly introduced direct producer fails this guard.
                    for _ in source.match_indices("append_admitted_claim_tx(") {
                        calls.push(path.file_name().unwrap().to_str().unwrap().into());
                    }
                }
            }
        }
        let root = std::path::Path::new(test_env!("CARGO_MANIFEST_DIR")).join("src");
        let mut calls = Vec::new();
        scan(&root, &mut calls);
        // One occurrence in this guard itself, and three in store.rs (definition + two seams).
        calls.sort();
        assert_eq!(
            calls,
            vec!["retirements.rs", "store.rs", "store.rs", "store.rs"]
        );
        for source in [
            include_str!("watches.rs"),
            include_str!("person_work.rs"),
            include_str!("limits.rs"),
            include_str!("adhoc_work.rs"),
            include_str!("github_workflow_failures.rs"),
        ] {
            assert!(source.contains("append_claim_tx(") && source.contains("\"message.sent\""));
        }
    }

    #[test]
    fn a_conflicting_lifecycle_operation_never_guesses_active() {
        let store = Store::open_memory("orchid").unwrap();
        let token = declare(&store);
        let terminal = retire(&store, &token);
        assert_eq!(
            store
                .connection
                .write()
                .execute(
                    "UPDATE operations SET state='conflict' WHERE canonical_claim_id=?1",
                    [terminal.id]
                )
                .unwrap(),
            1
        );
        let error = store
            .append_claim(&send("conflicting-lifecycle-send"))
            .unwrap_err();
        assert_eq!(error.code, "lifecycle-unavailable");
        assert_eq!(error.details["refusal"]["reason"], "conflict");
        assert!(
            store
                .message("message/conflicting-lifecycle-send")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn coordinated_upgrade_gate_blocks_retirement_but_keeps_ordinary_sends() {
        let store = Store::open_memory("orchid").unwrap();
        let token = declare(&store);
        let error = store
            .retire_agent(
                SUBJECT,
                ACTOR,
                "Work finished",
                &token,
                None,
                "upgrade-gated-retirement",
                &["birch".into()],
            )
            .unwrap_err();
        assert_eq!(error.code, "retirement-upgrade-required");
        assert!(store.agent_retirement(SUBJECT).unwrap().is_none());
        store.append_claim(&send("owner-offline-safe")).unwrap();
        store.connection.write().execute("INSERT INTO replication_peers(peer,status,schema_digest,updated_at_unix_ms) VALUES('birch','up',?1,'1')",[store.smalltalk.schema_digest()]).unwrap();
        store
            .retire_agent(
                SUBJECT,
                ACTOR,
                "Work finished",
                &token,
                None,
                "upgrade-gated-retirement",
                &["birch".into()],
            )
            .unwrap();
    }
}

#[cfg(test)]
mod registry_tests {
    use super::super::fleet_admission_tests::{admit, key, node, sync};
    use super::*;
    const FLEET: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
    #[test]
    fn previous_registries_retain_unknown_kind_and_field_and_upgrade_admits_original_claim() {
        for unknown_kind in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("peer.db");
            let anchor = key();
            let peer_key = key();
            let owner = node("orchid", Some(&anchor), Some(&anchor));
            admit(&owner, "orchid", &anchor, "anchor", None);
            let source =
                "version 2\nagent \"example/registry\" { command \"true\"; workspace \"/tmp\"; }\n";
            let intent = crate::parse_intent(source, "orchid").unwrap();
            owner
                .apply_internal(&intent, "registry-declaration")
                .unwrap();
            let subject = "agent/example/registry";
            let token = owner.selected_desired_token(subject).unwrap().unwrap();
            let accepted = owner
                .retire_agent(
                    subject,
                    "person/avery",
                    "Work finished",
                    &token,
                    None,
                    "registry-retirement",
                    &[],
                )
                .unwrap()
                .0;
            // Simulate a member whose decoder is from an older build after the creation;
            // the production gate independently prevents creation in a mixed-build fleet.
            admit(&owner, "birch", &peer_key, "invite", None);
            let older = Store::open(&path, "birch").unwrap();
            older.bind_fleet(FLEET).unwrap();
            older.pin_fleet_anchor(anchor.public()).unwrap();
            older.set_member_key(Some(peer_key.clone())).unwrap();
            let mut registry = st3_schema::registry().clone();
            if unknown_kind {
                registry.claims.remove(KIND);
            } else {
                registry
                    .claims
                    .get_mut(KIND)
                    .unwrap()
                    .fields
                    .remove("declaration");
            }
            older.set_claim_registry(registry);
            sync(&owner, &older);
            assert!(older.claim_by_id(&accepted.id).unwrap().is_none());
            let code:String=older.readers.get().query_row("SELECT error_code FROM replica_records WHERE claim_id=?1 OR (subject_hint=?2 AND kind_hint=?3)",params![accepted.id,subject,KIND],|row|row.get(0)).unwrap();
            assert_eq!(
                code,
                if unknown_kind {
                    "unknown-claim-kind"
                } else {
                    "unknown-claim-field"
                }
            );
            drop(older);
            let upgraded = Store::open(&path, "birch").unwrap();
            upgraded.set_member_key(Some(peer_key)).unwrap();
            upgraded.validate_replication_backlog().unwrap();
            let mut phases = Vec::new();
            upgraded
                .project_replication_backlog_with_progress("retirement-upgrade", |progress| {
                    phases.push(progress.phase)
                })
                .unwrap();
            assert!(
                phases.iter().all(|phase| !phase.starts_with("full-replay")),
                "terminal admission must project incrementally: {phases:?}"
            );
            assert_eq!(
                upgraded.claim_by_id(&accepted.id).unwrap().unwrap().id,
                accepted.id
            );
            assert_eq!(
                upgraded.agent_retirement(subject).unwrap().unwrap().id,
                accepted.id
            );
            let input = ClaimInput {
                subject: "message/after-upgrade".into(),
                kind: "message.sent".into(),
                actor: Some("person/avery".into()),
                fields: BTreeMap::from([
                    ("from".into(), json!("person/avery")),
                    ("to".into(), json!(subject)),
                    ("content".into(), json!("An invented note")),
                    ("status".into(), json!("sent")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some("after-upgrade".into()),
            };
            assert_eq!(
                upgraded.append_claim(&input).unwrap_err().code,
                "recipient-retired"
            );
        }
    }
}
