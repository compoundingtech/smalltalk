//! Observed status is separate from desired state and agent-declared work status.
//! Owner-bound registers provide current freshness; historical claims remain readable for legacy seats.
use super::*;

pub(crate) const WINDOW_MS: u128 = 7 * 24 * 60 * 60 * 1000;
pub(crate) const MAX_TRANSITIONS: usize = 200;
pub(crate) const STALE_MS: u128 = 90_000;

pub(super) fn permission_blocked(
    state: Option<&str>,
    blocked_on: Option<&str>,
    ask: Option<&str>,
) -> bool {
    state.is_some_and(|state| !matches!(state, "ended" | "indeterminate"))
        && blocked_on == Some("human")
        && ask == Some("permission")
}

pub(super) fn observation_time(claim: &ClaimRecord) -> u128 {
    claim
        .body
        .pointer("/fields/observed_at_ms")
        .and_then(Value::as_u64)
        .map_or(claim.accepted_at_unix_ms, u128::from)
        .min(claim.accepted_at_unix_ms)
}

/// Match history_at's SQL `status_transition IS NOT 0` selection before reducing sources.
/// A suppressed heartbeat can still change reducer state; feeding it only to the drop planner
/// would hide a later legacy transition that the published history reader exposes.
pub(super) fn history_source(claim: &ClaimRecord) -> bool {
    if claim.kind != "harness.observed" {
        return true;
    }
    !claim.body.pointer("/fields/status_transition").is_some_and(|stamp| {
        matches!(stamp, Value::Bool(false)) || stamp.as_f64() == Some(0.0)
    })
}

/// Canonical source positions of transitions and incarnation resets. Heartbeats and changes
/// to display details do not become transitions. Shared by the read and checkpoint rules.
pub(super) fn transition_positions(claims: &[&ClaimRecord]) -> Vec<usize> {
    transitions(claims)
        .into_iter()
        .map(|transition| transition.0)
        .collect()
}

fn transitions(claims: &[&ClaimRecord]) -> Vec<(usize, Option<String>, bool)> {
    #[derive(Default)]
    struct Status<'a> {
        harness: Option<&'a str>,
        prompt: Option<&'a str>,
        update_prompt: bool,
        provider_auth: Option<bool>,
        blocked_on: Option<&'a str>,
        ask: Option<&'a str>,
        state: Option<&'a str>,
    }
    let mut runtime = None;
    let mut statuses = BTreeMap::<&str, Status<'_>>::new();
    let mut entries = Vec::new();
    for (position, claim) in claims.iter().enumerate() {
        let fields = claim.body.get("fields").unwrap_or(&claim.body);
        let Some(incarnation) = fields.get("incarnation_id").and_then(Value::as_str) else {
            continue;
        };
        if claim.kind == "runtime.observed" && fields["status"] == "running" {
            if runtime != Some(incarnation) {
                if runtime.is_some() || !statuses.contains_key(incarnation) {
                    entries.push((position, None, true));
                    statuses.insert(incarnation, Status::default());
                }
                runtime = Some(incarnation);
            }
            continue;
        }
        let reset = runtime.is_none() && !statuses.contains_key(incarnation);
        match claim.kind.as_str() {
            "harness.observed" => {
                let Some(state) = fields.get("state").and_then(Value::as_str) else {
                    continue;
                };
                let status = statuses.entry(incarnation).or_default();
                if matches!(state, "ended" | "indeterminate") {
                    status.blocked_on = None;
                    status.ask = None;
                } else {
                    if let Some(blocked_on) = fields.get("blocked_on") {
                        status.blocked_on = blocked_on.as_str();
                    }
                    if let Some(ask) = fields.get("ask") {
                        status.ask = ask.as_str();
                    }
                }
                status.harness = Some(if permission_blocked(Some(state), status.blocked_on, status.ask) {
                    "blocked"
                } else {
                    state
                });
                if let Some(auth) = fields.get("provider_auth").and_then(Value::as_bool) {
                    status.provider_auth = Some(auth);
                }
            }
            "harness.diagnostic" if statuses.contains_key(incarnation) => {
                let status = statuses.get_mut(incarnation).unwrap();
                match fields.get("code").and_then(Value::as_str) {
                    Some("provider-auth-expired") => status.prompt = Some("unauthenticated"),
                    Some("provider-trust-prompt") => status.prompt = Some("blocked"),
                    Some("provider-auth-restored") => status.prompt = None,
                    Some("provider-update-prompt") => status.update_prompt = true,
                    Some("provider-update-restored") => status.update_prompt = false,
                    _ => continue,
                }
            }
            _ => continue,
        }
        let status = statuses.get_mut(incarnation).unwrap();
        let next = if status.update_prompt {
            Some("blocked")
        } else if status.provider_auth == Some(false) {
            Some("unauthenticated")
        } else {
            status.prompt.or(status.harness)
        };
        // Heartbeats never become transitions when a retained prefix disappears. A recorded
        // transition remains one even if trimming removes an intervening state.
        let heartbeat = claim.kind == "harness.observed"
            && fields.get("status_transition").and_then(Value::as_bool) == Some(false);
        let recorded = claim.kind == "harness.observed"
            && status.prompt.is_none()
            && status.provider_auth != Some(false)
            && !status.update_prompt
            && fields.get("status_transition").and_then(Value::as_bool) == Some(true);
        if (reset || status.state != next || recorded) && !heartbeat {
            entries.push((position, next.map(str::to_owned), reset));
        }
        status.state = next;
    }
    entries
}

