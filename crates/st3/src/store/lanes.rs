//! Lanes: reading one lane from its declaration and claims, finding a lane by a short name, and
//! recording one change. `crate::lane` holds the replay; this module feeds it.

use super::*;
use crate::lane::{self, Change, Event};
use crate::model::{LaneChangeRequest, LaneChangeResponse, LaneSpec, LaneView};

/// How many joins, leaves, moves, and approvals a lane view lists.
const LANE_RECENT_LIMIT: usize = 20;

impl Store {
    /// Every declared lane, open ones first. Closed lanes are listed only with `include_closed`.
    pub fn lanes(&self, include_closed: bool) -> Result<Vec<LaneView>> {
        let connection = self.readers.get();
        let mut views = Vec::new();
        for declaration in lane_declarations_tx(&connection, None)? {
            let view = lane_view_tx(&connection, &declaration)?;
            if view.open || include_closed {
                views.push(view);
            }
        }
        views.sort_by(|left, right| {
            right
                .open
                .cmp(&left.open)
                .then_with(|| left.subject.cmp(&right.subject))
        });
        Ok(views)
    }

    /// The lanes one mission run declares, open or closed.
    pub fn lanes_for_run(&self, run: &str) -> Result<Vec<LaneView>> {
        let run = normalize_mission_run(run);
        let connection = self.readers.get();
        lane_declarations_tx(&connection, Some(&run))?
            .iter()
            .map(|declaration| lane_view_tx(&connection, declaration))
            .collect()
    }

    /// One lane by its exact subject.
    pub fn lane(&self, subject: &str) -> Result<Option<LaneView>> {
        let connection = self.readers.get();
        let declaration = connection
            .query_row(
                "SELECT subject, kind, body, member, owner_run, owner_generation, owner_step
                 FROM desired WHERE subject=?1 AND kind='lane'",
                [subject],
                desired_from_row,
            )
            .optional()?;
        declaration
            .map(|declaration| lane_view_tx(&connection, &declaration))
            .transpose()
    }

