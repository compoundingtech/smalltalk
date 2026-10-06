//! Observed status is separate from desired state and agent-declared work status.
//! Claims provide the replicated history; local receipts only improve current freshness.
use super::*;

pub(crate) const WINDOW_MS: u128 = 7 * 24 * 60 * 60 * 1000;
pub(crate) const MAX_TRANSITIONS: usize = 200;
pub(crate) const STALE_MS: u128 = 90_000;

pub(super) fn observation_time(claim: &ClaimRecord) -> u128 {
    claim
        .body
        .pointer("/fields/observed_at_ms")
        .and_then(Value::as_u64)
        .map_or(claim.accepted_at_unix_ms, u128::from)
        .min(claim.accepted_at_unix_ms)
}

fn source_candidate(
    connection: &Connection,
    subject: &str,
    incarnation: &str,
    index: u64,
    kind: &str,
    auth_only: bool,
) -> Result<Option<ClaimRecord>> {
    let (source_index, auth) = if auth_only {
        ("claims_harness_auth_source_index", "AND json_type(body, '$.fields.provider_auth') IN ('true','false')")
    } else {
        ("claims_harness_source_index", "")
    };
    let query = format!(
        "SELECT {CLAIM_COLUMNS} FROM claims INDEXED BY {source_index}
         WHERE subject=?1 AND {INCARNATION_OF_CLAIM}=?2 AND kind=?3
           AND kind IN ('harness.current','harness.observed') AND +store_index<=?4 {auth}
         ORDER BY {HARNESS_SOURCE_TIME} DESC, {CANONICAL_ORDER_DESC} LIMIT 1"
    );
    Ok(connection.prepare_cached(&query)?
        .query_row(params![subject, incarnation, kind, index], claim_from_row)
        .optional()?)
}

/// Once the independent current lane exists for this runtime, receipt time cannot replace a
/// newer capture. Old writers retain the legacy observed fold when this lane is absent.
pub(super) fn current_source_at(
    connection: &Connection,
    subject: &str,
    incarnation: &str,
    index: u64,
) -> Result<Option<ClaimRecord>> {
    let Some(current) = source_candidate(connection, subject, incarnation, index, "harness.current", false)? else {
        return Ok(None);
    };
    let observed = source_candidate(connection, subject, incarnation, index, "harness.observed", false)?;
    Ok(match observed {
        Some(observed) if observation_time(&observed) > observation_time(&current) => Some(observed),
        _ => Some(current),
    })
}

pub(super) fn current_auth_at(
    connection: &Connection,
    subject: &str,
    incarnation: &str,
    index: u64,
) -> Result<Option<ClaimRecord>> {
    let current = source_candidate(connection, subject, incarnation, index, "harness.current", true)?;
    let observed = source_candidate(connection, subject, incarnation, index, "harness.observed", true)?;
    Ok(match (current, observed) {
        (Some(current), Some(observed)) if observation_time(&observed) > observation_time(&current) => Some(observed),
        (Some(current), _) => Some(current),
        (None, observed) => observed,
    })
}

fn history_gap(claim: &ClaimRecord) -> Option<crate::model::HarnessHistoryGap> {
    let fields = claim.body.get("fields")?;
    Some(crate::model::HarnessHistoryGap {
        runtime_incarnation: fields.get("incarnation_id")?.as_str()?.to_owned(),
        count: fields.get("history_gap_count")?.as_u64()?,
        from_ms: fields.get("history_gap_from_ms")?.as_u64()?,
        to_ms: fields.get("history_gap_to_ms")?.as_u64()?,
        reason: fields.get("history_gap_reason")?.as_str()?.to_owned(),
    })
}

