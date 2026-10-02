//! Subagents: helpers a seat's harness runs inside its own session, recorded as claims on the
//! parent seat. A subagent is not a seat or a mission.
//!
//! The seat's driver records `subagent.appeared` when its harness starts one, `subagent.renewed`
//! while it runs and its lease nears its end, and `subagent.ended` when the harness reports its
//! end. A subagent is open from its appearance until its end. The node that recorded an
//! appearance also ends the subagent when its lease runs out, its harness exits or restarts, or
//! its seat is stopped or removed, so a subagent never outlives what started it. On any node, an
//! open subagent whose lease ran out reads as expired.

use super::*;

/// How long a subagent's lease lasts from its appearance or renewal.
pub const SUBAGENT_LEASE_MS: u64 = 10 * 60 * 1000;

/// The kinds that make up a subagent's record on its parent seat.
pub const SUBAGENT_KINDS: [&str; 3] = ["subagent.appeared", "subagent.renewed", "subagent.ended"];

/// One open subagent: appeared and not yet ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SubagentView {
    /// The parent seat.
    pub agent: String,
    /// The harness's own ID for the subagent, unique within its parent seat.
    pub subagent_id: String,
    pub subagent_type: Option<String>,
    pub description: Option<String>,
    pub driver: String,
    pub session_id: Option<String>,
    pub incarnation_id: String,
    /// The step the parent held when the subagent appeared.
    pub step_run: Option<String>,
    pub started_at_unix_ms: u64,
    pub lease_expires_at_unix_ms: u64,
    /// The `subagent.appeared` claim, and the node that recorded it.
    pub appeared: String,
    pub origin: String,
    #[serde(skip)]
    pub store_index: u64,
}

impl SubagentView {
    pub fn expired_at(&self, now: u64) -> bool {
        self.lease_expires_at_unix_ms <= now
    }
}

/// A subagent this node ended because what started it went away or its lease ran out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EndedSubagent {
    pub agent: String,
    pub subagent_id: String,
    pub outcome: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SubagentSweep {
    pub ended: Vec<EndedSubagent>,
    /// The earliest lease that still runs on this node, when the sweep should look again.
    pub next_expiry_unix_ms: Option<u64>,
    /// Every subagent this node recorded before this claim has ended, so the next sweep starts
    /// here. An end is never undone and new appearances sort later.
    pub low_water: u64,
}

fn text(fields: &Value, name: &str) -> Option<String> {
    fields
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Which open subagents a read wants.
#[derive(Clone, Copy)]
enum Scope<'a> {
    Seat(&'a str),
    /// Recorded on this node from this claim on.
    RecordedOn(&'a str, u64),
    All,
}

/// Open subagents in `scope`, oldest first in the canonical claim order.
fn open_subagents_at(connection: &Connection, scope: Scope<'_>) -> Result<Vec<SubagentView>> {
    let filter = match scope {
        Scope::Seat(_) => "AND claims.subject=?1",
        // The sweep's own bound on this node's claims, not an order.
        Scope::RecordedOn(..) => "AND claims.origin=?1 AND claims.store_index>=?2",
        Scope::All => "",
    };
    let mut statement = connection.prepare_cached(&canonical_sql(&format!(
        "SELECT claims.subject, claims.id, claims.origin, claims.body, claims.store_index,
             (SELECT MAX(CAST(json_extract(renewed.body, '$.fields.lease_expires_at_unix_ms')
                  AS INTEGER))
              FROM claims renewed
              WHERE renewed.kind='subagent.renewed' AND renewed.subject=claims.subject
                AND json_extract(renewed.body, '$.fields.subagent_id')
                    =json_extract(claims.body, '$.fields.subagent_id'))
         FROM claims
         WHERE claims.kind='subagent.appeared' {filter}
           AND NOT EXISTS (
               SELECT 1 FROM claims ended
               WHERE ended.kind='subagent.ended' AND ended.subject=claims.subject
                 AND json_extract(ended.body, '$.fields.subagent_id')
                     =json_extract(claims.body, '$.fields.subagent_id'))
         ORDER BY CANONICAL_ASC(claims)"
    )))?;
    let read = |row: &rusqlite::Row<'_>| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, u64>(4)?,
            row.get::<_, Option<i64>>(5)?,
        ))
    };
    let rows = match scope {
        Scope::Seat(agent) => statement.query_map([agent], read)?.collect::<Vec<_>>(),
        Scope::RecordedOn(origin, after) => statement
            .query_map(params![origin, after], read)?
            .collect::<Vec<_>>(),
        Scope::All => statement.query_map([], read)?.collect::<Vec<_>>(),
    };
    let mut open = Vec::new();
    for row in rows {
        let (agent, appeared, origin, body, store_index, renewed) = row?;
        let body: Value = serde_json::from_str(&body)?;
        let fields = &body["fields"];
        let Some(subagent_id) = text(fields, "subagent_id") else {
            continue;
        };
        let leased = fields["lease_expires_at_unix_ms"].as_u64().unwrap_or(0);
        open.push(SubagentView {
            agent,
            subagent_id,
            subagent_type: text(fields, "subagent_type"),
            description: text(fields, "description"),
            driver: text(fields, "driver").unwrap_or_default(),
            session_id: text(fields, "session_id"),
            incarnation_id: text(fields, "incarnation_id").unwrap_or_default(),
            step_run: text(fields, "step_run"),
            started_at_unix_ms: fields["started_at_unix_ms"].as_u64().unwrap_or(0),
            lease_expires_at_unix_ms: leased.max(renewed.map_or(0, |at| at.max(0) as u64)),
            appeared,
            origin,
            store_index,
        });
    }
    Ok(open)
}

