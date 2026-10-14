//! Indexed launch evidence usable inside the publication writer transaction.
use super::*;
use crate::model::MemberSpec;
use crate::publication::{Interruption, interruption};

pub(crate) type Guard = dyn Fn(&DesiredSubject, &Check) -> Result<(), St3Error> + Sync;

#[derive(Default)]
pub(crate) struct CommitChecks<'a> {
    pub owned: Option<&'a owned_sets::Options>,
    pub guard: Option<&'a Guard>,
}

pub(crate) struct Check {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "The following refusal slice reports the checked incarnation."
        )
    )]
    pub incarnation: Option<String>,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "The following refusal slice reports the actual launch values."
        )
    )]
    pub launched: Option<MemberSpec>,
    pub interruption: Interruption,
    pub unknown_reason: Option<&'static str>,
}

pub(crate) fn launched_member(
    connection: &Connection,
    subject: &str,
    origin: &str,
    incarnation: &str,
    at: u64,
    local_origin: &str,
) -> Result<Option<MemberSpec>> {
    if !publication_facts::ready(connection)? {
        return Ok(None);
    }
    let query = format!(
        "SELECT claims.store_index, claims.body FROM local_publication_claims AS facts INDEXED BY local_publication_claims_lookup
         JOIN claims ON claims.id=facts.claim_id
         WHERE facts.subject=?1 AND facts.origin=?2 AND facts.incarnation=?3
           AND facts.kind='runtime.action.succeeded'
           AND claims.store_index<=?4 AND {UNREPAIRED_CLAIM} ORDER BY facts.sort_key DESC LIMIT 1"
    );
    let shared: Option<(u64, String)> = connection
        .prepare_cached(&query)?
        .query_row(
            params![subject, origin, incarnation, at.min(i64::MAX as u64)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    // Start successes may be retained locally. Those records sort after their graph cut;
    // the newest local start for this incarnation supersedes an earlier graph start.
    let local: Option<(u64, String)> = if origin == local_origin {
        connection
            .prepare_cached(
                "SELECT local_observations.after_store_index, local_observations.body
                 FROM local_publication_starts AS facts
                 JOIN local_observations ON local_observations.id=facts.observation_id
                 WHERE facts.subject=?1 AND facts.incarnation=?2
                   AND local_observations.after_store_index<=?3
                 ORDER BY facts.observation_id DESC LIMIT 1",
            )?
            .query_row(
                params![subject, incarnation, at.min(i64::MAX as u64)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
    } else {
        None
    };
    let body = shared
        .into_iter()
        .chain(local)
        .max_by_key(|(index, _)| *index)
        .map(|(_, body)| body);
    let Some(body) = body else {
        return Ok(None);
    };
    let body: Value = serde_json::from_str(&body)?;
    let Some(token) = body
        .pointer("/fields/desired_token")
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    let body: Option<String> = connection.prepare_cached(&format!(
        "SELECT body FROM claims WHERE id=?1 AND subject=?2 AND kind='intent.desired' AND {UNREPAIRED_CLAIM}"
    ))?.query_row(params![token,subject], |row| row.get(0)).optional()?;
    Ok(body
        .and_then(|body| serde_json::from_str::<DesiredSubject>(&body).ok())
        .and_then(|desired| desired.member))
}

pub(crate) fn check(
    connection: &Connection,
    proposed: &DesiredSubject,
    at: u64,
    local_origin: &str,
) -> Result<Check> {
    smallclaims::touched::note_read(|| proposed.subject.clone());
    if !publication_facts::ready(connection)? {
        return Ok(Check {
            incarnation: None,
            launched: None,
            interruption: Interruption::Unknown,
            unknown_reason: Some("publication-evidence-incomplete"),
        });
    }
    // Seek the next origin prefix instead of DISTINCT's walk over every historical row.
    // One runtime per origin prevents a stopped peer from hiding a running incarnation.
    let mut origins = connection.prepare_cached(
        "WITH RECURSIVE origins(origin) AS (
            SELECT MIN(origin) FROM local_publication_claims
                INDEXED BY local_publication_runtime_lookup
                WHERE subject=?1 AND kind='runtime.observed'
            UNION ALL
            SELECT (SELECT MIN(origin) FROM local_publication_claims
                INDEXED BY local_publication_runtime_lookup
                WHERE subject=?1 AND kind='runtime.observed' AND origin>origins.origin)
            FROM origins WHERE origin IS NOT NULL
         ) SELECT origin FROM origins WHERE origin IS NOT NULL",
    )?;
    let origins = origins
        .query_map([&proposed.subject], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let query = format!(
        "SELECT claims.body FROM local_publication_claims AS facts
         INDEXED BY local_publication_runtime_lookup JOIN claims ON claims.id=facts.claim_id
         WHERE facts.subject=?1 AND facts.origin=?2 AND facts.kind='runtime.observed'
           AND claims.store_index<=?3 AND {UNREPAIRED_CLAIM} ORDER BY facts.sort_key DESC LIMIT 1"
    );
    let mut unchanged = Check {
        incarnation: None,
        launched: None,
        interruption: Interruption::None,
        unknown_reason: None,
    };
    for origin in origins {
        let body: Option<String> = connection
            .prepare_cached(&query)?
            .query_row(
                params![proposed.subject, origin, at.min(i64::MAX as u64)],
                |row| row.get(0),
            )
            .optional()?;
        let Some(body) = body else {
            continue;
        };
        let body: Value = serde_json::from_str(&body)?;
        let fields = body.get("fields").unwrap_or(&body);
        if fields["status"] != "running" {
            continue;
        }
        let incarnation = fields["incarnation_id"].as_str().map(str::to_owned);
        let launched = incarnation
            .as_deref()
            .map(|incarnation| {
                launched_member(
                    connection,
                    &proposed.subject,
                    &origin,
                    incarnation,
                    at,
                    local_origin,
                )
            })
            .transpose()?
            .flatten();
        let interruption = interruption(proposed, true, incarnation.as_deref(), launched.as_ref());
        let check = Check {
            incarnation,
            launched,
            unknown_reason: (interruption == Interruption::Unknown)
                .then_some("publication-launch-unknown"),
            interruption,
        };
        if check.interruption != Interruption::None {
            return Ok(check);
        }
        unchanged = check;
    }
    Ok(unchanged)
}

impl Store {
    pub(crate) fn publication_cache_epoch(&self) -> Option<(i64, bool)> {
        *self
            .smalltalk
            .publication_cache_epoch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn publication_evidence_epoch(&self, subject: &str) -> Result<(u64, i64, bool)> {
        smallclaims::touched::note_read(|| subject.to_owned());
        Ok(self
            .readers
            .get()
            .prepare_cached(
                "SELECT (SELECT COALESCE(MAX(store_index),0) FROM claims WHERE subject=?1),
                    (SELECT COALESCE(MAX(id),0) FROM local_observations WHERE subject=?1),
                    (SELECT phase=3 FROM local_publication_bootstrap WHERE singleton=1)
                        AND NOT EXISTS(SELECT 1 FROM local_publication_pending)",
            )?
            .query_row([subject], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?)
    }
    pub(crate) fn publication_action(
        &self,
        proposed: &DesiredSubject,
    ) -> Result<Option<PlannedAction>> {
        let connection = self.readers.get();
        let check = check(
            &connection,
            proposed,
            current_index(&connection)?,
            &self.origin,
        )?;
        let (action, reason) = match check.interruption {
            Interruption::None => return Ok(None),
            Interruption::Stop => (
                "stop",
                "the publication would stop a running incarnation".into(),
            ),
            Interruption::Restart(changes) => (
                "restart",
                format!(
                    "the launched {} changed; a running seat restarts on its last session",
                    changes.join(" and ")
                ),
            ),
            Interruption::Unknown => (
                "observe-or-start",
                check
                    .unknown_reason
                    .unwrap_or("publication-launch-unknown")
                    .into(),
            ),
        };
        Ok(Some(PlannedAction {
            subject: proposed.subject.clone(),
            action: action.into(),
            reason,
        }))
    }
    pub(crate) fn publication_launch(
        &self,
        subject: &str,
        incarnation: &str,
    ) -> Result<Option<MemberSpec>> {
        launched_member(
            &self.readers.get(),
            subject,
            &self.origin,
            incarnation,
            i64::MAX as u64,
            &self.origin,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::parse_intent;

    fn declaration(command: &str) -> NormalizedIntent {
        parse_intent(&format!("version 2\nagent \"example/worker\" {{ workspace \"/tmp\"; command \"{command}\"; }}"), "node").unwrap()
    }

    fn publish(store: &Store, intent: &NormalizedIntent, key: &str) -> ApplyResponse {
        let preview = store
            .mission(
                intent,
                IntentInput {
                    kdl: String::new(),
                    source_name: None,
                },
            )
            .unwrap();
        store.apply(intent, &preview.subject_tokens, key).unwrap()
    }

    fn observe(store: &Store, incarnation: &str, token: Option<&str>) {
        if let Some(token) = token {
            store
                .append_claim(&ClaimInput {
                    subject: "agent/example/worker".into(),
                    kind: "runtime.action.succeeded".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("action".into(), json!("start")),
                        ("incarnation_id".into(), json!(incarnation)),
                        ("desired_token".into(), json!(token)),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        store
            .append_claim(&ClaimInput {
                subject: "agent/example/worker".into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("status".into(), json!("running")),
                    ("incarnation_id".into(), json!(incarnation)),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }

    #[test]
    fn publication_check_uses_launch_not_selected_declaration() {
        let store = Store::open_memory("node").unwrap();
        let original = declaration("true");
        let applied = publish(&store, &original, "original");
        let token = &applied.subject_tokens["agent/example/worker"][0];
        observe(&store, "one", Some(token));
        let drift = declaration("false");
        publish(&store, &drift, "drift");
        let preview = store
            .mission(
                &drift,
                IntentInput {
                    kdl: String::new(),
                    source_name: None,
                },
            )
            .unwrap();
        assert!(preview.changes.is_empty());
        assert_eq!(preview.predicted_actions[0].action, "restart");
        let connection = store.readers.get();
        let check = check(
            &connection,
            &drift.subjects["agent/example/worker"],
            u64::MAX,
            "node",
        )
        .unwrap();
        assert_eq!(check.interruption, Interruption::Restart(vec!["launch"]));
        assert_eq!(check.incarnation.as_deref(), Some("one"));
        assert_eq!(
            check.launched.unwrap(),
            original.subjects["agent/example/worker"]
                .member
                .clone()
                .unwrap()
        );
    }

    #[test]
    fn publication_guard_reads_the_replacement_incarnation_before_writing() {
        let store = Store::open_memory("node").unwrap();
        let original = declaration("true");
        let applied = publish(&store, &original, "original");
        observe(
            &store,
            "one",
            Some(&applied.subject_tokens["agent/example/worker"][0]),
        );
        let proposed = declaration("true");
        let preview = store
            .mission(
                &proposed,
                IntentInput {
                    kdl: String::new(),
                    source_name: None,
                },
            )
            .unwrap();
        assert!(preview.predicted_actions.is_empty());
        // Matching old launch evidence is not evidence for a new runtime.
        observe(&store, "two", None);
        let before = store.index().unwrap();
        let error = store
            .apply_as_checked(
                &proposed,
                &preview.subject_tokens,
                "guard",
                None,
                &|_, check| {
                    assert_eq!(check.incarnation.as_deref(), Some("two"));
                    if check.interruption == Interruption::Unknown {
                        Err(St3Error::new("conflict", "unknown launch"))
                    } else {
                        Ok(())
                    }
                },
            )
            .unwrap_err();
        assert_eq!(error.code, "conflict");
        assert_eq!(store.index().unwrap(), before);
    }
    #[test]
    fn publication_guard_refuses_launch_drift_in_a_partial_bundle() {
        let store = Store::open_memory("node").unwrap();
        let original = declaration("true");
        let applied = publish(&store, &original, "original");
        let original_token = applied.subject_tokens["agent/example/worker"][0].clone();
        let changed = declaration("false");
        let changed_apply = publish(&store, &changed, "changed");
        observe(
            &store,
            "two",
            Some(&changed_apply.subject_tokens["agent/example/worker"][0]),
        );
        let mut bundle = changed.clone();
        let addition = parse_intent("version 2\nhost \"example-host\" {}", "node").unwrap();
        bundle.subjects.extend(addition.subjects);
        let preview = store
            .mission(
                &bundle,
                IntentInput {
                    kdl: String::new(),
                    source_name: None,
                },
            )
            .unwrap();
        observe(&store, "one", Some(&original_token));
        let before = store.index().unwrap();
        let refused = store
            .apply_as_checked(
                &bundle,
                &preview.subject_tokens,
                "guard-bundle",
                None,
                &|_, check| {
                    if check.interruption != Interruption::None {
                        Err(St3Error::new("conflict", "interrupting publication"))
                    } else {
                        Ok(())
                    }
                },
            )
            .unwrap_err();
        assert_eq!(refused.code, "conflict");
        assert_eq!(store.index().unwrap(), before);
        assert!(
            store
                .desired_subject_with_writer("host/example-host")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn publication_check_stopped_origin_cannot_hide_another_origins_running_seat() {
        let local = Store::open_memory("node").unwrap();
        let peer = Store::open_memory("peer").unwrap();
        let original = declaration("true");
        let applied = publish(&peer, &original, "peer-original");
        peer.append_claim(&ClaimInput {
            subject: "agent/example/worker".into(),
            kind: "runtime.action.succeeded".into(),
            actor: Some("agent/example/writer".into()),
            fields: BTreeMap::from([
                ("action".into(), json!("start")),
                ("incarnation_id".into(), json!("peer-inc")),
                (
                    "desired_token".into(),
                    json!(applied.subject_tokens["agent/example/worker"][0]),
                ),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
        peer.append_claim(&ClaimInput {
            subject: "agent/example/worker".into(),
            kind: "runtime.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("status".into(), json!("running")),
                ("incarnation_id".into(), json!("peer-inc")),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
        local
            .import_replication("peer", &peer.export_replication(0).unwrap())
            .unwrap();
        local
            .append_claim(&ClaimInput {
                subject: "agent/example/worker".into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("status".into(), json!("stopped")),
                    ("incarnation_id".into(), json!("node-inc")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        while !local.advance_publication_evidence().unwrap() {}
        let proposed = declaration("false");
        let judgment = check(
            &local.readers.get(),
            &proposed.subjects["agent/example/worker"],
            u64::MAX,
            "node",
        )
        .unwrap();
        assert!(matches!(judgment.interruption, Interruption::Restart(_)));
        assert_eq!(judgment.incarnation.as_deref(), Some("peer-inc"));
    }

    #[test]
    #[ignore = "process-wide SQL counters: run alone with --exact --ignored --test-threads=1"]
    fn publication_check_cost_is_independent_of_unrelated_and_old_incarnation_history() {
        use smallclaims::sqlite::work;
        let measure = |count: u64| {
            let store = Store::open_memory("node").unwrap();
            let intent = declaration("true");
            let applied = publish(&store, &intent, "original");
            observe(
                &store,
                "current",
                Some(&applied.subject_tokens["agent/example/worker"][0]),
            );
            let mut connection = store.connection.write();
            let tx = connection.transaction().unwrap();
            tx.execute(
                "INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms)
                VALUES ('noise-batch','node',100000,'synthetic','1')",
                [],
            )
            .unwrap();
            for index in 0..count {
                let (subject, kind, body) = match index % 3 {
                    0 => (
                        "agent/unrelated",
                        "harness.observed",
                        json!({"fields":{"state":"idle"}}),
                    ),
                    1 => (
                        "agent/example/worker",
                        "runtime.observed",
                        json!({"fields":{"status":"running","incarnation_id":"old"}}),
                    ),
                    _ => (
                        "agent/example/worker",
                        "runtime.action.succeeded",
                        json!({"fields":{"action":"start","incarnation_id":"old","desired_token":"missing"}}),
                    ),
                };
                tx.execute("INSERT INTO claims(store_index,id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
                    VALUES (?1,?2,'noise-batch',?3,?4,'node',?5,'[]','1')",
                    params![100000 + index, format!("noise-{index}"), subject, kind, body.to_string()]).unwrap();
            }
            tx.commit().unwrap();
            drop(connection);
            while !store.advance_publication_evidence().unwrap() {}
            let connection = store.readers.get();
            let before = work::total();
            let started = std::time::Instant::now();
            let result = check(
                &connection,
                &intent.subjects["agent/example/worker"],
                u64::MAX,
                "node",
            )
            .unwrap();
            let elapsed = started.elapsed();
            let cost = work::total() - before;
            assert_eq!(result.interruption, Interruption::None);
            assert_eq!(result.incarnation.as_deref(), Some("current"));
            println!("publication check history={count}: {cost:?}, elapsed={elapsed:?}");
            assert!(elapsed < std::time::Duration::from_millis(100));
            cost
        };
        let small = measure(1000);
        let large = measure(10000);
        assert!(
            small.vm_steps > 0 && large.vm_steps <= small.vm_steps + 100,
            "small={small:?}, large={large:?}"
        );
        assert_eq!(large.fullscan_steps, 0);
    }
}