pub(super) fn state_run_since(
    connection: &Connection,
    subject: &str,
    incarnation: &str,
    state: &str,
    index: u64,
) -> Result<Option<u128>> {
    let mut statement = connection.prepare_cached(&harness_sql(
        connection,
        subject,
        &harness_observations_of_incarnation_query(),
    )?)?;
    let mut rows = statement.query(params![subject, index, incarnation])?;
    let mut since = None;
    while let Some(row) = rows.next()? {
        smallclaims::read_budget::check()?;
        let body: Value = serde_json::from_str(&row.get::<_, String>(1)?)?;
        let fields = body.get("fields").unwrap_or(&body);
        if fields.get("state").and_then(Value::as_str) != Some(state) {
            break;
        }
        let accepted: u128 = row.get::<_, String>(2)?.parse()?;
        since = Some(
            fields
                .get("observed_at_ms")
                .and_then(Value::as_u64)
                .map_or(accepted, u128::from)
                .min(accepted),
        );
    }
    Ok(since)
}

pub(super) fn enrich_harness(
    connection: &Connection,
    subject: &str,
    at_index: Option<u64>,
    view: &mut crate::model::CurrentHarnessView,
) -> Result<()> {
    let index = at_index.unwrap_or(i64::MAX as u64);
    let claim = claim_by_id_tx(connection, &view.claim)?;
    if let Some(claim) = claim.as_ref()
        && claim.kind == "harness.observed"
    {
        view.observed_at_unix_ms = observation_time(claim);
    }
    if claim
        .as_ref()
        .is_some_and(|claim| matches!(claim.kind.as_str(), "work.claimed" | "work.progress"))
    {
        return Ok(());
    }
    if let Some(claim) = claim.as_ref()
        && let Some(since) = claim
            .body
            .pointer("/fields/observed_since_ms")
            .and_then(Value::as_u64)
    {
        view.since_unix_ms = u128::from(since).min(claim.accepted_at_unix_ms);
    } else if claim
        .as_ref()
        .is_some_and(|claim| claim.kind == "harness.observed")
    {
        view.since_unix_ms = state_run_since(
            connection,
            subject,
            &view.incarnation_id,
            &view.state,
            index,
        )?
        .unwrap_or(view.since_unix_ms);
    }
    let has_prompt: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM claims WHERE subject=?1 AND kind='harness.diagnostic'
         AND store_index<=?2 AND json_extract(body,'$.fields.incarnation_id')=?3
         AND json_extract(body,'$.fields.code') IN ('provider-auth-expired','provider-trust-prompt','provider-auth-restored','provider-update-prompt','provider-update-restored'))",
        params![subject, index, view.incarnation_id], |row| row.get(0),
    )?;
    let unstamped_auth = claim.as_ref().is_some_and(|claim| {
        claim
            .body
            .pointer("/fields/provider_auth")
            .and_then(Value::as_bool)
            .is_some()
            && claim.body.pointer("/fields/observed_since_ms").is_none()
    });
    if has_prompt || view.state == "needs-login" || unstamped_auth {
        // Rare positive prompt and credential fences can interrupt an otherwise unchanged harness state.
        // Replay transition sources only for this incarnation, excluding ordinary heartbeats.
        let query = format!("SELECT {CLAIM_COLUMNS} FROM claims INDEXED BY claims_incarnation_accepted_index
            JOIN batches ON batches.id=claims.batch_id
            WHERE claims.subject=?1 AND +claims.store_index<=?2 AND {INCARNATION_OF_CLAIM}=?3
            AND claims.kind IN ('harness.observed','harness.diagnostic','runtime.observed')
            AND (claims.kind!='harness.observed' OR json_extract(claims.body,'$.fields.status_transition') IS NOT 0)
            ORDER BY {CANONICAL_ORDER}");
        let claims = connection
            .prepare_cached(&current_sql(&query))?
            .query_map(params![subject, index, view.incarnation_id], claim_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let refs = claims.iter().collect::<Vec<_>>();
        if let Some(entry) = transitions(&refs).last()
            && entry.1.as_deref()
                == Some(if view.state == "needs-login" {
                    "unauthenticated"
                } else {
                    view.state.as_str()
                })
        {
            view.since_unix_ms = observation_time(&claims[entry.0]);
        }
    }
    if claim
        .as_ref()
        .is_some_and(|claim| claim.id.starts_with(LOCAL_OBSERVATION_ID_PREFIX))
        && !matches!(
            view.state.as_str(),
            "blocked" | "needs-login" | "indeterminate"
        )
    {
        let restored: Option<String> = connection.query_row(&canonical_sql(
            "SELECT accepted_at_unix_ms FROM claims WHERE subject=?1 AND kind='harness.diagnostic'
             AND json_extract(body,'$.fields.incarnation_id')=?2
             AND json_extract(body,'$.fields.code') IN ('provider-auth-restored','provider-update-restored')
             ORDER BY CANONICAL_DESC(claims) LIMIT 1"), params![subject,view.incarnation_id], |row| row.get(0)).optional()?;
        if let Some(restored) = restored {
            view.since_unix_ms = view.since_unix_ms.max(restored.parse()?);
        }
    }
    Ok(())
}

impl Store {
    /// Client status describes harness evidence. Work progress can prove delivery readiness
    /// internally, but never substitutes for an observation in this contract.
    pub(crate) fn observed_harness_at(
        &self,
        subject: &str,
        index: u64,
    ) -> Result<Option<crate::model::CurrentHarnessView>> {
        let connection = self.readers.get();
        let mut harness = current_harness_fold_at(&connection, subject, Some(index), false, false)?;
        if let Some(view) = harness.as_mut() {
            enrich_harness(&connection, subject, Some(index), view)?;
        }
        Ok(harness)
    }

    /// Freshness is a read-time fact, never part of the replicated projection digest.
    pub fn seat_observation_at(
        &self,
        subject: &str,
        harness: Option<&crate::model::CurrentHarnessView>,
        index: u64,
        now: u128,
    ) -> Result<&'static str> {
        let Some(harness) = harness else {
            return Ok("missing");
        };
        let connection = self.readers.get();
        let claim = claim_by_id_tx(&connection, &harness.claim)?;
        let mut observed = claim
            .as_ref()
            .map_or(harness.observed_at_unix_ms, observation_time);
        let prompt_query = newest_claims_of_kind_query("claims.accepted_at_unix_ms", "harness.diagnostic")
            .replace(" ORDER BY", " AND json_extract(claims.body,'$.fields.incarnation_id')=?3 AND json_extract(claims.body,'$.fields.code') IN ('provider-auth-expired','provider-trust-prompt','provider-auth-restored','provider-update-prompt','provider-update-restored') ORDER BY") + " LIMIT 1";
        let prompt: Option<String> = connection
            .prepare_cached(&prompt_query)?
            .query_row(params![subject, index, harness.incarnation_id], |row| {
                row.get(0)
            })
            .optional()?;
        if let Some(prompt) = prompt {
            observed = observed.max(prompt.parse()?);
        }
        let local: Option<(String, u128)> = connection
            .prepare_cached(
                "SELECT body, observed_at_unix_ms FROM local_observations
             WHERE subject=?1 AND kind='harness.observed' AND after_store_index<=?2
               AND json_extract(body, '$.fields.incarnation_id')=?3
             ORDER BY id DESC LIMIT 1",
            )?
            .query_row(params![subject, index, harness.incarnation_id], |row| {
                Ok((row.get(0)?, row.get::<_, i64>(1)? as u128))
            })
            .optional()?;
        if let Some((body, accepted)) = local {
            let body: Value = serde_json::from_str(&body)?;
            let at = body
                .pointer("/fields/observed_at_ms")
                .and_then(Value::as_u64)
                .map_or(accepted, u128::from)
                .min(accepted);
            observed = observed.max(at);
        }
        Ok(if now.saturating_sub(observed) > STALE_MS {
            "stale"
        } else {
            "current"
        })
    }

    pub fn seat_status_history(&self, subject: &str, now: u128) -> Result<Value> {
        self.seat_status_history_at(subject, now, i64::MAX as u64)
    }

    pub(crate) fn seat_status_history_at(
        &self,
        subject: &str,
        now: u128,
        index: u64,
    ) -> Result<Value> {
        let connection = self.readers.get();
        history_at(&connection, subject, now, index)
    }
}

pub(super) fn history_at(
    connection: &Connection,
    subject: &str,
    now: u128,
    index: u64,
) -> Result<Value> {
    history_at_inner(connection, subject, now, index, None)
}

pub(super) fn history_at_with_sources(connection: &Connection, subject: &str, now: u128, index: u64) -> Result<(Value, Vec<smallclaims::store::checkpoint::CheckpointItemSource>)> {
    let mut sources = Vec::new();
    let history = history_at_inner(connection, subject, now, index, Some(&mut sources))?;
    Ok((history, sources))
}

fn history_at_inner(connection: &Connection, subject: &str, now: u128, index: u64, mut sources: Option<&mut Vec<smallclaims::store::checkpoint::CheckpointItemSource>>) -> Result<Value> {
    let cutoff = now.saturating_sub(WINDOW_MS);
    let current: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM latest_values WHERE subject=?1 AND kind='harness.observed')",
        [subject],
        |row| row.get(0),
    )?;
    if current {
        // Keep the existing envelope for installed clients. The empty, incomplete history
        // explicitly stops promising retained transitions; current status carries its own age.
        return Ok(json!({"kind":"status-history", "seat":subject, "items":[],
            "retained_from":crate::api::client_timestamp(now), "complete":false}));
    }
    // Seek each kind by acceptance time. Read a single older baseline per prompt channel, rather
    // than decoding a seat's lifetime observations on every history request.
    let mut keyed = Vec::new();
    let mut older = false;
    for (kind, codes) in [
        ("harness.observed", ""),
        ("runtime.observed", ""),
        (
            "harness.diagnostic",
            "'provider-auth-expired','provider-trust-prompt','provider-auth-restored'",
        ),
        (
            "harness.diagnostic",
            "'provider-update-prompt','provider-update-restored'",
        ),
    ] {
        let columns = format!(
            "{CLAIM_COLUMNS}, batches.origin, batches.replica_sequence, {}",
            canonical::position_sql("claims")
        );
        let filter = if codes.is_empty() {
            " AND (claims.kind!='harness.observed' OR json_extract(claims.body, '$.fields.status_transition') IS NOT 0)".into()
        } else {
            format!(" AND json_extract(claims.body, '$.fields.code') IN ({codes})")
        };
        let query = newest_claims_of_kind_query(&columns, kind)
            .replace(" ORDER BY", &format!("{filter} ORDER BY"));
        let mut statement = connection.prepare_cached(&current_sql(&query))?;
        let mut rows = statement.query(params![subject, index])?;
        while let Some(row) = rows.next()? {
            smallclaims::read_budget::check()?;
            let claim = claim_from_row(row)?;
            let before_window = claim.accepted_at_unix_ms < cutoff;
            let key = canonical::key_from_record(&claim, row.get(10)?, row.get(11)?, row.get(12)?);
            keyed.push((key, claim));
            if before_window {
                older = true;
                break;
            }
        }
    }
    // Seed older baselines with their incarnation's first retained runtime observation, so
    // a later running receipt does not hide a prompt that already held that same runtime.
    let baseline_incarnations = keyed
        .iter()
        .filter(|(_, claim)| claim.accepted_at_unix_ms < cutoff)
        .filter_map(|(_, claim)| {
            claim
                .body
                .pointer("/fields/incarnation_id")
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let query = format!("SELECT {CLAIM_COLUMNS}, batches.origin, batches.replica_sequence, {}
        FROM claims INDEXED BY claims_incarnation_accepted_index JOIN batches ON batches.id=claims.batch_id
        WHERE claims.subject=?1 AND {INCARNATION_OF_CLAIM}=?2 AND +claims.store_index<=?3 AND claims.kind='runtime.observed'
        AND json_extract(claims.body,'$.fields.status')='running' ORDER BY {CANONICAL_ORDER} LIMIT 1", canonical::position_sql("claims"));
    for incarnation in baseline_incarnations {
        let source = connection
            .prepare_cached(&query)?
            .query_row(params![subject, incarnation, index], |row| {
                let claim = claim_from_row(row)?;
                Ok((
                    canonical::key_from_record(&claim, row.get(10)?, row.get(11)?, row.get(12)?),
                    claim,
                ))
            })
            .optional()?;
        if let Some(source) = source {
            keyed.push(source);
        }
    }
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    keyed.dedup_by(|a, b| a.1.id == b.1.id);
    let claims = keyed
        .iter()
        .map(|(_, claim)| claim)
        .collect::<Vec<_>>();
    let entries = transitions(&claims);
    let tombstoned: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM checkpoint_claims WHERE subject=?1 AND kind IN ('harness.observed','runtime.observed'))",
            [subject], |row| row.get(0)
        )?;
    let eligible = entries
        .iter()
        .filter(|entry| observation_time(claims[entry.0]) >= cutoff)
        .collect::<Vec<_>>();
    let legacy = claims.iter().any(|claim| {
        claim.kind == "harness.observed"
            && claim
                .body
                .pointer("/fields/incarnation_id")
                .and_then(Value::as_str)
                .is_none()
    });
    let trimmed = legacy
        || older
        || tombstoned
        || eligible.len() < entries.len()
        || eligible.len() > MAX_TRANSITIONS;
    let start = eligible.len().saturating_sub(MAX_TRANSITIONS);
    let items = eligible[start..].iter().map(|entry| {
        let claim = claims[entry.0];
        if let Some(sources) = sources.as_mut() {
            sources.push(smallclaims::store::checkpoint::CheckpointItemSource {
                claim: claim.id.clone(), order: keyed[entry.0].0.clone(),
            });
        }
        let fields = claim.body.get("fields").unwrap_or(&claim.body);
        json!({
            "seat": subject, "runtime_incarnation": fields["incarnation_id"],
            "state": entry.1, "observed_at": crate::api::client_timestamp(observation_time(claim)), "reset": entry.2,
        })
    }).collect::<Vec<_>>();
    Ok(json!({
        "kind": "status-history", "seat": subject, "items": items,
        "retained_from": items.first().map(|item| item["observed_at"].clone()).unwrap_or_else(|| json!(crate::api::client_timestamp(cutoff))),
        "complete": !trimmed,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_source_selection_matches_sql_for_legacy_transition_stamps() {
        let store = Store::open_memory("cedar").unwrap();
        let mut claim = observe(&store, "one", "idle", now_ms(), "ready");
        let connection = Connection::open_in_memory().unwrap();
        for stamp in [None, Some(json!(false)), Some(json!(true)), Some(Value::Null),
            Some(json!(0)), Some(json!(0.0)), Some(serde_json::from_str::<Value>("-0").unwrap()),
            Some(serde_json::from_str::<Value>("0e0").unwrap()), Some(serde_json::from_str::<Value>("1e-400").unwrap()),
            Some(json!("0")), Some(json!("false"))]
        {
            claim.body = json!({"fields":{"state":"idle", "incarnation_id":"one"}});
            if let Some(stamp) = stamp { claim.body["fields"]["status_transition"] = stamp; }
            let selected: bool = connection.query_row(
                "SELECT json_extract(?1, '$.fields.status_transition') IS NOT 0",
                [claim.body.to_string()], |row| row.get(0),
            ).unwrap();
            assert_eq!(history_source(&claim), selected, "{}", claim.body);
        }
        claim.body = json!({"status_transition":false});
        assert!(history_source(&claim), "a legacy raw body has no $.fields stamp");
        claim.kind = "runtime.observed".into();
        claim.body = json!({"fields":{"status_transition":false}});
        assert!(history_source(&claim));
    }

    /// `current_harness`, after checking that the login-only fold, which the attention read
    /// uses, reaches the same answer to "does this seat need a login?" for the same claims.
    fn checked_harness(store: &Store, subject: &str) -> Result<Option<crate::model::CurrentHarnessView>> {
        let full = store.current_harness(subject)?;
        let fast = store.current_harness_for_login(subject)?;
        assert_eq!(
            fast.as_ref().is_some_and(|harness| harness.state == "needs-login"),
            full.as_ref().is_some_and(|harness| harness.state == "needs-login"),
            "the login-only fold disagrees for {subject}: {fast:?} against {full:?}"
        );
        if let (Some(fast), Some(full)) = (&fast, &full)
            && fast.state == "needs-login"
        {
            assert_eq!(fast.incarnation_id, full.incarnation_id);
            assert_eq!(fast.observed_at_unix_ms, full.observed_at_unix_ms);
        }
        Ok(full)
    }

    fn input(kind: &str, fields: Value) -> ClaimInput {
        ClaimInput {
            subject: "agent/cedar".into(),
            kind: kind.into(),
            actor: Some("agent/cedar".into()),
            fields: serde_json::from_value(fields).unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        }
    }
    fn runtime(store: &Store, incarnation: &str) {
        store
            .append_claim(&input(
                "runtime.observed",
                json!({
                    "status":"running", "incarnation_id":incarnation, "runtime_id":"native"
                }),
            ))
            .unwrap();
    }
    fn observe(
        store: &Store,
        incarnation: &str,
        state: &str,
        at: u128,
        reason: &str,
    ) -> ClaimRecord {
        store.append_latest_observation(&input("harness.observed", json!({
            "state":state, "incarnation_id":incarnation, "observed_at_ms":at as u64, "reason":reason
        })), at).unwrap().0
    }

    fn observe_legacy(
        store: &Store,
        incarnation: &str,
        state: &str,
        at: u128,
        reason: &str,
    ) -> ClaimRecord {
        append_legacy_graph_observation_fenced(&store.graph, &input("harness.observed", json!({
            "state":state,"incarnation_id":incarnation,"observed_at_ms":at as u64,"reason":reason
        })), at, None).unwrap().0
    }

    #[test]
    fn approval_observation_is_blocked_in_current_then_clears() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        let at = now_ms();
        observe(&store, "one", "working", at, "working");
        store
            .append_latest_observation(
                &input(
                    "harness.observed",
                    json!({
                        "state":"working", "incarnation_id":"one", "observed_at_ms":(at + 1) as u64,
                        "blocked_on":"human", "ask":"permission", "reason":"waitingOnApproval"
                    }),
                ),
                at + 1,
            )
            .unwrap();
        let blocked = checked_harness(&store, "agent/cedar").unwrap().unwrap();
        assert_eq!(blocked.state, "blocked");
        assert_eq!(blocked.blocked_on.as_deref(), Some("human"));
        assert_eq!(blocked.ask.as_deref(), Some("permission"));
        assert_eq!(blocked.reason.as_deref(), Some("waitingOnApproval"));

        let sparse = store
            .append_latest_observation(
                &input(
                    "harness.observed",
                    json!({
                        "state":"working", "incarnation_id":"one", "observed_at_ms":(at + 2) as u64,
                        "reason":"stillWaiting"
                    }),
                ),
                at + 2,
            )
            .unwrap()
            .0;
        assert_eq!(sparse.body["fields"]["status_transition"], false);
        assert_eq!(checked_harness(&store, "agent/cedar").unwrap().unwrap().state, "blocked");

        store
            .append_latest_observation(
                &input(
                    "harness.observed",
                    json!({
                        "state":"working", "incarnation_id":"one", "observed_at_ms":(at + 3) as u64,
                        "blocked_on":null, "ask":null, "reason":null
                    }),
                ),
                at + 3,
            )
            .unwrap();
        let resumed = checked_harness(&store, "agent/cedar").unwrap().unwrap();
        assert_eq!(resumed.state, "working");
        assert!(resumed.blocked_on.is_none());

        store
            .append_latest_observation(
                &input(
                    "harness.observed",
                    json!({
                        "state":"working", "incarnation_id":"one", "observed_at_ms":(at + 4) as u64,
                        "blocked_on":"human", "ask":"question", "reason":"waitingOnUserInput"
                    }),
                ),
                at + 4,
            )
            .unwrap();
        assert_eq!(
            checked_harness(&store, "agent/cedar").unwrap().unwrap().state,
            "working"
        );
        let history = store.seat_status_history("agent/cedar", at + 5).unwrap();
        let states = history["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["state"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(states, ["working", "blocked", "working"]);

        runtime(&store, "two");
        observe(&store, "two", "working", at + 5, "working");
        let current = checked_harness(&store, "agent/cedar").unwrap().unwrap();
        assert_eq!(current.incarnation_id, "two");
        assert_eq!(current.state, "working");
        let history = store.seat_status_history("agent/cedar", at + 6).unwrap();
        let last = history["items"].as_array().unwrap().last().unwrap();
        assert_eq!(last["state"], "working");
        assert_eq!(last["runtime_incarnation"], "two");
    }

    #[test]
    fn terminal_observation_fences_permission_before_sparse_work_resumes() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        let at = now_ms() - 1_000;
        let publish = |state: &str, fields: Value, time: u128| {
            let mut fields = fields.as_object().unwrap().clone();
            fields.insert("state".into(), json!(state));
            fields.insert("incarnation_id".into(), json!("one"));
            fields.insert("observed_at_ms".into(), json!(time as u64));
            store.append_latest_observation(&input("harness.observed", json!(fields)), time)
                .unwrap().0
        };
        publish("working", json!({"blocked_on":"human", "ask":"permission"}), at);
        assert_eq!(checked_harness(&store, "agent/cedar").unwrap().unwrap().state, "blocked");
        let ended = publish("ended", json!({"blocked_on":"human", "ask":"permission"}), at + 1);
        assert!(ended.body["fields"]["blocked_on"].is_null());
        assert!(ended.body["fields"]["ask"].is_null());
        let resumed = publish("working", json!({"reason":"resumed"}), at + 2);
        assert!(resumed.body["fields"]["blocked_on"].is_null());
        assert!(resumed.body["fields"]["ask"].is_null());
        assert_eq!(resumed.body["fields"]["status_transition"], true);
        assert_eq!(resumed.body["fields"]["observed_since_ms"], json!((at + 2) as u64));
        let current = checked_harness(&store, "agent/cedar").unwrap().unwrap();
        assert_eq!(current.state, "working");
        assert_eq!(current.since_unix_ms, at + 2);
        assert!(current.blocked_on.is_none());
        assert!(current.ask.is_none());
        let history = store.seat_status_history("agent/cedar", at + 3).unwrap();
        let states = history["items"].as_array().unwrap().iter()
            .filter_map(|item| item["state"].as_str()).collect::<Vec<_>>();
        assert_eq!(states, ["blocked", "ended", "working"]);
    }

    #[test]
    fn unknown_provider_auth_does_not_clear_a_known_rejection() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        let at = now_ms();
        let rejected = store
            .append_latest_observation(
                &input("harness.observed", json!({
                    "state":"working", "incarnation_id":"one", "observed_at_ms":at as u64,
                    "provider_auth":false, "reason":"providerAuth"
                })),
                at,
            )
            .unwrap()
            .0;
        let uncertain = store
            .append_latest_observation(
                &input("harness.observed", json!({
                    "state":"working", "incarnation_id":"one", "observed_at_ms":(at + 1) as u64,
                    "provider_auth":null, "reason":"authUnknown"
                })),
                at + 1,
            )
            .unwrap()
            .0;
        assert_eq!(uncertain.body["fields"]["provider_auth"], false);
        assert_eq!(uncertain.body["fields"]["status_transition"], false);
        assert_eq!(
            uncertain.body["fields"]["observed_since_ms"],
            rejected.body["fields"]["observed_since_ms"]
        );
        assert_eq!(checked_harness(&store, "agent/cedar").unwrap().unwrap().state, "needs-login");
        let history = store.seat_status_history("agent/cedar", at + 2).unwrap();
        let states = history["items"].as_array().unwrap().iter()
            .filter_map(|item| item["state"].as_str()).collect::<Vec<_>>();
        assert_eq!(states, ["unauthenticated"]);
    }

    #[test]
    fn repeated_observations_and_detail_changes_preserve_since_and_refresh_freshness() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        let at = now_ms() - 1_000;
        let first = observe(&store, "one", "idle", at, "ready");
        let second = observe(&store, "one", "idle", at + 1, "ready");
        assert!(local_observation_position(&second).is_some());
        observe(&store, "one", "idle", at + 2, "waiting");
        let harness = checked_harness(&store, "agent/cedar").unwrap().unwrap();
        assert_eq!(
            harness.since_unix_ms,
            first.body["fields"]["observed_since_ms"].as_u64().unwrap() as u128
        );
        let index = store.index().unwrap();
        assert_eq!(
            store
                .seat_observation_at("agent/cedar", Some(&harness), index, at + 90_002)
                .unwrap(),
            "current"
        );
        assert_eq!(
            store
                .seat_observation_at("agent/cedar", Some(&harness), index, at + 90_003)
                .unwrap(),
            "stale"
        );
        assert_eq!(
            store
                .seat_observation_at("agent/cedar", None, index, at)
                .unwrap(),
            "missing"
        );
        let history = store.seat_status_history("agent/cedar", at + 3).unwrap();
        assert_eq!(history["items"], json!([]));
        assert_eq!(history["complete"], false);
        observe(&store, "one", "working", at + 4, "turn");
        assert!(
            store
                .current_harness("agent/cedar")
                .unwrap()
                .unwrap()
                .since_unix_ms
                == at + 4
        );
    }

    #[test]
    fn status_upgrade_preserves_since_from_legacy_observations() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        let at = now_ms() - 1_000;
        let first = observe_legacy(&store, "one", "idle", at, "ready");
        store.connection.lock().unwrap().execute(
            "UPDATE claims SET body=json_remove(body, '$.fields.observed_since_ms', '$.fields.status_transition') WHERE id=?1",
            [first.id],
        ).unwrap();
        let before = checked_harness(&store, "agent/cedar").unwrap().unwrap();
        observe_legacy(&store, "one", "idle", at + 1, "waiting");
        let after = checked_harness(&store, "agent/cedar").unwrap().unwrap();
        assert_eq!(before.since_unix_ms, at);
        assert_eq!(after.since_unix_ms, before.since_unix_ms);
    }

    #[test]
    fn history_reports_runtime_reset_even_before_a_new_observation() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        let at = now_ms();
        observe_legacy(&store, "one", "idle", at, "ready");
        runtime(&store, "two");
        assert!(checked_harness(&store, "agent/cedar").unwrap().is_none());
        let history = store.seat_status_history("agent/cedar", now_ms()).unwrap();
        let items = history["items"].as_array().unwrap();
        let reset = items.last().unwrap();
        assert_eq!(reset["runtime_incarnation"], "two");
        assert_eq!(reset["reset"], true);
        assert_eq!(reset["state"], Value::Null);
        assert_eq!(history["complete"], true);
    }

    #[test]
    fn history_is_bounded_by_count_and_age_and_reports_its_retained_boundary() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        let at = now_ms();
        for index in 0..205 {
            observe_legacy(
                &store,
                "one",
                if index % 2 == 0 { "working" } else { "idle" },
                at,
                &format!("turn-{index}"),
            );
        }
        let history = store.seat_status_history("agent/cedar", at).unwrap();
        assert_eq!(history["items"].as_array().unwrap().len(), MAX_TRANSITIONS);
        assert_eq!(history["complete"], false);
        assert_eq!(history["retained_from"], history["items"][0]["observed_at"]);
        let aged = store
            .seat_status_history("agent/cedar", now_ms() + WINDOW_MS + 1)
            .unwrap();
        assert!(aged["items"].as_array().unwrap().is_empty());
        assert_eq!(aged["complete"], false);
    }

    #[test]
    fn unchanged_observations_publish_a_bounded_heartbeat_without_a_transition() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        let at = now_ms();
        let first = observe_legacy(&store, "one", "idle", at, "ready");
        // Move the previous publication's acceptance time to simulate the publish interval.
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE claims SET accepted_at_unix_ms=?1 WHERE id=?2",
                params![(at - 60_000).to_string(), first.id],
            )
            .unwrap();
        let heartbeat = observe_legacy(&store, "one", "idle", at + 1, "ready");
        assert!(local_observation_position(&heartbeat).is_none());
        assert_eq!(heartbeat.body["fields"]["status_transition"], false);
        assert_eq!(
            heartbeat.body["fields"]["observed_since_ms"],
            first.body["fields"]["observed_since_ms"]
        );
    }
    #[test]
    fn status_historical_observations_do_not_restart_the_current_runtime_since() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        let at = now_ms() - 1_000;
        observe_legacy(&store, "one", "idle", at, "ready");
        runtime(&store, "two");
        observe_legacy(&store, "two", "working", at + 1, "turn");
        observe_legacy(&store, "one", "working", at + 2, "historical turn");
        observe_legacy(&store, "two", "working", at + 3, "turn");
        let current = checked_harness(&store, "agent/cedar").unwrap().unwrap();
        assert_eq!(current.since_unix_ms, at + 1);
        let history = store.seat_status_history("agent/cedar", now_ms()).unwrap();
        let resets = history["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["reset"] == true)
            .count();
        assert_eq!(resets, 2, "only runtime replacements reset history");
    }

    #[test]
    fn status_history_restores_the_auth_fence_that_predates_the_window() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        observe_legacy(&store, "one", "idle", now_ms(), "ready");
        let diagnostic = |code| {
            store.append_legacy_claim(&input("harness.diagnostic", json!({
            "incarnation_id":"one", "code":code, "driver":"codex", "reason":"fixture", "severity":"warning"
        }))).unwrap()
        };
        diagnostic("provider-auth-expired");
        diagnostic("provider-update-prompt");
        let restored = diagnostic("provider-update-restored");
        let now = restored.accepted_at_unix_ms + WINDOW_MS + 1_000;
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE claims SET accepted_at_unix_ms=?1 WHERE id=?2",
                params![(now - 1).to_string(), restored.id],
            )
            .unwrap();
        let history = store.seat_status_history("agent/cedar", now).unwrap();
        assert_eq!(history["items"].as_array().unwrap().len(), 1);
        assert_eq!(history["items"][0]["state"], "unauthenticated");
        assert_eq!(history["complete"], false);
    }

    #[test]
    fn status_update_prompt_has_priority_and_preserves_since_until_restored() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        observe(&store, "one", "idle", now_ms(), "ready");
        let diagnostic = |code| {
            store.append_claim(&input("harness.diagnostic", json!({
            "incarnation_id":"one", "code":code, "driver":"codex", "reason":"fixture", "severity":"warning"
        }))).unwrap()
        };
        diagnostic("provider-auth-expired");
        let prompt = diagnostic("provider-update-prompt");
        diagnostic("provider-update-prompt");
        diagnostic("provider-auth-restored");
        observe(&store, "one", "idle", now_ms(), "parallel channel");
        let blocked = checked_harness(&store, "agent/cedar").unwrap().unwrap();
        assert_eq!(blocked.state, "blocked");
        assert_eq!(blocked.since_unix_ms, prompt.accepted_at_unix_ms);
        let restored = diagnostic("provider-update-restored");
        let idle = checked_harness(&store, "agent/cedar").unwrap().unwrap();
        assert_eq!(idle.state, "idle");
        assert_eq!(idle.since_unix_ms, restored.accepted_at_unix_ms);
        let history = store.seat_status_history("agent/cedar", now_ms()).unwrap();
        assert_eq!(history["items"], json!([]));
        assert_eq!(history["complete"], false);
    }

    #[test]
    fn a_null_auth_successor_through_publication_keeps_the_login_episode() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        let at = now_ms() - 1_000;
        let publish = |auth: Value, sequence: u64, time: u128| {
            store
                .append_latest_observation(
                    &input(
                        "harness.observed",
                        json!({
                            "incarnation_id":"one", "driver":"claude", "state":"idle",
                            "provider_auth":auth, "provider_auth_sequence":sequence,
                            "ownership_sequence":1, "observed_at_ms":time as u64
                        }),
                    ),
                    time,
                )
                .unwrap()
                .0
        };
        publish(json!(false), 1, at);
        let successor = publish(Value::Null, 1, at + 1);
        assert!(local_observation_position(&successor).is_some());
        let blocked = checked_harness(&store, "agent/cedar").unwrap().unwrap();
        assert_eq!(blocked.state, "needs-login");
        assert_eq!(blocked.since_unix_ms, at);
        let history = store.seat_status_history("agent/cedar", now_ms()).unwrap();
        assert_eq!(history["items"], json!([]));
        publish(json!(true), 2, at + 2);
        let recovered = checked_harness(&store, "agent/cedar").unwrap().unwrap();
        assert_eq!(recovered.state, "idle");
        assert_eq!(recovered.since_unix_ms, at + 2);
    }

    #[test]
    fn native_login_since_survives_activity_until_successful_recovery() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        let observed_at = std::cell::Cell::new(now_ms() as u64 - 1_000);
        let native = |state: &str, auth: bool| {
            observed_at.set(observed_at.get() + 1);
            store
                .append_claim(&input(
                    "harness.observed",
                    json!({
                        "incarnation_id":"one", "state":state, "provider_auth":auth,
                        "driver":"claude", "provider_auth_sequence": if auth { 2 } else { 1 },
                        "observed_at_ms": observed_at.get()
                    }),
                ))
                .unwrap()
        };
        let first = native("idle", false);
        native("working", false);
        native("idle", false);
        let blocked = checked_harness(&store, "agent/cedar").unwrap().unwrap();
        assert_eq!(blocked.state, "needs-login");
        assert_eq!(
            blocked.since_unix_ms,
            u128::from(first.body["fields"]["observed_at_ms"].as_u64().unwrap())
        );
        let restored = native("idle", true);
        let idle = checked_harness(&store, "agent/cedar").unwrap().unwrap();
        assert_eq!(idle.state, "idle");
        assert_eq!(
            idle.since_unix_ms,
            u128::from(restored.body["fields"]["observed_at_ms"].as_u64().unwrap())
        );
        let history = store.seat_status_history("agent/cedar", now_ms()).unwrap();
        assert_eq!(history["items"], json!([]));
    }

    #[test]
    fn status_prompt_transitions_and_repeats_agree_with_current_since() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        let at = now_ms();
        observe_legacy(&store, "one", "idle", at, "ready");
        let diagnostic = |code| {
            store.append_legacy_claim(&input("harness.diagnostic", json!({
            "incarnation_id":"one", "code":code, "reason":"fixture", "severity":"warning"
        }))).unwrap()
        };
        let prompt = diagnostic("provider-auth-expired");
        diagnostic("provider-auth-expired");
        let blocked = checked_harness(&store, "agent/cedar").unwrap().unwrap();
        assert_eq!(blocked.state, "needs-login");
        assert_eq!(blocked.since_unix_ms, prompt.accepted_at_unix_ms);
        let restored = diagnostic("provider-auth-restored");
        let idle = checked_harness(&store, "agent/cedar").unwrap().unwrap();
        assert_eq!(idle.state, "idle");
        assert_eq!(idle.since_unix_ms, restored.accepted_at_unix_ms);
        let history = store.seat_status_history("agent/cedar", now_ms()).unwrap();
        let states = history["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry["state"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(states, ["idle", "unauthenticated", "idle"]);
    }
}