/// The claim of `kind` that names `subagent_id` on `agent`, if any.
fn subagent_claim_tx(
    transaction: &Transaction<'_>,
    agent: &str,
    kind: &str,
    subagent_id: &str,
) -> Result<Option<ClaimRecord>, St3Error> {
    debug_assert!(SUBAGENT_KINDS.contains(&kind));
    // The kind is written into the statement so SQLite can use that kind's partial index.
    transaction
        .query_row(
            &format!(
                "SELECT {CLAIM_COLUMNS} FROM claims JOIN batches ON batches.id=claims.batch_id
                 WHERE claims.kind='{kind}' AND claims.subject=?1
                   AND json_extract(claims.body, '$.fields.subagent_id')=?2
                 ORDER BY {CANONICAL_ORDER} LIMIT 1"
            ),
            params![agent, subagent_id],
            claim_from_row,
        )
        .optional()
        .map_err(internal)
}

/// Keep each subagent's record whole: one appearance, renewals only while it is open, and one end.
/// A repeated appearance or end answers with the claim already recorded. Returns that claim when
/// the input adds nothing.
pub(super) fn check_subagent_claim_tx(
    transaction: &Transaction<'_>,
    input: &ClaimInput,
) -> Result<Option<ClaimRecord>, St3Error> {
    if !SUBAGENT_KINDS.contains(&input.kind.as_str()) {
        return Ok(None);
    }
    let Some(subagent_id) = input
        .fields
        .get("subagent_id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
    else {
        return Err(St3Error::new(
            "invalid-subagent",
            "a subagent claim needs the harness's subagent ID",
        ));
    };
    let appeared = subagent_claim_tx(
        transaction,
        &input.subject,
        "subagent.appeared",
        subagent_id,
    )?;
    if input.kind == "subagent.appeared" {
        return Ok(appeared);
    }
    if appeared.is_none() {
        return Err(St3Error::new(
            "unknown-subagent",
            format!(
                "subagent `{subagent_id}` never appeared on `{}`",
                input.subject
            ),
        ));
    }
    let ended = subagent_claim_tx(transaction, &input.subject, "subagent.ended", subagent_id)?;
    match (input.kind.as_str(), ended) {
        ("subagent.ended", ended) => Ok(ended),
        (_, Some(_)) => Err(St3Error::new(
            "subagent-ended",
            format!(
                "subagent `{subagent_id}` on `{}` already ended",
                input.subject
            ),
        )),
        (_, None) => Ok(None),
    }
}

/// The newest runtime record of a seat: its status and incarnation.
fn seat_runtime(connection: &Connection, seat: &str) -> Result<Option<(String, Option<String>)>> {
    let mut statement = connection.prepare_cached(&canonical_sql(
        "SELECT body FROM claims
         WHERE subject=?1 AND kind='runtime.observed'
         ORDER BY CANONICAL_DESC(claims)",
    ))?;
    let bodies = statement.query_map([seat], |row| row.get::<_, String>(0))?;
    for body in bodies {
        let body: Value = serde_json::from_str(&body?)?;
        let fields = body.get("fields").unwrap_or(&body);
        if let Some(status) = text(fields, "status") {
            return Ok(Some((status, text(fields, "incarnation_id"))));
        }
    }
    Ok(None)
}

/// What the sweep needs to know about a parent seat: its declaration's kind and its newest
/// runtime status and incarnation.
struct Seat {
    declared: Option<String>,
    runtime: Option<(String, Option<String>)>,
}

fn seat(connection: &Connection, agent: &str) -> Result<Seat> {
    Ok(Seat {
        declared: connection
            .query_row(
                "SELECT kind FROM desired WHERE subject=?1",
                [agent],
                |row| row.get(0),
            )
            .optional()?,
        runtime: seat_runtime(connection, agent)?,
    })
}

/// Why an open subagent must end now, as its outcome and reason, or `None` while it may run.
fn ending(seat: &Seat, subagent: &SubagentView, now: u64) -> Option<(&'static str, String)> {
    if seat.declared.as_deref() != Some("agent") {
        return Some(("seat-stopped", "its seat was stopped or removed".into()));
    }
    match &seat.runtime {
        Some((status, _)) if status == "stopped" => {
            return Some(("seat-stopped", "its seat was stopped".into()));
        }
        Some((status, _))
            if matches!(status.as_str(), "exited" | "vanished" | "absent" | "failed") =>
        {
            return Some(("harness-exited", format!("its harness {status}")));
        }
        Some((_, Some(incarnation))) if *incarnation != subagent.incarnation_id => {
            return Some((
                "harness-exited",
                format!("its harness restarted as incarnation {incarnation}"),
            ));
        }
        _ => {}
    }
    subagent
        .expired_at(now)
        .then(|| ("expired", "its lease ran out without a renewal".into()))
}

impl Store {
    /// A seat's open subagents, oldest first. One whose lease ran out still shows until the node
    /// that recorded it ends it; `SubagentView::expired_at` tells it apart.
    pub fn open_subagents(&self, agent: &str) -> Result<Vec<SubagentView>> {
        open_subagents_at(&self.readers.get(), Scope::Seat(agent))
    }

    /// Every open subagent, oldest first.
    pub fn all_open_subagents(&self) -> Result<Vec<SubagentView>> {
        open_subagents_at(&self.readers.get(), Scope::All)
    }

    /// End each subagent recorded on this node from `low_water` on whose seat was stopped or
    /// removed, whose harness exited or restarted, or whose lease ran out.
    pub fn end_stale_subagents(&self, now: u64, low_water: u64) -> Result<SubagentSweep, St3Error> {
        let mut sweep = SubagentSweep {
            low_water,
            ..SubagentSweep::default()
        };
        let mut stale = Vec::new();
        let mut running = None::<u64>;
        {
            let connection = self.readers.get();
            sweep.low_water = connection
                .query_row(
                    "SELECT COALESCE(MAX(store_index), 0) + 1 FROM claims",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .map_err(internal)?
                .max(low_water);
            let mut seats = BTreeMap::<String, Seat>::new();
            for subagent in
                open_subagents_at(&connection, Scope::RecordedOn(&self.origin, low_water))
                    .map_err(internal)?
            {
                if !seats.contains_key(&subagent.agent) {
                    let state = seat(&connection, &subagent.agent).map_err(internal)?;
                    seats.insert(subagent.agent.clone(), state);
                }
                match ending(&seats[&subagent.agent], &subagent, now) {
                    Some((outcome, reason)) => stale.push((subagent, outcome, reason)),
                    None => {
                        let next = sweep
                            .next_expiry_unix_ms
                            .get_or_insert(subagent.lease_expires_at_unix_ms);
                        *next = (*next).min(subagent.lease_expires_at_unix_ms);
                        let oldest = running.get_or_insert(subagent.store_index);
                        *oldest = (*oldest).min(subagent.store_index);
                    }
                }
            }
        }
        // A subagent that failed to end below stays in the next sweep.
        let mut failed = None::<St3Error>;
        for (subagent, outcome, reason) in stale {
            let mut fields = BTreeMap::from([
                (
                    "subagent_id".into(),
                    Value::String(subagent.subagent_id.clone()),
                ),
                ("outcome".into(), Value::String(outcome.into())),
                ("reason".into(), Value::String(reason.clone())),
                ("ended_at_unix_ms".into(), Value::from(now)),
            ]);
            if subagent.started_at_unix_ms > 0 {
                fields.insert(
                    "duration_ms".into(),
                    Value::from(now.saturating_sub(subagent.started_at_unix_ms)),
                );
            }
            let ended = self.append_claim_outcome(&ClaimInput {
                subject: subagent.agent.clone(),
                kind: "subagent.ended".into(),
                actor: None,
                fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!(
                    "subagent-sweep:{}:{}",
                    subagent.agent, subagent.subagent_id
                )),
            });
            match ended {
                Ok((_, true)) => sweep.ended.push(EndedSubagent {
                    agent: subagent.agent,
                    subagent_id: subagent.subagent_id,
                    outcome: outcome.into(),
                    reason,
                }),
                Ok((_, false)) => {}
                Err(error) => {
                    let oldest = running.get_or_insert(subagent.store_index);
                    *oldest = (*oldest).min(subagent.store_index);
                    failed.get_or_insert(error);
                }
            }
        }
        if let Some(oldest) = running {
            sweep.low_water = oldest;
        }
        failed.map_or(Ok(sweep), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEAT: &str = "agent/alder.parent";

    fn declare(store: &Store, seat: &str) {
        let name = seat.trim_start_matches("agent/");
        let intent = crate::graph::parse_internal_intent(
            &format!("version 2\nagent {name:?} {{ workspace \"/tmp\"; command \"true\"; }}"),
            "alder",
        )
        .unwrap();
        store
            .apply_internal(&intent, &format!("declare-{seat}"))
            .unwrap();
    }

    fn stop(store: &Store, seat: &str) {
        let intent =
            crate::graph::parse_internal_intent(&format!("version 2\nstop {seat:?}\n"), "alder")
                .unwrap();
        store
            .apply_internal(&intent, &format!("stop-{seat}"))
            .unwrap();
    }

    fn runtime(store: &Store, seat: &str, status: &str, incarnation: &str) {
        store
            .append_claim(&ClaimInput {
                subject: seat.into(),
                kind: "runtime.observed".into(),
                actor: Some(seat.into()),
                fields: BTreeMap::from([
                    ("status".into(), json!(status)),
                    ("runtime_id".into(), json!("runtime-1")),
                    ("incarnation_id".into(), json!(incarnation)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }

    fn claim(
        store: &Store,
        kind: &str,
        id: &str,
        extra: &[(&str, Value)],
    ) -> Result<(ClaimRecord, bool), St3Error> {
        let mut fields = BTreeMap::from([("subagent_id".into(), json!(id))]);
        for (name, value) in extra {
            fields.insert((*name).into(), value.clone());
        }
        store.append_client_claim_outcome(&ClaimInput {
            subject: SEAT.into(),
            kind: kind.into(),
            actor: Some(SEAT.into()),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
    }

    fn appear(store: &Store, id: &str, lease: u64) -> Result<(ClaimRecord, bool), St3Error> {
        claim(
            store,
            "subagent.appeared",
            id,
            &[
                ("subagent_type", json!("Explore")),
                ("description", json!("map the code")),
                ("driver", json!("claude")),
                ("session_id", json!("session-1")),
                ("incarnation_id", json!("inc-1")),
                ("step_run", json!("step-run/run-1/claims")),
                ("started_at_unix_ms", json!(1_000)),
                ("lease_expires_at_unix_ms", json!(lease)),
            ],
        )
    }

    fn running_seat() -> Store {
        let store = Store::open_memory("alder").unwrap();
        declare(&store, SEAT);
        runtime(&store, SEAT, "running", "inc-1");
        store
    }

    fn ids(store: &Store) -> Vec<String> {
        store
            .open_subagents(SEAT)
            .unwrap()
            .into_iter()
            .map(|subagent| subagent.subagent_id)
            .collect()
    }

    #[test]
    fn a_subagent_is_open_from_its_appearance_to_its_end() {
        let store = running_seat();
        assert!(appear(&store, "a1", 50_000).unwrap().1);
        let open = store.open_subagents(SEAT).unwrap();
        assert_eq!(open.len(), 1);
        let subagent = &open[0];
        assert_eq!(subagent.agent, SEAT);
        assert_eq!(subagent.subagent_type.as_deref(), Some("Explore"));
        assert_eq!(subagent.description.as_deref(), Some("map the code"));
        assert_eq!(subagent.step_run.as_deref(), Some("step-run/run-1/claims"));
        assert_eq!(subagent.session_id.as_deref(), Some("session-1"));
        assert_eq!(subagent.lease_expires_at_unix_ms, 50_000);
        assert_eq!(subagent.origin, "alder");
        assert!(subagent.expired_at(50_000) && !subagent.expired_at(49_999));

        // A repeated appearance answers with the first and records nothing.
        let (again, appended) = appear(&store, "a1", 90_000).unwrap();
        assert!(!appended);
        assert_eq!(again.id, subagent.appeared);

        let lease = [("lease_expires_at_unix_ms", json!(80_000))];
        assert!(claim(&store, "subagent.renewed", "a1", &lease).unwrap().1);
        assert_eq!(
            store.open_subagents(SEAT).unwrap()[0].lease_expires_at_unix_ms,
            80_000
        );

        let end = [("outcome", json!("completed")), ("total_tokens", json!(12))];
        let (ended, appended) = claim(&store, "subagent.ended", "a1", &end).unwrap();
        assert!(appended);
        assert!(ids(&store).is_empty());
        // A second end answers with the first; a renewal after the end is refused.
        let (again, appended) = claim(
            &store,
            "subagent.ended",
            "a1",
            &[("outcome", json!("expired"))],
        )
        .unwrap();
        assert!(!appended);
        assert_eq!(again.id, ended.id);
        assert_eq!(
            claim(&store, "subagent.renewed", "a1", &lease)
                .unwrap_err()
                .code,
            "subagent-ended"
        );
        // A subagent that never appeared can be neither renewed nor ended.
        for (kind, field) in [
            ("subagent.renewed", ("lease_expires_at_unix_ms", json!(1))),
            ("subagent.ended", ("outcome", json!("completed"))),
        ] {
            assert_eq!(
                claim(&store, kind, "never", &[field]).unwrap_err().code,
                "unknown-subagent"
            );
        }
        assert_eq!(
            claim(
                &store,
                "subagent.ended",
                "a1",
                &[("outcome", json!("vanished"))]
            )
            .unwrap_err()
            .code,
            "invalid-claim-field"
        );
    }

    #[test]
    fn a_seat_records_only_its_own_subagents() {
        let store = running_seat();
        let error = store
            .append_client_claim_outcome(&ClaimInput {
                subject: SEAT.into(),
                kind: "subagent.appeared".into(),
                actor: Some("agent/alder.other".into()),
                fields: BTreeMap::from([
                    ("subagent_id".into(), json!("a1")),
                    ("driver".into(), json!("claude")),
                    ("incarnation_id".into(), json!("inc-1")),
                    ("lease_expires_at_unix_ms".into(), json!(1)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap_err();
        assert_eq!(error.code, "claim-write-forbidden");
    }

    #[test]
    fn the_sweep_ends_an_unrenewed_subagent_and_waits_for_a_running_one() {
        let store = running_seat();
        appear(&store, "short", 10_000).unwrap();
        appear(&store, "long", 30_000).unwrap();
        let sweep = store.end_stale_subagents(9_999, 0).unwrap();
        assert!(sweep.ended.is_empty());
        assert_eq!(sweep.next_expiry_unix_ms, Some(10_000));

        let sweep = store.end_stale_subagents(10_000, sweep.low_water).unwrap();
        assert_eq!(
            sweep.ended,
            [EndedSubagent {
                agent: SEAT.into(),
                subagent_id: "short".into(),
                outcome: "expired".into(),
                reason: "its lease ran out without a renewal".into(),
            }]
        );
        assert_eq!(sweep.next_expiry_unix_ms, Some(30_000));
        assert_eq!(ids(&store), ["long"]);
        let ended = store
            .graph
            .claims_for(SEAT, Some("subagent.ended"))
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(ended.actor, None);
        assert_eq!(ended.body["fields"]["duration_ms"], 9_000);

        // A renewal keeps it running past its first lease.
        claim(
            &store,
            "subagent.renewed",
            "long",
            &[("lease_expires_at_unix_ms", json!(60_000))],
        )
        .unwrap();
        let sweep = store.end_stale_subagents(45_000, sweep.low_water).unwrap();
        assert!(sweep.ended.is_empty());
        assert_eq!(sweep.next_expiry_unix_ms, Some(60_000));
    }

    #[test]
    fn the_sweep_ends_subagents_whose_harness_exited_or_restarted() {
        let store = running_seat();
        appear(&store, "a1", u64::MAX / 2).unwrap();
        runtime(&store, SEAT, "running", "inc-2");
        let sweep = store.end_stale_subagents(5_000, 0).unwrap();
        assert_eq!(sweep.ended.len(), 1);
        assert_eq!(sweep.ended[0].outcome, "harness-exited");
        assert_eq!(
            sweep.ended[0].reason,
            "its harness restarted as incarnation inc-2"
        );

        let store = running_seat();
        appear(&store, "a1", u64::MAX / 2).unwrap();
        runtime(&store, SEAT, "exited", "inc-1");
        let sweep = store.end_stale_subagents(5_000, 0).unwrap();
        assert_eq!(sweep.ended[0].outcome, "harness-exited");
        assert_eq!(sweep.ended[0].reason, "its harness exited");
    }

    #[test]
    fn the_sweep_ends_the_subagents_of_a_stopped_seat() {
        let store = running_seat();
        appear(&store, "a1", u64::MAX / 2).unwrap();
        appear(&store, "a2", u64::MAX / 2).unwrap();
        stop(&store, SEAT);
        let sweep = store.end_stale_subagents(5_000, 0).unwrap();
        assert_eq!(
            sweep
                .ended
                .iter()
                .map(|ended| (ended.subagent_id.as_str(), ended.outcome.as_str()))
                .collect::<Vec<_>>(),
            [("a1", "seat-stopped"), ("a2", "seat-stopped")]
        );
        assert!(ids(&store).is_empty());
        assert_eq!(sweep.next_expiry_unix_ms, None);
    }

    #[test]
    fn the_sweep_starts_after_what_has_ended_and_still_finds_new_subagents() {
        let store = running_seat();
        for index in 0..60 {
            appear(&store, &format!("many-{index}"), 1_000).unwrap();
        }
        let sweep = store.end_stale_subagents(1_000, 0).unwrap();
        assert_eq!(sweep.ended.len(), 60);
        assert!(store.all_open_subagents().unwrap().is_empty());
        let low_water = sweep.low_water;
        appear(&store, "later", 2_000).unwrap();
        let sweep = store.end_stale_subagents(1_500, low_water).unwrap();
        assert!(sweep.ended.is_empty());
        assert_eq!(sweep.next_expiry_unix_ms, Some(2_000));
        // The open subagent holds the low water at itself.
        assert!(sweep.low_water > low_water);
        let open = store.open_subagents(SEAT).unwrap();
        assert_eq!(sweep.low_water, open[0].store_index);
        let sweep = store.end_stale_subagents(2_000, sweep.low_water).unwrap();
        assert_eq!(sweep.ended.len(), 1);
    }
}