    /// Find the lane an argument names: an exact `lane/...` subject, `RUN/NAME`, a mission run
    /// that declares exactly one open lane, a mission whose open lanes are exactly one, or a lane
    /// name that exactly one open lane uses.
    pub fn resolve_lane(&self, argument: &str) -> Result<String, St3Error> {
        let argument = argument.trim();
        if argument.starts_with("lane/") {
            return match self.lane(argument).map_err(internal)? {
                Some(view) => Ok(view.subject),
                None => Err(lane_not_found(argument)),
            };
        }
        let open = self.lanes(false).map_err(internal)?;
        let exact = format!("lane/{argument}");
        if let Some(view) = open.iter().find(|view| view.subject == exact) {
            return Ok(view.subject.clone());
        }
        let run = normalize_mission_run(argument);
        let mission = format!(
            "mission/{}",
            argument.strip_prefix("mission/").unwrap_or(argument)
        );
        type Rule = fn(&LaneView, &str, &str, &str) -> bool;
        let rules: [Rule; 3] = [
            |view, _, run, _| view.run.as_deref() == Some(run),
            |view, _, _, mission| view.mission.as_deref() == Some(mission),
            |view, name, _, _| view.name == name,
        ];
        for matches in rules {
            let found = open
                .iter()
                .filter(|view| matches(view, argument, &run, &mission))
                .collect::<Vec<_>>();
            match found.as_slice() {
                [] => {}
                [view] => return Ok(view.subject.clone()),
                many => {
                    return Err(St3Error::new(
                        "ambiguous-lane",
                        format!(
                            "`{argument}` names {} open lanes; use one subject: {}",
                            many.len(),
                            many.iter()
                                .map(|view| view.subject.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    ));
                }
            }
        }
        Err(lane_not_found(argument))
    }

    /// Record one join, leave, move, mark, or approval. A join of an entry that is already in the
    /// lane records nothing.
    pub fn change_lane(&self, request: &LaneChangeRequest) -> Result<LaneChangeResponse, St3Error> {
        let lane_subject = self.resolve_lane(&request.lane)?;
        let view = self
            .lane(&lane_subject)
            .map_err(internal)?
            .ok_or_else(|| lane_not_found(&lane_subject))?;
        let actor = request.actor.trim();
        if !(actor.starts_with("person/") || actor.starts_with("agent/")) || actor.contains(' ') {
            return Err(St3Error::new(
                "invalid-lane-actor",
                "a lane change is made by a `person/` or `agent/` actor",
            ));
        }
        let entry = lane_entry(&view, &request.entry)?;
        let text = |value: &Option<String>| {
            value
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        let mut fields = BTreeMap::from([("entry".to_owned(), Value::String(entry.clone()))]);
        if let Some(reason) = text(&request.reason) {
            fields.insert("reason".into(), Value::String(reason));
        }
        let mut anchor = None;
        let kind = match request.change.as_str() {
            "join" => lane::JOINED_CLAIM,
            "leave" => {
                let outcome = text(&request.outcome).unwrap_or_else(|| "removed".into());
                if !lane::OUTCOMES.contains(&outcome.as_str()) {
                    return Err(St3Error::new(
                        "invalid-lane-outcome",
                        "an entry leaves a lane `completed` or `removed`",
                    ));
                }
                fields.insert("outcome".into(), Value::String(outcome));
                lane::LEFT_CLAIM
            }
            "move" => {
                let placement = text(&request.placement)
                    .as_deref()
                    .and_then(Placement::parse)
                    .ok_or_else(|| {
                        St3Error::new(
                            "invalid-lane-placement",
                            "a lane move uses top, bottom, before, or after",
                        )
                    })?;
                let named = text(&request.anchor).map(|value| lane_entry(&view, &value));
                match (placement.needs_anchor(), named) {
                    (true, None) => {
                        return Err(St3Error::new(
                            "missing-lane-anchor",
                            format!("a {} move names another entry", placement.as_str()),
                        ));
                    }
                    (false, Some(_)) => {
                        return Err(St3Error::new(
                            "unexpected-lane-anchor",
                            format!("a {} move does not name another entry", placement.as_str()),
                        ));
                    }
                    (true, Some(named)) => {
                        let named = named?;
                        if named == entry {
                            return Err(St3Error::new(
                                "invalid-lane-anchor",
                                "an entry cannot move relative to itself",
                            ));
                        }
                        fields.insert("anchor".into(), Value::String(named.clone()));
                        anchor = Some(named);
                    }
                    (false, None) => {}
                }
                fields.insert("placement".into(), Value::String(placement.as_str().into()));
                lane::MOVED_CLAIM
            }
            "mark" => {
                let state = text(&request.state).unwrap_or_default();
                if !lane::STATES.contains(&state.as_str()) {
                    return Err(St3Error::new(
                        "invalid-lane-state",
                        "a lane entry is marked waiting, held, ready, or running",
                    ));
                }
                fields.remove("reason");
                fields.insert("state".into(), Value::String(state));
                if let Some(detail) = text(&request.detail) {
                    fields.insert("detail".into(), Value::String(detail));
                }
                if let Some(head) = text(&request.head) {
                    fields.insert("head".into(), Value::String(head));
                }
                lane::MARKED_CLAIM
            }
            "approve" => {
                if view.approver.as_deref() != Some(actor) {
                    return Err(St3Error::new(
                        "lane-approval-denied",
                        match view.approver.as_deref() {
                            Some(approver) => {
                                format!("only {approver} approves entries in `{lane_subject}`")
                            }
                            None => format!("`{lane_subject}` declares no approver"),
                        },
                    ));
                }
                lane::APPROVED_CLAIM
            }
            other => {
                return Err(St3Error::new(
                    "invalid-lane-change",
                    format!(
                        "`{other}` is not a lane change; use join, leave, move, mark, or approve"
                    ),
                ));
            }
        };
        let input = ClaimInput {
            subject: lane_subject.clone(),
            kind: kind.into(),
            actor: Some(actor.to_owned()),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(request.idempotency_key.clone()),
        };
        // A retry names the earlier operation even after the lane moved on. Reappend to verify
        // the full request digest before returning its canonical claim.
        if self
            .operation_claim(&request.idempotency_key)
            .map_err(internal)?
            .is_some()
        {
            let claim = self.append_claim(&input)?;
            return self.lane_change_response(&lane_subject, Some(claim));
        }
        if !view.open {
            return Err(St3Error::new(
                "lane-closed",
                format!("`{lane_subject}` is closed: its run ended or a revision dropped it"),
            ));
        }
        let queued = |subject: &str| view.entries.iter().any(|queued| queued.entry == subject);
        if kind == lane::JOINED_CLAIM {
            if queued(&entry) {
                return self.lane_change_response(&lane_subject, None);
            }
        } else {
            for named in std::iter::once(&entry).chain(anchor.as_ref()) {
                if !queued(named) {
                    return Err(St3Error::new(
                        "entry-not-in-lane",
                        format!("`{named}` is not in `{lane_subject}`"),
                    )
                    .with_detail("lane", lane_subject.clone())
                    .with_detail("entry", named.clone()));
                }
            }
        }
        let claim = self.append_claim(&input)?;
        self.lane_change_response(&lane_subject, Some(claim))
    }

    fn lane_change_response(
        &self,
        subject: &str,
        claim: Option<ClaimRecord>,
    ) -> Result<LaneChangeResponse, St3Error> {
        let lane = self
            .lane(subject)
            .map_err(internal)?
            .ok_or_else(|| lane_not_found(subject))?;
        Ok(LaneChangeResponse { claim, lane })
    }
}

fn lane_not_found(argument: &str) -> St3Error {
    St3Error::new(
        "lane-not-found",
        format!("no open lane matches `{argument}`; `st lanes ls` lists them"),
    )
}

/// The full entry subject for an argument in this lane, checked against its prefix.
fn lane_entry(view: &LaneView, argument: &str) -> Result<String, St3Error> {
    let entry = lane::entry_subject(view.entries_prefix.as_deref(), argument);
    st3_schema::registry()
        .validate_subject(&entry)
        .map_err(|error| St3Error::new("invalid-lane-entry", error.message))?;
    if let Some(prefix) = view.entries_prefix.as_deref()
        && !entry.starts_with(prefix)
    {
        return Err(St3Error::new(
            "invalid-lane-entry",
            format!("entries in `{}` start with `{prefix}`", view.subject),
        ));
    }
    Ok(entry)
}

fn lane_declarations_tx(connection: &Connection, run: Option<&str>) -> Result<Vec<DesiredSubject>> {
    let mut statement = connection.prepare_cached(if run.is_some() {
        "SELECT subject, kind, body, member, owner_run, owner_generation, owner_step
         FROM desired WHERE owner_run=?1 AND kind='lane' ORDER BY subject"
    } else {
        "SELECT subject, kind, body, member, owner_run, owner_generation, owner_step
         FROM desired WHERE kind='lane' AND ?1 IS NULL ORDER BY subject"
    })?;
    let rows = statement.query_map([run], desired_from_row)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn lane_view_tx(connection: &Connection, declaration: &DesiredSubject) -> Result<LaneView> {
    let spec = crate::graph::lane_spec(&declaration.desired).unwrap_or(LaneSpec {
        stopped: true,
        ..LaneSpec::default()
    });
    let run = declaration.owner_run.clone();
    let (mission, run_live) = match run.as_deref() {
        Some(run) => connection
            .query_row(
                "SELECT mission_id, status FROM mission_runs WHERE id=?1",
                [run.strip_prefix("mission-run/").unwrap_or(run)],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .map(|(mission, status)| {
                (
                    Some(format!(
                        "mission/{}",
                        mission.strip_prefix("mission/").unwrap_or(&mission)
                    )),
                    !is_terminal_run_state(&status),
                )
            })
            .unwrap_or((None, false)),
        None => (None, true),
    };
    let name = run
        .as_deref()
        .and_then(|run| {
            declaration
                .subject
                .strip_prefix(&format!(
                    "lane/{}/",
                    run.strip_prefix("mission-run/").unwrap_or(run)
                ))
                .map(str::to_owned)
        })
        .unwrap_or_else(|| {
            declaration
                .subject
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_owned()
        });
    let events = lane_events_tx(connection, &declaration.subject)?;
    // The last of the newest claims in replica-stable order, as the replay applies them.
    let newest = events.iter().max_by_key(|event| event.at_unix_ms);
    let revision = newest.map_or_else(|| "empty".to_owned(), |event| event.claim_id.clone());
    let updated_at_unix_ms = newest.map(|event| event.at_unix_ms);
    let replayed = lane::replay(&events, LANE_RECENT_LIMIT);
    Ok(LaneView {
        subject: declaration.subject.clone(),
        name,
        run,
        mission,
        entries_prefix: spec.entries,
        approver: spec.approver,
        open: !spec.stopped && run_live,
        revision,
        updated_at_unix_ms,
        entries: replayed.entries,
        recent: replayed.recent,
    })
}

/// A lane's claims in the store's replica-stable order.
fn lane_events_tx(connection: &Connection, subject: &str) -> Result<Vec<Event>> {
    let mut statement = connection.prepare_cached(
        "SELECT claims.id, claims.kind, claims.actor, claims.body, claims.accepted_at_unix_ms
         FROM claims JOIN batches ON batches.id=claims.batch_id
         WHERE claims.subject=?1 AND claims.kind IN
               ('lane.joined', 'lane.left', 'lane.moved', 'lane.marked', 'lane.approved')
           AND NOT EXISTS (
               SELECT 1 FROM replica_records
               WHERE replica_records.claim_id=claims.id
                 AND replica_records.state='repaired'
           )
         ORDER BY length(claims.accepted_at_unix_ms), claims.accepted_at_unix_ms,
                  batches.origin, batches.replica_sequence, claims.store_index",
    )?;
    let rows = statement.query_map([subject], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    let mut events = Vec::new();
    for row in rows {
        let (claim_id, kind, actor, body, accepted_at) = row?;
        let body = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
        let fields = body.get("fields").unwrap_or(&body);
        let text = |name: &str| fields.get(name).and_then(Value::as_str).map(str::to_owned);
        let Some(entry) = text("entry") else {
            continue;
        };
        let change = match kind.as_str() {
            lane::JOINED_CLAIM => Change::Joined,
            lane::LEFT_CLAIM => Change::Left {
                outcome: text("outcome").unwrap_or_else(|| "removed".into()),
            },
            lane::MOVED_CLAIM => {
                let Some(placement) = text("placement").as_deref().and_then(Placement::parse)
                else {
                    continue;
                };
                Change::Moved {
                    placement,
                    anchor: text("anchor"),
                }
            }
            lane::MARKED_CLAIM => Change::Marked {
                state: text("state").unwrap_or_else(|| "waiting".into()),
                detail: text("detail"),
                head: text("head"),
            },
            lane::APPROVED_CLAIM => Change::Approved,
            _ => continue,
        };
        events.push(Event {
            claim_id,
            at_unix_ms: accepted_at.parse().unwrap_or_default(),
            actor: actor.unwrap_or_default(),
            entry,
            reason: text("reason"),
            change,
        });
    }
    Ok(events)
}