fn history_gaps_at(
    connection: &Connection,
    subject: &str,
    index: u64,
    from: u128,
    to: u128,
) -> Result<Vec<crate::model::HarnessHistoryGap>> {
    // Aggregate per source runtime, not the runtime currently selected for the Agent card.
    // Older retained gaps must remain visible after a reset or a producer's nullable snapshot.
    let query = format!(
        "SELECT {INCARNATION_OF_CLAIM},
            MAX(json_extract(body,'$.fields.history_gap_count')),
            MIN(json_extract(body,'$.fields.history_gap_from_ms')),
            MAX(json_extract(body,'$.fields.history_gap_to_ms'))
         FROM claims INDEXED BY claims_harness_gap_index
         WHERE subject=?1 AND +store_index<=?2 AND kind='harness.current'
           AND json_type(body,'$.fields.history_gap_count')='integer'
           AND json_extract(body,'$.fields.history_gap_count')>0
         GROUP BY {INCARNATION_OF_CLAIM}
         ORDER BY MIN(json_extract(body,'$.fields.history_gap_from_ms')), {INCARNATION_OF_CLAIM}"
    );
    let mut statement = connection.prepare_cached(&query)?;
    let current = statement.query_map(params![subject, index], |row| {
        Ok(crate::model::HarnessHistoryGap {
            runtime_incarnation: row.get(0)?, count: row.get(1)?,
            from_ms: row.get(2)?, to_ms: row.get(3)?, reason: "cap-full".into(),
        })
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut gaps = current.into_iter().map(|gap| (gap.runtime_incarnation.clone(), gap))
        .collect::<BTreeMap<_, _>>();
    // Each durable row is a unique prepared segment; retries reuse its claim ID.
    // Current snapshots are cumulative, so adding them to segment totals would double count.
    let mut durable = BTreeMap::<String, crate::model::HarnessHistoryGap>::new();
    let mut statement = connection.prepare_cached(
        "SELECT body FROM claims INDEXED BY claims_harness_durable_gap_index
         WHERE subject=?1 AND +store_index<=?2 AND kind='harness.history.gap'"
    )?;
    let mut rows = statement.query(params![subject, index])?;
    while let Some(row) = rows.next()? {
        let body: Value = serde_json::from_str(&row.get::<_, String>(0)?)?;
        let fields = &body["fields"];
        let incarnation = fields["runtime_incarnation"].as_str().context("gap runtime is missing")?;
        let count = fields["history_gap_count"].as_u64().context("gap count is missing")?;
        let from_ms = fields["history_gap_from_ms"].as_u64().context("gap interval start is missing")?;
        let to_ms = fields["history_gap_to_ms"].as_u64().context("gap interval end is missing")?;
        if let Some(gap) = durable.get_mut(incarnation) {
            gap.count = gap.count.checked_add(count).context("history gap count exceeds u64")?;
            gap.from_ms = gap.from_ms.min(from_ms);
            gap.to_ms = gap.to_ms.max(to_ms);
        } else {
            durable.insert(incarnation.to_owned(), crate::model::HarnessHistoryGap {
                runtime_incarnation: incarnation.to_owned(), count, from_ms, to_ms, reason: "cap-full".into(),
            });
        }
    }
    for (incarnation, durable) in durable {
        if let Some(gap) = gaps.get_mut(&incarnation) {
            gap.count = gap.count.max(durable.count);
            gap.from_ms = gap.from_ms.min(durable.from_ms);
            gap.to_ms = gap.to_ms.max(durable.to_ms);
        } else {
            gaps.insert(incarnation, durable);
        }
    }
    let mut gaps = gaps.into_values()
        .filter(|gap| u128::from(gap.to_ms) >= from && u128::from(gap.from_ms) <= to)
        .collect::<Vec<_>>();
    gaps.sort_by(|left, right| (left.from_ms, &left.runtime_incarnation).cmp(&(right.from_ms, &right.runtime_incarnation)));
    Ok(gaps)
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
                status.harness = Some(state);
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
    let mut statement = connection.prepare_cached(&harness_observations_of_incarnation_query())?;
    let mut rows = statement.query(params![subject, index, incarnation])?;
    let mut since = None;
    while let Some(row) = rows.next()? {
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
    if claim
        .as_ref()
        .is_some_and(|claim| matches!(claim.kind.as_str(), "work.claimed" | "work.progress"))
    {
        return Ok(());
    }
    if view.state == "needs-login" && claim.as_ref().is_some_and(|claim| {
        matches!(claim.kind.as_str(), "harness.current" | "harness.observed")
            && claim.body.pointer("/fields/provider_auth").and_then(Value::as_bool) != Some(false)
    }) {
        // A fresh unknown-auth capture refreshes activity/counts but cannot restart or lift
        // the source-newest explicit refusal's episode.
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
    if let Some(claim) = claim.as_ref()
        && claim.kind == "harness.current"
    {
        // Current captures carry their own source time, including for native auth fences.
        // Ordered history is independent and cannot redefine the current state's since.
        return Ok(());
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
            .prepare_cached(&query)?
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
    Ok(())
}

impl Store {
    pub(crate) fn harness_history_gap_at(
        &self,
        subject: &str,
        incarnation: &str,
        index: u64,
    ) -> Result<Option<crate::model::HarnessHistoryGap>> {
        let connection = self.readers.get();
        Ok(source_candidate(&connection, subject, incarnation, index, "harness.current", false)?
            .as_ref().and_then(history_gap))
    }

    /// Client status describes harness evidence. Work progress can prove delivery readiness
    /// internally, but never substitutes for an observation in this contract.
    pub(crate) fn observed_harness_at(
        &self,
        subject: &str,
        index: u64,
    ) -> Result<Option<crate::model::CurrentHarnessView>> {
        let connection = self.readers.get();
        let mut harness = current_harness_fold_at(&connection, subject, Some(index), false)?;
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
    let cutoff = now.saturating_sub(WINDOW_MS);
    let history_gaps = history_gaps_at(connection, subject, index, cutoff, now)?;
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
        let mut statement = connection.prepare_cached(&query)?;
        let mut rows = statement.query(params![subject, index])?;
        while let Some(row) = rows.next()? {
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
        .into_iter()
        .map(|(_, claim)| claim)
        .collect::<Vec<_>>();
    let refs = claims.iter().collect::<Vec<_>>();
    let entries = transitions(&refs);
    let tombstoned: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM checkpoint_claims WHERE subject=?1 AND kind IN ('harness.observed','runtime.observed'))",
            [subject], |row| row.get(0)
        )?;
    let eligible = entries
        .iter()
        .filter(|entry| observation_time(&claims[entry.0]) >= cutoff)
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
        || !history_gaps.is_empty()
        || eligible.len() < entries.len()
        || eligible.len() > MAX_TRANSITIONS;
    let start = eligible.len().saturating_sub(MAX_TRANSITIONS);
    let items = eligible[start..].iter().map(|entry| {
        let claim = &claims[entry.0];
        let fields = claim.body.get("fields").unwrap_or(&claim.body);
        json!({
            "seat": subject, "runtime_incarnation": fields["incarnation_id"],
            "state": entry.1, "observed_at": crate::api::client_timestamp(observation_time(claim)), "reset": entry.2,
        })
    }).collect::<Vec<_>>();
    Ok(json!({
        "kind": "status-history", "seat": subject, "items": items,
        "retained_from": items.first().map(|item| item["observed_at"].clone()).unwrap_or_else(|| json!(crate::api::client_timestamp(cutoff))),
        "complete": !trimmed, "history_gaps": history_gaps,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn repeated_observations_and_detail_changes_preserve_since_and_refresh_freshness() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        let at = now_ms() - 1_000;
        let first = observe(&store, "one", "idle", at, "ready");
        let second = observe(&store, "one", "idle", at + 1, "ready");
        assert!(local_observation_position(&second).is_some());
        observe(&store, "one", "idle", at + 2, "waiting");
        let harness = store.current_harness("agent/cedar").unwrap().unwrap();
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
        assert_eq!(
            history["items"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["state"] == "idle")
                .count(),
            1
        );
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
        let first = observe(&store, "one", "idle", at, "ready");
        store.connection.lock().unwrap().execute(
            "UPDATE claims SET body=json_remove(body, '$.fields.observed_since_ms', '$.fields.status_transition') WHERE id=?1",
            [first.id],
        ).unwrap();
        let before = store.current_harness("agent/cedar").unwrap().unwrap();
        observe(&store, "one", "idle", at + 1, "waiting");
        let after = store.current_harness("agent/cedar").unwrap().unwrap();
        assert_eq!(before.since_unix_ms, at);
        assert_eq!(after.since_unix_ms, before.since_unix_ms);
    }

    #[test]
    fn history_reports_runtime_reset_even_before_a_new_observation() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        let at = now_ms();
        observe(&store, "one", "idle", at, "ready");
        runtime(&store, "two");
        assert!(store.current_harness("agent/cedar").unwrap().is_none());
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
            observe(
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
        let first = observe(&store, "one", "idle", at, "ready");
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
        let heartbeat = observe(&store, "one", "idle", at + 1, "ready");
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
        observe(&store, "one", "idle", at, "ready");
        runtime(&store, "two");
        observe(&store, "two", "working", at + 1, "turn");
        observe(&store, "one", "working", at + 2, "historical turn");
        observe(&store, "two", "working", at + 3, "turn");
        let current = store.current_harness("agent/cedar").unwrap().unwrap();
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
        observe(&store, "one", "idle", now_ms(), "ready");
        let diagnostic = |code| {
            store.append_claim(&input("harness.diagnostic", json!({
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
        let blocked = store.current_harness("agent/cedar").unwrap().unwrap();
        assert_eq!(blocked.state, "blocked");
        assert_eq!(blocked.since_unix_ms, prompt.accepted_at_unix_ms);
        let restored = diagnostic("provider-update-restored");
        let idle = store.current_harness("agent/cedar").unwrap().unwrap();
        assert_eq!(idle.state, "idle");
        assert_eq!(idle.since_unix_ms, restored.accepted_at_unix_ms);
        let history = store.seat_status_history("agent/cedar", now_ms()).unwrap();
        let states = history["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry["state"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(states, ["idle", "unauthenticated", "blocked", "idle"]);
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
        assert!(local_observation_position(&successor).is_none());
        let blocked = store.current_harness("agent/cedar").unwrap().unwrap();
        assert_eq!(blocked.state, "needs-login");
        assert_eq!(blocked.since_unix_ms, at);
        let history = store.seat_status_history("agent/cedar", now_ms()).unwrap();
        assert_eq!(
            history["items"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["state"] == "unauthenticated")
                .count(),
            1
        );
        publish(json!(true), 2, at + 2);
        let recovered = store.current_harness("agent/cedar").unwrap().unwrap();
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
        let blocked = store.current_harness("agent/cedar").unwrap().unwrap();
        assert_eq!(blocked.state, "needs-login");
        assert_eq!(
            blocked.since_unix_ms,
            u128::from(first.body["fields"]["observed_at_ms"].as_u64().unwrap())
        );
        let restored = native("idle", true);
        let idle = store.current_harness("agent/cedar").unwrap().unwrap();
        assert_eq!(idle.state, "idle");
        assert_eq!(
            idle.since_unix_ms,
            u128::from(restored.body["fields"]["observed_at_ms"].as_u64().unwrap())
        );
        let history = store.seat_status_history("agent/cedar", now_ms()).unwrap();
        let states = history["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry["state"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(states, ["unauthenticated", "idle"]);
    }

    #[test]
    fn status_prompt_transitions_and_repeats_agree_with_current_since() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        let at = now_ms();
        observe(&store, "one", "idle", at, "ready");
        let diagnostic = |code| {
            store.append_claim(&input("harness.diagnostic", json!({
            "incarnation_id":"one", "code":code, "reason":"fixture", "severity":"warning"
        }))).unwrap()
        };
        let prompt = diagnostic("provider-auth-expired");
        diagnostic("provider-auth-expired");
        let blocked = store.current_harness("agent/cedar").unwrap().unwrap();
        assert_eq!(blocked.state, "needs-login");
        assert_eq!(blocked.since_unix_ms, prompt.accepted_at_unix_ms);
        let restored = diagnostic("provider-auth-restored");
        let idle = store.current_harness("agent/cedar").unwrap().unwrap();
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

    #[test]
    fn categorical_current_is_source_monotonic_clears_asks_and_preserves_native_counts() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        let at = now_ms() as u64 - 1_000;
        let publish = |time: u64, blocked: &str, ask: &str, count: Value| {
            store.append_harness_current(&crate::harness_events::CurrentPublication {
                runtime_incarnation: "one".into(),
                claim: input("harness.current", json!({
                    "incarnation_id":"one", "state":"working", "observed_at_ms":time,
                    "observed_since_ms":at - 10, "blocked_on":blocked, "ask":ask,
                    "running_subagents":count, "background_jobs":0
                })),
            }).unwrap().0
        };
        let history_before = store.seat_status_history("agent/cedar", now_ms()).unwrap()["items"].clone();
        let blocked = publish(at, "human", "question", json!(3));
        assert_eq!(store.seat_status_history("agent/cedar", now_ms()).unwrap()["items"], history_before);
        store.append_claim(&input("harness.observed", json!({
            "incarnation_id":"one", "state":"working", "observed_at_ms":at - 100,
            "blocked_on":"none", "ask":"none", "reason":"old detail"
        }))).unwrap();
        let current = store.observed_harness_at("agent/cedar", store.index().unwrap()).unwrap().unwrap();
        assert_eq!(current.claim, blocked.id);
        assert_eq!(current.blocked_on.as_deref(), Some("human"));
        assert_eq!(current.running_subagents, Some(3));
        assert_eq!(current.background_jobs, Some(0));
        assert_eq!(current.since_unix_ms, u128::from(at - 10));
        let answered = publish(at + 1, "none", "none", json!(0));
        publish(at - 1, "human", "question", json!(9));
        let current = store.current_harness("agent/cedar").unwrap().unwrap();
        assert_eq!(current.claim, answered.id);
        assert_eq!(current.ask.as_deref(), Some("none"));
        assert_eq!(current.reason, None);
        assert_eq!(current.running_subagents, Some(0));
        assert_eq!(current.since_unix_ms, u128::from(at - 10));
        let tied = store.append_claim(&input("harness.observed", json!({
            "incarnation_id":"one", "state":"working", "observed_at_ms":at + 1,
            "blocked_on":"human", "ask":"question"
        }))).unwrap();
        assert_eq!(store.current_harness("agent/cedar").unwrap().unwrap().claim, answered.id);
        let history = store.seat_status_history("agent/cedar", now_ms()).unwrap();
        assert_eq!(history["items"].as_array().unwrap().iter().filter(|item| item["state"] == "working").count(), 1);
        let newer = store.append_claim(&input("harness.observed", json!({
            "incarnation_id":"one", "state":"idle", "observed_at_ms":at + 2,
            "blocked_on":null, "ask":null
        }))).unwrap();
        assert_ne!(newer.id, tied.id);
        let current = store.current_harness("agent/cedar").unwrap().unwrap();
        assert_eq!(current.claim, newer.id);
        assert_eq!(current.state, "idle");
        assert_eq!(current.running_subagents, None);
        assert_eq!(current.background_jobs, None);
        runtime(&store, "two");
        assert!(store.current_harness("agent/cedar").unwrap().is_none());
    }

    #[test]
    fn categorical_current_native_auth_and_runtime_retry_fences_ignore_delayed_history() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        let at = now_ms() as u64 - 1_000;
        let publication = |auth: Value, time: u64| crate::harness_events::CurrentPublication {
            runtime_incarnation: "one".into(),
            claim: input("harness.current", json!({
                "incarnation_id":"one", "state":"idle", "observed_at_ms":time,
                "provider_auth":auth, "background_jobs":2, "running_subagents":null
            })),
        };
        store.append_harness_current(&publication(json!(false), at)).unwrap();
        store.append_harness_current(&publication(Value::Null, at + 1)).unwrap();
        let blocked = store.current_harness("agent/cedar").unwrap().unwrap();
        assert_eq!(blocked.state, "needs-login");
        assert_eq!(blocked.background_jobs, Some(2));
        assert_eq!(blocked.since_unix_ms, u128::from(at));
        assert_eq!(store.seat_observation_at("agent/cedar", Some(&blocked), store.index().unwrap(), u128::from(at) + STALE_MS + 1).unwrap(), "current");
        let recovered = publication(json!(true), at + 2);
        let (claim, first) = store.append_harness_current(&recovered).unwrap();
        assert!(first);
        let (retry, appended) = store.append_harness_current(&recovered).unwrap();
        assert!(!appended);
        assert_eq!(retry.id, claim.id);
        store.append_claim(&input("harness.observed", json!({
            "incarnation_id":"one", "state":"working", "observed_at_ms":at,
            "provider_auth":false
        }))).unwrap();
        assert_eq!(store.current_harness("agent/cedar").unwrap().unwrap().state, "idle");
        runtime(&store, "two");
        assert!(store.append_harness_current(&recovered).is_err());
    }

    #[test]
    fn categorical_current_history_gaps_are_visible_independently_and_survive_runtime_reset() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        let at = now_ms() as u64 - 1_000;
        let before = store.seat_status_history("agent/cedar", now_ms()).unwrap();
        assert_eq!(before["complete"], true);
        store.append_harness_current(&crate::harness_events::CurrentPublication {
            runtime_incarnation: "one".into(),
            claim: input("harness.current", json!({
                "incarnation_id":"one", "state":"working", "observed_at_ms":at,
                "observed_since_ms":at - 10, "running_subagents":2,
                "history_gap_count":3, "history_gap_from_ms":at - 5,
                "history_gap_to_ms":at, "history_gap_reason":"cap-full"
            })),
        }).unwrap();
        let gap = store.harness_history_gap_at("agent/cedar", "one", store.index().unwrap()).unwrap().unwrap();
        assert_eq!((gap.count, gap.from_ms, gap.to_ms, gap.reason.as_str()), (3, at - 5, at, "cap-full"));
        let history = store.seat_status_history("agent/cedar", now_ms()).unwrap();
        assert_eq!(history["complete"], false);
        assert_eq!(history["items"], before["items"], "loss markers never invent missing transitions");
        assert_eq!(history["history_gaps"], json!([gap]));
        store.append_claim(&input("harness.observed", json!({
            "incarnation_id":"one", "state":"idle", "observed_at_ms":at + 1
        }))).unwrap();
        assert_eq!(store.current_harness("agent/cedar").unwrap().unwrap().state, "idle");
        assert_eq!(store.harness_history_gap_at("agent/cedar", "one", store.index().unwrap()).unwrap().unwrap().count, 3);
        runtime(&store, "two");
        assert!(store.harness_history_gap_at("agent/cedar", "two", store.index().unwrap()).unwrap().is_none());
        let reset = store.seat_status_history("agent/cedar", now_ms()).unwrap();
        assert_eq!(reset["complete"], false);
        assert_eq!(reset["history_gaps"], history["history_gaps"]);
        let expired = store.seat_status_history("agent/cedar", u128::from(at) + WINDOW_MS + 1).unwrap();
        assert_eq!(expired["history_gaps"], json!([]));
    }

    #[test]
    fn categorical_current_durable_gap_segments_survive_lost_fastlane_and_dedupe_with_aggregates() {
        let store = Store::open_memory("cedar").unwrap();
        runtime(&store, "one");
        let at = now_ms() as u64 - 1_000;
        let gap = |sequence: u64, count: u64, from: u64, to: u64| {
            let mut claim = input("harness.history.gap", json!({
                "runtime_incarnation":"one", "history_gap_count":count,
                "history_gap_from_ms":from, "history_gap_to_ms":to, "history_gap_reason":"cap-full"
            }));
            claim.idempotency_key = Some(format!("harness-gap:agent/cedar:one:{sequence}"));
            claim
        };
        let segment = gap(1, 2, at - 5, at);
        let first = store.append_claim(&segment).unwrap();
        assert_eq!(store.append_claim(&segment).unwrap().id, first.id);
        runtime(&store, "two");
        store.append_claim(&gap(2, 3, at + 1, at + 2)).unwrap();
        let history = store.seat_status_history("agent/cedar", now_ms()).unwrap();
        assert_eq!(history["complete"], false);
        assert_eq!(history["history_gaps"], json!([{
            "runtime_incarnation":"one", "count":5, "from_ms":at - 5,
            "to_ms":at + 2, "reason":"cap-full"
        }]));
        assert!(history["items"].as_array().unwrap().iter().all(|item| item["state"].is_null()));
        // A retained cumulative current claim is the same loss, not five more events.
        store.append_claim(&input("harness.current", json!({
            "incarnation_id":"one", "state":"working", "observed_at_ms":at + 3,
            "history_gap_count":5, "history_gap_from_ms":at - 5,
            "history_gap_to_ms":at + 2, "history_gap_reason":"cap-full"
        }))).unwrap();
        let merged = store.seat_status_history("agent/cedar", now_ms()).unwrap();
        assert_eq!(merged["history_gaps"], history["history_gaps"]);
        assert_eq!(merged["items"], history["items"]);
        assert!(store.current_harness("agent/cedar").unwrap().is_none());
    }
}
