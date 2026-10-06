//! Account limits: a recent reading of each paying account's 5-hour and weekly limits, and
//! the policy that stops an account's seats as it nears its weekly limit.
//!
//! A harness reports its account's limits with every status refresh, so every seat on an account
//! repeats them, and an idle seat keeps its last reading. An account's limits are therefore the
//! highest weekly reading within an hour of the freshest source observation, in the latest reset
//! window, even after seats switch away, labelled with its original time and measuring seat.
//! This also protects readers from older producers that re-stamp cached limits. Readings never
//! cross accounts or providers: a harness that names no account reads as `PROVIDER/unknown`.
//! Seatless identity-less history is hidden once that driver has identified evidence; it is
//! never attributed to an identified account, and active seats keep their unknown evidence.

use super::accounts::limit_binding_is_current;
use super::*;
use crate::model::DoctorCheck;
use serde::Deserialize;

/// The actor st records for what its limits policy does.
pub const LIMITS_ACTOR: &str = "daemon/limits";

const ACCOUNT_READING_WINDOW_MS: u64 = 3_600_000;

fn select_account_reading(readings: Vec<AccountLimit>) -> AccountLimit {
    // A partial snapshot after a relaunch can know only the five-hour window. It is not a
    // weekly observation and must neither erase nor freshen the last weekly evidence.
    let has_weekly = readings.iter().any(|limit| limit.weekly_percent.is_some());
    let readings = readings
        .into_iter()
        .filter(|limit| !has_weekly || limit.weekly_percent.is_some())
        .collect::<Vec<_>>();
    let latest = readings
        .iter()
        .map(|limit| limit.measured_at_unix_ms)
        .max()
        .unwrap();
    let since = latest.saturating_sub(ACCOUNT_READING_WINDOW_MS);
    // Reset advancement beats percentage: an old window must not resurrect exhaustion after
    // a reset, even when an older producer publishes that window again with a fresh stamp.
    let reset = readings
        .iter()
        .filter(|limit| limit.measured_at_unix_ms >= since)
        .filter_map(|limit| limit.weekly_resets_at_unix_ms)
        .max();
    readings
        .into_iter()
        .filter(|limit| {
            limit.measured_at_unix_ms >= since && limit.weekly_resets_at_unix_ms == reset
        })
        .max_by(|left, right| {
            left.weekly_percent
                .partial_cmp(&right.weekly_percent)
                .unwrap()
                .then_with(|| {
                    // Providers that report only a five-hour window use that window's level.
                    if left.weekly_percent.is_none() && right.weekly_percent.is_none() {
                        left.five_hour_resets_at_unix_ms
                            .cmp(&right.five_hour_resets_at_unix_ms)
                            .then_with(|| {
                                left.five_hour_percent
                                    .partial_cmp(&right.five_hour_percent)
                                    .unwrap()
                            })
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
                .then_with(|| {
                    (left.measured_at_unix_ms, &left.measured_by)
                        .cmp(&(right.measured_at_unix_ms, &right.measured_by))
                })
        })
        .expect("the latest reset has a recent reading")
}

/// Bound accounts remain distinct even when an older producer reports a generic provider label.
pub(super) fn episode_account(limit: &AccountLimit) -> String {
    limit
        .account_ref
        .as_ref()
        .map_or_else(|| limit.account.clone(), |name| format!("account/{name}"))
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AccountLimit {
    /// The account label, or `DRIVER/unknown` when the harness named none.
    pub account: String,
    /// The declared account (`ada/claude`) the measuring seat was launched on, when it was bound
    /// to one. Bound seats derive the opaque label from this name; unbound seats use provider identity.
    #[serde(default)]
    pub account_ref: Option<String>,
    /// Whether the source named a provider identity or a declared account. This says nothing
    /// about the age or completeness of its quota evidence.
    #[serde(default)]
    pub identified: bool,
    pub driver: String,
    pub plan: Option<String>,
    pub five_hour_percent: Option<f64>,
    pub five_hour_resets_at_unix_ms: Option<u64>,
    pub weekly_percent: Option<f64>,
    pub weekly_resets_at_unix_ms: Option<u64>,
    pub measured_at_unix_ms: u64,
    /// The seat whose harness measured this reading, and that seat's host.
    pub measured_by: String,
    pub host: String,
    /// Every seat whose newest reading names this account.
    pub seats: Vec<String>,
}

/// One node's `[limits]` policy.
#[derive(Clone, Debug)]
pub struct LimitsPolicy {
    pub stop_at_weekly_percent: u32,
    pub keep: BTreeSet<String>,
    pub notify: String,
    pub fresh_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct LimitsOutcome {
    /// Seats this node stopped in this pass.
    pub stopped: Vec<String>,
    /// Pooled seats this node restarted on another account in their pool, with that account.
    pub switched: Vec<(String, String)>,
    /// Operations messages this pass made or found.
    pub notified: Vec<String>,
}

pub(super) fn reading(origin: &str, body: &Value) -> Option<(AccountLimit, String)> {
    let fields = body.get("fields")?;
    let driver = fields["driver"].as_str()?.to_owned();
    let identity = fields["account"]
        .as_str()
        .filter(|account| !account.is_empty());
    let account_ref = fields["account_ref"]
        .as_str()
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    // Older bound producers can carry a generic label or none at all. Match the publishers'
    // stable declared label so one declared account cannot split as its provider label changes.
    let account = account_ref.as_ref().map_or_else(
        || identity.map_or_else(|| format!("{driver}/unknown"), str::to_owned),
        |name| st_drivers::account::account_label(&driver, &format!("declared:{name}")),
    );
    let identified = account_ref.is_some() || identity.is_some();
    let percent = |name: &str| fields[name].as_f64().filter(|value| value.is_finite());
    Some((
        AccountLimit {
            account: account.clone(),
            account_ref,
            identified,
            driver,
            plan: fields["plan"].as_str().map(str::to_owned),
            five_hour_percent: percent("five_hour_percent"),
            five_hour_resets_at_unix_ms: fields["five_hour_resets_at_unix_ms"].as_u64(),
            weekly_percent: percent("weekly_percent"),
            weekly_resets_at_unix_ms: fields["weekly_resets_at_unix_ms"].as_u64(),
            measured_at_unix_ms: fields["measured_at_unix_ms"].as_u64()?,
            measured_by: String::new(),
            host: origin.to_owned(),
            seats: Vec::new(),
        },
        account,
    ))
}

pub(super) fn account_limits_at(connection: &Connection) -> Result<Vec<AccountLimit>> {
    let mut statement = connection.prepare_cached(&canonical_sql(
        "SELECT subject, origin, body FROM claims WHERE kind='harness.limits'
         ORDER BY CANONICAL_ASC(claims)",
    ))?;
    let mut newest = BTreeMap::<String, (String, String, Option<String>)>::new();
    let mut readings = BTreeMap::<(String, String, Option<String>), Vec<AccountLimit>>::new();
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (subject, origin, body) = row?;
        let body: Value = serde_json::from_str(&body)?;
        if let Some((mut limit, _)) = reading(&origin, &body) {
            limit.measured_by = subject.clone();
            let key = (
                limit.driver.clone(),
                limit.account.clone(),
                limit.account_ref.clone(),
            );
            newest.insert(subject, key.clone());
            // Keep the last reading even after the account's last seat switches away. Otherwise
            // the exhausted account would immediately look unused to the next pool choice.
            readings.entry(key).or_default().push(limit);
        }
    }
    let mut accounts = readings
        .into_iter()
        .map(|(key, readings)| (key, select_account_reading(readings)))
        .collect::<BTreeMap<_, _>>();
    for (seat, key) in newest {
        accounts
            .get_mut(&key)
            .expect("the seat reported an account")
            .seats
            .push(seat);
    }
    let identified_drivers = accounts
        .values()
        .filter(|limit| limit.identified)
        .map(|limit| limit.driver.clone())
        .collect::<BTreeSet<_>>();
    let mut active = connection.prepare_cached("SELECT subject FROM desired WHERE kind='agent'")?;
    let active_seats = active
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<BTreeSet<_>, _>>()?;
    accounts.retain(|_, limit| {
        limit.identified
            || !identified_drivers.contains(&limit.driver)
            || limit.seats.iter().any(|seat| active_seats.contains(seat))
    });
    Ok(accounts.into_values().collect())
}

/// The host a seat is declared on, when its current declaration is an agent and not a stop.
fn seat_host(connection: &Connection, seat: &str) -> Result<Option<String>> {
    let row = connection
        .query_row(
            "SELECT kind, json_extract(member, '$.host') FROM desired WHERE subject=?1",
            [seat],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    Ok(row.and_then(|(kind, host)| (kind == "agent").then_some(host).flatten()))
}

fn utc(unix_ms: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(unix_ms as i64).map_or_else(
        || unix_ms.to_string(),
        |at| at.format("%Y-%m-%d %H:%M UTC").to_string(),
    )
}

impl Store {
    /// The highest recent weekly reading in each account's latest reset window, in account order.
    pub fn account_limits(&self) -> Result<Vec<AccountLimit>> {
        account_limits_at(&self.readers.get())
    }

    /// Missing quota evidence is unknown, never evidence that an account is below its limit.
    /// Keep the last reading visible and report unavailable weekly evidence for active seats.
    pub fn account_limits_check(&self, now: u128, fresh_ms: u64) -> Result<DoctorCheck> {
        let limits = self.account_limits()?;
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT subject, body FROM desired WHERE kind='agent' ORDER BY subject",
        )?;
        let mut issues = BTreeSet::new();
        let mut active_accounts = BTreeSet::new();
        for row in statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (seat, body) = row?;
            let desired: Value = serde_json::from_str(&body)?;
            let binding = crate::accounts::harness_binding(&desired);
            let mut reading = None;
            for limit in &limits {
                if let Some(binding) = &binding
                    && let crate::accounts::Binding::Account(name) = &binding.binding
                {
                    if limit.driver == binding.driver
                        && limit.account_ref.as_ref() == Some(name)
                    {
                        reading = Some(limit);
                        break;
                    }
                    continue;
                }
                if limit.seats.contains(&seat)
                    && limit_binding_is_current(&connection, &self.origin, &seat, limit)?
                {
                    reading = Some(limit);
                    break;
                }
            }
            if let Some(reading) = reading {
                active_accounts.insert(episode_account(reading));
            } else {
                let driver = desired["children"]
                    .as_array()
                    .and_then(|children| children.iter().find(|child| child["name"] == "harness"))
                    .and_then(|harness| harness["arguments"][0].as_str());
                if matches!(driver, Some("claude" | "codex")) {
                    let account = match binding.map(|binding| binding.binding) {
                        Some(crate::accounts::Binding::Account(name)) => format!("account/{name}"),
                        _ => seat,
                    };
                    issues.insert(format!("{account}: no weekly reading"));
                }
            }
        }
        for limit in limits {
            let account = episode_account(&limit);
            if !active_accounts.contains(&account)
                || !matches!(limit.driver.as_str(), "claude" | "codex")
            {
                continue;
            }
            let reason = if limit.weekly_percent.is_none() {
                Some("no weekly reading")
            } else if u128::from(limit.measured_at_unix_ms) > now.saturating_add(60_000) {
                Some("weekly observation is in the future")
            } else if limit
                .weekly_resets_at_unix_ms
                .is_some_and(|at| u128::from(at) <= now)
            {
                Some("no weekly reading since the reset")
            } else if now.saturating_sub(u128::from(limit.measured_at_unix_ms))
                > u128::from(fresh_ms)
            {
                Some("weekly reading is stale")
            } else {
                None
            };
            if let Some(reason) = reason {
                issues.insert(format!("{account}: {reason}"));
            }
        }
        Ok(DoctorCheck {
            name: "account-limits".into(),
            status: if issues.is_empty() { "pass" } else { "warn" }.into(),
            message: if issues.is_empty() {
                "active accounts have current weekly evidence, or use a harness without weekly limits".into()
            } else {
                format!(
                    "{}. Unavailable evidence is not below the limit. Automatic stops wait for fresh weekly evidence; inspect the provider login and quota reporting. The last source reading remains visible in `st usage --json`.",
                    issues.into_iter().collect::<Vec<_>>().join("; ")
                )
            },
        })
    }

    /// Stop the seats this node hosts on every account whose fresh weekly reading reached the
    /// policy's percentage, and notify operations once per weekly window. Each seat is
    /// stopped at most once per window, so a person can start it again.
    pub fn enforce_account_limits(
        &self,
        policy: &LimitsPolicy,
        now: u128,
    ) -> Result<LimitsOutcome, St3Error> {
        if !policy.notify.starts_with("agent/")
            || policy.notify.split('/').skip(1).any(str::is_empty)
            || policy.notify.chars().any(char::is_whitespace)
        {
            return Err(St3Error::new(
                "invalid-limits-recipient",
                "limits notifications require an operations agent",
            ));
        }
        let mut outcome = LimitsOutcome::default();
        for limit in self.account_limits().map_err(internal)? {
            let Some(weekly) = limit.weekly_percent else {
                continue;
            };
            if weekly < f64::from(policy.stop_at_weekly_percent)
                || limit
                    .weekly_resets_at_unix_ms
                    .is_some_and(|at| u128::from(at) <= now)
                || now.saturating_sub(u128::from(limit.measured_at_unix_ms))
                    > u128::from(policy.fresh_ms)
                || u128::from(limit.measured_at_unix_ms) > now.saturating_add(60_000)
            {
                continue;
            }
            let episode_account = episode_account(&limit);
            // One episode per weekly window. Without a reported reset, a UTC day stands in.
            let episode = limit.weekly_resets_at_unix_ms.map_or_else(
                || format!("day-{}", limit.measured_at_unix_ms / 86_400_000),
                |at| at.to_string(),
            );
            // A pooled seat moves to another account in its pool, so the stop becomes a switch;
            // every other seat on the account stops.
            let mut targets = Vec::new();
            let mut local = Vec::new();
            let mut local_switch = Vec::new();
            {
                let connection = self.readers.get();
                for seat in &limit.seats {
                    if policy.keep.contains(seat) {
                        continue;
                    }
                    let Some(host) = seat_host(&connection, seat).map_err(internal)? else {
                        continue;
                    };
                    if crate::suspension::current(self, seat)
                        .map_err(internal)?
                        .is_some_and(|suspension| suspension.holds_seat())
                    {
                        continue;
                    }
                    if !super::accounts::limit_binding_is_current(
                        &connection,
                        &self.origin,
                        seat,
                        &limit,
                    )
                    .map_err(internal)?
                    {
                        continue;
                    }
                    let binding = self.seat_binding(seat).map_err(internal)?;
                    let mut alternative = None;
                    if let Some(binding) = &binding {
                        // A pooled seat's reading names the account it ran on. A reading without
                        // one predates the binding, so it says nothing about this seat.
                        let Some(leaving) = limit.account_ref.as_deref() else {
                            continue;
                        };
                        if matches!(&binding.binding, crate::accounts::Binding::Account(name) if name != leaving)
                        {
                            continue;
                        }
                        alternative = self
                            .pool_alternatives(
                                binding,
                                &host,
                                leaving,
                                f64::from(policy.stop_at_weekly_percent),
                                now,
                            )
                            .map_err(internal)?
                            .into_iter()
                            .next();
                    }
                    if alternative.is_none() {
                        targets.push(seat.clone());
                    }
                    let handled_before: bool = connection
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM local_limit_stops
                             WHERE account=?1 AND episode=?2 AND seat=?3)",
                            params![episode_account, episode, seat],
                            |row| row.get(0),
                        )
                        .map_err(internal)?;
                    if host == self.origin && !handled_before {
                        match alternative {
                            Some(account) => local_switch.push((seat.clone(), account)),
                            None => local.push(seat.clone()),
                        }
                    }
                }
            }
            for (seat, account) in local_switch {
                if self.switch_seat_account(&seat, &limit, &account, &episode, now)? {
                    outcome.switched.push((seat, account));
                }
            }
            let mut stopped_here = Vec::new();
            for seat in local {
                let kdl = format!("version 2\nstop {seat:?}\n");
                let intent = crate::graph::parse_internal_intent(&kdl, &self.origin)?;
                let planned = self.mission(
                    &intent,
                    IntentInput {
                        kdl: kdl.clone(),
                        source_name: Some("st limits".into()),
                    },
                )?;
                self.apply_as(
                    &intent,
                    &planned.subject_tokens,
                    &format!("limits-stop:{episode_account}:{episode}:{seat}"),
                    Some(LIMITS_ACTOR),
                )?;
                self.connection
                    .batched(|tx| {
                        tx.execute(
                            "INSERT OR IGNORE INTO local_limit_stops(account, episode, seat, stopped_at_unix_ms)
                             VALUES (?1, ?2, ?3, ?4)",
                            params![episode_account, episode, seat, now as i64],
                        )
                    })
                    .map_err(internal)?
                    .map_err(internal)?;
                stopped_here.push(seat.clone());
                outcome.stopped.push(seat);
            }
            // The node that measured the reading notifies operations, so one node writes.
            if targets.is_empty() || limit.host != self.origin {
                continue;
            }
            let provider = limit.account.split('/').next().unwrap_or("").to_owned();
            let provider = provider
                .chars()
                .next()
                .map(|first| {
                    first.to_uppercase().collect::<String>() + &provider[first.len_utf8()..]
                })
                .unwrap_or_default();
            let plural = if targets.len() == 1 { "" } else { "s" };
            let kept = if policy.keep.is_empty() {
                String::new()
            } else {
                format!(
                    " except {}",
                    policy.keep.iter().cloned().collect::<Vec<_>>().join(", ")
                )
            };
            let reset = limit.weekly_resets_at_unix_ms.map_or_else(
                || "The harness does not report when the weekly limit resets.".to_owned(),
                |at| format!("It resets {}.", utc(at)),
            );
            let title = format!(
                "{provider} account {} is at {weekly:.0}% of its weekly limit: {} affected seat{plural}",
                limit.account,
                targets.len()
            );
            let reason = format!(
                "{} reached {weekly:.0}% of its weekly limit ({} measured it at {}). The policy stops \
                 seats that use it{kept}. Affected seats across the fleet: {}. This pass stopped \
                 {} on {}. Other hosts enforce their own stops. {reset} A seat restarted by a \
                 person stays running until the reset. Review the account and stopped seats, group related alerts, and act \
                 on standing instructions. Ask a person only if a decision is needed, using a \
                 structured request with your recommendation, reasons and proposed action.",
                limit.account,
                limit.measured_by,
                utc(limit.measured_at_unix_ms),
                targets.join(", "),
                if stopped_here.is_empty() {
                    "none".into()
                } else {
                    stopped_here.join(", ")
                },
                self.origin,
            );
            // The payload is pinned by account/window. A later reading must not turn a retry
            // into an idempotency conflict or recreate an already handled notification.
            let key = format!("limits-notify:{episode_account}:{episode}");
            let subject = format!(
                "message/limits-{}",
                &canonical_hash(&key).map_err(internal)?[..20]
            );
            self.connection.batched(|tx| {
                let recorded = tx.query_row(
                    "SELECT 1 FROM claims WHERE subject=?1 AND kind='message.sent' LIMIT 1",
                    [&subject], |_| Ok(()),
                ).optional().map_err(internal)?.is_some();
                if !recorded {
                    append_claim_tx(tx, &self.origin, &subject, "message.sent", Some(LIMITS_ACTOR),
                        &json!({"fields": {
                            "from": LIMITS_ACTOR, "to": policy.notify,
                            "title": title, "content": reason, "status": "sent",
                            "tags": ["account-limits", format!("account:{}", limit.account), format!("window:{episode}")]
                        }, "evidence": []}), &[], None).map_err(claim_append_error)?;
                }
                Ok::<_, St3Error>(())
            }).map_err(internal)??;
            outcome.notified.push(subject);
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u128 = 60 * 60 * 1000;

    fn declare(store: &Store, seat: &str, host: &str) {
        let name = seat.trim_start_matches("agent/");
        let intent = crate::graph::parse_internal_intent(
            &format!("version 2\nagent {name:?} {{ workspace \"/tmp\"; command \"true\"; }}"),
            "alder",
        )
        .unwrap();
        store
            .apply_internal(&intent, &format!("declare-{seat}"))
            .unwrap();
        // The reconciler resolves where each seat runs; these seats run where the test says.
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE desired SET member=json_object('host', ?2) WHERE subject=?1",
                params![seat, host],
            )
            .unwrap();
    }

    fn read(store: &Store, seat: &str, account: Option<&str>, weekly: f64, measured_at: u128) {
        let mut fields = BTreeMap::from([
            ("driver".into(), json!("claude")),
            ("incarnation_id".into(), json!("inc-1")),
            ("weekly_percent".into(), json!(weekly)),
            ("five_hour_percent".into(), json!(40.0)),
            (
                "weekly_resets_at_unix_ms".into(),
                json!(1_800_000_000_000_u64),
            ),
            ("measured_at_unix_ms".into(), json!(measured_at as u64)),
        ]);
        if let Some(account) = account {
            fields.insert("account".into(), json!(account));
        }
        store
            .append_claim(&ClaimInput {
                subject: seat.into(),
                kind: "harness.limits".into(),
                actor: Some(seat.into()),
                fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("limits-{seat}-{weekly}-{measured_at}")),
            })
            .unwrap();
    }

    fn live(store: &Store, seat: &str) -> bool {
        seat_host(&store.readers.get(), seat).unwrap().is_some()
    }

    #[test]
    fn a_partial_report_after_restart_preserves_the_weekly_source_time() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claims.sqlite3");
        let now = now_ms();
        {
            let store = Store::open(&path, "alder").unwrap();
            declare(&store, "agent/alder.worker", "alder");
            read(
                &store,
                "agent/alder.worker",
                Some("claude/example"),
                95.0,
                now,
            );
        }
        let store = Store::open(&path, "alder").unwrap();
        // A restarted harness knows its five-hour window before it knows its weekly window.
        store
            .append_claim(&ClaimInput {
                subject: "agent/alder.worker".into(),
                kind: "harness.limits".into(),
                actor: Some("agent/alder.worker".into()),
                fields: BTreeMap::from([
                    ("driver".into(), json!("claude")),
                    ("account".into(), json!("claude/example")),
                    ("five_hour_percent".into(), json!(1.0)),
                    ("measured_at_unix_ms".into(), json!((now + HOUR + 1) as u64)),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let limit = &store.account_limits().unwrap()[0];
        assert_eq!(limit.weekly_percent, Some(95.0));
        assert_eq!(limit.measured_at_unix_ms, now as u64);
        assert_eq!(
            store
                .enforce_account_limits(&policy(), now + HOUR + 1)
                .unwrap(),
            LimitsOutcome::default()
        );
        assert!(
            store
                .account_limits_check(now + HOUR + 1, HOUR as u64)
                .unwrap()
                .message
                .contains("is stale")
        );
    }

    #[test]
    fn a_bound_seat_restarting_on_the_same_account_does_not_need_its_own_quota() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claims.sqlite3");
        let now = now_ms();
        {
            let store = Store::open(&path, "alder").unwrap();
            declare_accounts(&store);
            read_account(
                &store,
                "agent/alder.single",
                "ada/one",
                "codex/aaaa",
                95.0,
                now,
            );
        }
        let store = Store::open(&path, "alder").unwrap();
        store
            .append_claim(&ClaimInput {
                subject: "agent/alder.single".into(),
                kind: "runtime.observed".into(),
                actor: Some("daemon/alder".into()),
                fields: BTreeMap::from([
                    ("status".into(), json!("running")),
                    ("runtime_id".into(), json!("alder.single")),
                    ("incarnation_id".into(), json!("inc-2")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        assert_eq!(
            store
                .enforce_account_limits(&policy(), now + 1)
                .unwrap()
                .stopped,
            ["agent/alder.single"]
        );
    }

    #[test]
    fn recent_maximum_expires_and_does_not_cross_weekly_reset_windows() {
        let store = Store::open_memory("alder").unwrap();
        let now = now_ms();
        let busy = "agent/alder.busy";
        let stale = "agent/alder.stale";
        read(&store, busy, Some("claude/aaaa"), 97.0, now);
        read(&store, stale, Some("claude/aaaa"), 45.0, now + 1);
        assert_eq!(store.account_limits().unwrap()[0].weekly_percent, Some(97.0));
        // Arrival order cannot override source time, even outside the maximum's window.
        read(&store, stale, Some("claude/aaaa"), 8.0, now - 2 * HOUR);
        assert_eq!(store.account_limits().unwrap()[0].weekly_percent, Some(97.0));
        read(&store, stale, Some("claude/aaaa"), 46.0, now + HOUR + 1);
        assert_eq!(store.account_limits().unwrap()[0].weekly_percent, Some(46.0));

        let mut next = store.latest_claim(busy, Some("harness.limits")).unwrap().unwrap();
        next.body["fields"]["weekly_resets_at_unix_ms"] = json!(1_800_600_000_000_u64);
        next.body["fields"]["weekly_percent"] = json!(2.0);
        next.body["fields"]["measured_at_unix_ms"] = json!((now + HOUR + 2) as u64);
        store.append_claim(&ClaimInput {
            subject: busy.into(), kind: "harness.limits".into(), actor: Some(busy.into()),
            fields: serde_json::from_value(next.body["fields"].clone()).unwrap(),
            evidence: vec![], expected_subject: None, idempotency_key: None,
        }).unwrap();
        // An old producer re-publishes the previous reset with an even newer timestamp.
        read(&store, stale, Some("claude/aaaa"), 99.0, now + HOUR + 3);
        let limit = &store.account_limits().unwrap()[0];
        assert_eq!(limit.weekly_percent, Some(2.0));
        assert_eq!(limit.measured_by, busy);
    }

    #[test]
    fn an_account_past_its_weekly_limit_stops_its_local_seats_once_and_notifies_operations_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claims.sqlite3");
        let store = Store::open(&path, "alder").unwrap();
        let [busy, idle, coordinator, remote, other] = [
            "agent/alder.busy",
            "agent/alder.idle",
            "agent/alder.coordinator",
            "agent/alder.remote",
            "agent/alder.other",
        ];
        for seat in [busy, idle, coordinator, other] {
            declare(&store, seat, "alder");
        }
        declare(&store, remote, "birch");
        let now = now_ms();
        // The idle seat still shows the account's old reading; the freshest one decides.
        read(&store, idle, Some("claude/aaaa"), 80.0, now - HOUR / 2);
        read(&store, busy, Some("claude/aaaa"), 96.0, now - 60_000);
        read(
            &store,
            coordinator,
            Some("claude/aaaa"),
            95.0,
            now - 120_000,
        );
        read(&store, remote, Some("claude/aaaa"), 90.0, now - HOUR / 4);
        read(&store, other, None, 99.0, now - 4 * HOUR);

        let limits = store.account_limits().unwrap();
        assert_eq!(
            limits
                .iter()
                .map(|limit| limit.account.as_str())
                .collect::<Vec<_>>(),
            ["claude/aaaa", "claude/unknown"]
        );
        let account = &limits[0];
        assert_eq!(account.weekly_percent, Some(96.0));
        assert_eq!(account.measured_by, busy);
        assert_eq!(account.seats, [busy, coordinator, idle, remote]);

        let policy = LimitsPolicy {
            stop_at_weekly_percent: 95,
            keep: BTreeSet::from([coordinator.to_owned()]),
            notify: "agent/alder.operations".into(),
            fresh_ms: HOUR as u64,
        };
        let outcome = store.enforce_account_limits(&policy, now).unwrap();
        assert_eq!(
            outcome.stopped,
            [busy, idle],
            "this node's seats, never the kept one"
        );
        assert_eq!(outcome.notified.len(), 1);
        assert!(!live(&store, busy) && !live(&store, idle));
        assert!(live(&store, coordinator) && live(&store, remote));
        assert!(live(&store, other), "a stale reading is not acted on");
        assert!(
            store
                .attention_items(Some("person/avery"))
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .claims_for_kind_at("work.person-asked", None, false, 1)
                .unwrap()
                .claims
                .is_empty()
        );
        let message = store.message(&outcome.notified[0]).unwrap().unwrap();
        assert_eq!(message.to, "agent/alder.operations");
        assert!(
            message
                .title
                .as_deref()
                .unwrap()
                .contains("claude/aaaa is at 96% of its weekly limit: 3 affected seats")
        );
        assert!(message.content.contains("structured request"));
        assert!(message.content.contains(busy) && message.content.contains(idle));
        assert!(message.content.contains("It resets"));

        // Restart the store, then start a stopped seat again. A newer percentage neither
        // stops that seat again nor resends the notification in the same window.
        drop(store);
        let store = Store::open(&path, "alder").unwrap();
        declare(&store, busy, "alder");
        read(&store, busy, Some("claude/aaaa"), 97.0, now + 60_000);
        let again = store.enforce_account_limits(&policy, now + 60_000).unwrap();
        assert!(again.stopped.is_empty(), "{again:?}");
        assert!(live(&store, busy));
        assert_eq!(again.notified, outcome.notified);
        let original = store
            .claims_for(&outcome.notified[0], Some("message.sent"))
            .unwrap();
        assert_eq!(
            original.len(),
            1,
            "a changed percentage must not resend the alert"
        );
        assert_eq!(
            store
                .message(&outcome.notified[0])
                .unwrap()
                .unwrap()
                .content,
            message.content
        );

        // A new window can notify operations again after the seats are started.
        declare(&store, idle, "alder");
        let mut next = store
            .latest_claim(busy, Some("harness.limits"))
            .unwrap()
            .unwrap();
        next.body["fields"]["weekly_resets_at_unix_ms"] = json!(1_800_600_000_000_u64);
        store
            .append_claim(&ClaimInput {
                subject: busy.into(),
                kind: "harness.limits".into(),
                actor: Some(busy.into()),
                fields: serde_json::from_value(next.body["fields"].clone()).unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("next-window".into()),
            })
            .unwrap();
        let next_window = store
            .enforce_account_limits(&policy, now + 120_000)
            .unwrap();
        assert_eq!(next_window.stopped, [busy, idle]);
        assert_eq!(next_window.notified.len(), 1);
        assert_ne!(next_window.notified, outcome.notified);
        assert!(
            store
                .claims_for_kind_at("work.person-asked", None, false, 1)
                .unwrap()
                .claims
                .is_empty()
        );
    }

    fn declare_accounts(store: &Store) {
        let intent = crate::graph::parse_internal_intent(
            r#"version 2
account "ada/one" { provider "openai"; owner "person/ada"; login "/srv/logins/one"; }
account "ada/two" { provider "openai"; owner "person/ada"; login "/srv/logins/two"; }
agent "pooled" { workspace "/tmp"; harness "codex" { account-pool "person/ada"; }; }
agent "single" { workspace "/tmp"; harness "codex" { account "ada/one"; }; }
agent "other" { workspace "/tmp"; harness "codex" { account-pool "person/ada"; }; }
"#,
            "alder",
        )
        .unwrap();
        store.apply_internal(&intent, "declare-accounts").unwrap();
    }

    fn read_account(
        store: &Store,
        seat: &str,
        account: &str,
        label: &str,
        weekly: f64,
        measured_at: u128,
    ) {
        store
            .append_claim(&ClaimInput {
                subject: seat.into(),
                kind: "harness.limits".into(),
                actor: Some(seat.into()),
                fields: BTreeMap::from([
                    ("driver".into(), json!("codex")),
                    ("incarnation_id".into(), json!("inc-1")),
                    ("account".into(), json!(label)),
                    ("account_ref".into(), json!(account)),
                    ("weekly_percent".into(), json!(weekly)),
                    (
                        "weekly_resets_at_unix_ms".into(),
                        json!(1_800_000_000_000_u64),
                    ),
                    ("measured_at_unix_ms".into(), json!(measured_at as u64)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("limits-{seat}-{account}-{weekly}-{measured_at}")),
            })
            .unwrap();
    }

    fn policy() -> LimitsPolicy {
        LimitsPolicy {
            stop_at_weekly_percent: 95,
            keep: BTreeSet::new(),
            notify: "agent/alder.operations".into(),
            fresh_ms: HOUR as u64,
        }
    }

    #[test]
    fn legacy_unknowns_are_hidden_only_with_identified_evidence_and_no_active_seat() {
        let store = Store::open_memory("alder").unwrap();
        let now = now_ms();
        let seat = "agent/alder.worker";
        read(&store, seat, None, 99.0, now - 3 * 24 * HOUR);
        let unknown = store.account_limits().unwrap();
        assert_eq!(unknown.len(), 1);
        assert!(!unknown[0].identified);
        // With no identified evidence this remains unknown, even without a declaration.
        assert_eq!(unknown[0].account, "claude/unknown");
        read(&store, seat, Some("claude/one"), 100.0, now);
        read(&store, seat, Some("claude/two"), 20.0, now + 1);
        read(&store, "agent/alder.other", Some("claude/three"), 30.0, now);
        let limits = store.account_limits().unwrap();
        assert_eq!(limits.len(), 3);
        assert!(limits.iter().all(|limit| limit.identified));
        let exhausted = limits
            .iter()
            .find(|limit| limit.account == "claude/one")
            .unwrap();
        assert!(exhausted.seats.is_empty());
        assert_eq!(exhausted.weekly_percent, Some(100.0));

        // Missing identity from a currently declared seat remains visible regardless of age.
        let active = "agent/alder.unknown";
        declare(&store, active, "alder");
        read(&store, active, None, 90.0, now - 3 * 24 * HOUR);
        let limits = store.account_limits().unwrap();
        let unknown = limits.iter().find(|limit| !limit.identified).unwrap();
        assert_eq!(unknown.seats, [active]);
        let stop =
            crate::graph::parse_internal_intent(&format!("version 2\nstop {active:?}\n"), "alder")
                .unwrap();
        store.apply_internal(&stop, "retire-unknown").unwrap();
        // A retired seat's last reading still has a seat string, but cannot keep a phantom alive.
        assert!(
            store
                .account_limits()
                .unwrap()
                .iter()
                .all(|limit| limit.identified)
        );
    }

    #[test]
    fn identified_evidence_and_grouping_are_scoped_to_the_driver() {
        let store = Store::open_memory("alder").unwrap();
        let now = now_ms() as u64;
        for (seat, driver, account, weekly) in [
            ("agent/example/claude", "claude", "shared-label", 95.0),
            ("agent/example/claude-old", "claude", "", 99.0),
            ("agent/example/codex", "codex", "", 20.0),
        ] {
            store
                .append_claim(&ClaimInput {
                    subject: seat.into(),
                    kind: "harness.limits".into(),
                    actor: Some(seat.into()),
                    fields: serde_json::from_value(json!({
                        "driver": driver, "account": account, "account_ref": "",
                        "weekly_percent": weekly, "measured_at_unix_ms": now,
                    }))
                    .unwrap(),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        let limits = store.account_limits().unwrap();
        assert_eq!(limits.len(), 2);
        assert_eq!(limits[0].account, "shared-label");
        assert!(limits[0].identified);
        assert_eq!(limits[1].account, "codex/unknown");
        assert!(!limits[1].identified);
        store
            .append_claim(&ClaimInput {
                subject: "agent/example/codex".into(),
                kind: "harness.limits".into(),
                actor: Some("agent/example/codex".into()),
                fields: serde_json::from_value(json!({
                    "driver": "codex", "account": "shared-label", "weekly_percent": 10.0,
                    "measured_at_unix_ms": now,
                }))
                .unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let limits = store.account_limits().unwrap();
        assert_eq!(limits.len(), 2);
        assert_eq!(limits[0].weekly_percent, Some(95.0));
        assert_eq!(limits[1].weekly_percent, Some(10.0));
        assert!(limits.iter().all(|limit| limit.identified));
    }

    #[test]
    fn bound_readings_with_different_provider_labels_do_not_split_or_merge_accounts() {
        let store = Store::open_memory("alder").unwrap();
        let now = now_ms();
        read_account(&store, "agent/alder.first", "ada/one", "", 95.0, now);
        read_account(
            &store,
            "agent/alder.second",
            "ada/one",
            "codex/generic",
            20.0,
            now + 1,
        );
        read_account(
            &store,
            "agent/alder.third",
            "ada/two",
            "codex/generic",
            30.0,
            now,
        );
        let limits = store.account_limits().unwrap();
        assert_eq!(limits.len(), 2);
        let first = limits
            .iter()
            .find(|limit| limit.account_ref.as_deref() == Some("ada/one"))
            .unwrap();
        assert!(first.identified);
        assert_eq!(first.weekly_percent, Some(95.0));
        assert_eq!(first.seats, ["agent/alder.first", "agent/alder.second"]);
        assert_eq!(
            first.account,
            st_drivers::account::account_label("codex", "declared:ada/one")
        );
        assert_ne!(limits[0].account, limits[1].account);
    }

    #[test]
    fn an_exhausted_account_keeps_its_reading_after_its_last_seat_switches_away() {
        let store = Store::open_memory("alder").unwrap();
        declare_accounts(&store);
        let seat = "agent/alder.pooled";
        let now = now_ms();
        read_account(&store, seat, "", "", 100.0, now - 3 * 24 * HOUR);
        read_account(&store, seat, "ada/one", "codex/aaaa", 97.0, now);
        read_account(&store, seat, "ada/two", "codex/bbbb", 20.0, now + 1);

        let limits = store.account_limits().unwrap();
        let exhausted = limits
            .iter()
            .find(|limit| limit.account_ref.as_deref() == Some("ada/one"))
            .unwrap();
        assert_eq!(limits.len(), 2);
        assert_eq!(exhausted.weekly_percent, Some(97.0));
        assert!(exhausted.seats.is_empty());
        let binding = store.seat_binding(seat).unwrap().unwrap();
        assert!(
            store
                .pool_alternatives(&binding, "alder", "ada/two", 95.0, now + 2)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn declared_accounts_without_provider_identities_keep_separate_limits() {
        let store = Store::open_memory("alder").unwrap();
        declare_accounts(&store);
        let now = now_ms();
        read_account(&store, "agent/alder.pooled", "ada/one", "", 97.0, now);
        read_account(&store, "agent/alder.other", "ada/two", "", 20.0, now + 1);
        let limits = store.account_limits().unwrap();
        assert_eq!(limits.len(), 2);
        let first = limits.iter().find(|limit| limit.account_ref.as_deref() == Some("ada/one")).unwrap();
        let second = limits.iter().find(|limit| limit.account_ref.as_deref() == Some("ada/two")).unwrap();
        assert_eq!(first.weekly_percent, Some(97.0));
        assert_eq!(second.weekly_percent, Some(20.0));
        let outcome = store.enforce_account_limits(&policy(), now + 2).unwrap();
        assert_eq!(
            outcome.switched,
            [("agent/alder.pooled".to_owned(), "ada/two".to_owned())]
        );
        assert!(outcome.stopped.is_empty());
    }

    #[test]
    fn a_sole_reporter_stops_after_exhausting_both_accounts_in_one_weekly_window() {
        let store = Store::open_memory("alder").unwrap();
        declare_accounts(&store);
        let now = now_ms();
        let seat = "agent/alder.pooled";
        read_account(&store, seat, "ada/one", "codex/aaaa", 97.0, now);
        assert_eq!(
            store
                .enforce_account_limits(&policy(), now)
                .unwrap()
                .switched,
            [(seat.to_owned(), "ada/two".to_owned())]
        );
        // The first account's reading cannot stop the new binding before it reports.
        assert_eq!(
            store.enforce_account_limits(&policy(), now + 1).unwrap(),
            LimitsOutcome::default()
        );
        read_account(&store, seat, "ada/two", "codex/bbbb", 97.0, now + 1);
        let outcome = store.enforce_account_limits(&policy(), now + 2).unwrap();
        assert!(outcome.switched.is_empty());
        assert_eq!(outcome.stopped, [seat]);
        assert_eq!(
            store.seat_account_choice(seat).unwrap().as_deref(),
            Some("ada/two")
        );
    }

    #[test]
    fn accounts_with_the_same_legacy_label_have_separate_handled_episodes() {
        let store = Store::open_memory("alder").unwrap();
        declare_accounts(&store);
        let now = now_ms();
        let seat = "agent/alder.pooled";
        read_account(&store, seat, "ada/one", "codex/unknown", 97.0, now);
        assert_eq!(
            store
                .enforce_account_limits(&policy(), now)
                .unwrap()
                .switched,
            [(seat.to_owned(), "ada/two".to_owned())]
        );
        read_account(&store, seat, "ada/two", "codex/unknown", 97.0, now + 1);
        assert_eq!(
            store
                .enforce_account_limits(&policy(), now + 2)
                .unwrap()
                .stopped,
            [seat]
        );
    }

    #[test]
    fn an_interrupted_switch_rolls_back_choice_restart_and_episode_and_can_retry() {
        let store = Store::open_memory("alder").unwrap();
        declare_accounts(&store);
        let now = now_ms();
        let seat = "agent/alder.pooled";
        store.choose_seat_account(seat, "ada/one", now).unwrap();
        read_account(&store, seat, "ada/one", "codex/aaaa", 97.0, now);
        // Fail the final write after the new choice and restart claim have been inserted.
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_account_switch BEFORE INSERT ON local_limit_stops
             BEGIN SELECT RAISE(ABORT, 'injected account switch interruption'); END;",
            )
            .unwrap();
        assert!(store.enforce_account_limits(&policy(), now).is_err());
        assert_eq!(
            store.seat_account_choice(seat).unwrap().as_deref(),
            Some("ada/one")
        );
        assert!(
            store
                .claims_for(seat, Some("runtime.action.requested"))
                .unwrap()
                .is_empty()
        );
        let markers: u64 = store
            .readers
            .get()
            .query_row("SELECT COUNT(*) FROM local_limit_stops", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(markers, 0);
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_account_switch")
            .unwrap();
        let retried = store.enforce_account_limits(&policy(), now + 1).unwrap();
        assert_eq!(retried.switched, [(seat.to_owned(), "ada/two".to_owned())]);
        assert_eq!(
            store
                .claims_for(seat, Some("runtime.action.requested"))
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .enforce_account_limits(&policy(), now + 2)
                .unwrap()
                .switched
                .is_empty()
        );
    }

    #[test]
    fn a_person_restarting_a_stopped_seat_is_not_stopped_twice_in_the_same_window() {
        let store = Store::open_memory("alder").unwrap();
        declare_accounts(&store);
        let now = now_ms();
        let seat = "agent/alder.single";
        read_account(&store, seat, "ada/one", "codex/aaaa", 97.0, now);
        assert_eq!(
            store
                .enforce_account_limits(&policy(), now)
                .unwrap()
                .stopped,
            [seat]
        );
        let intent = crate::graph::parse_internal_intent(
            "version 2\nagent \"single\" { workspace \"/tmp\"; harness \"codex\" { account \"ada/one\"; }; }",
            "alder",
        ).unwrap();
        store.apply_internal(&intent, "person-restart").unwrap();
        store.append_claim(&ClaimInput {
            subject: seat.into(), kind: "runtime.observed".into(), actor: None,
            fields: serde_json::from_value(json!({"status": "running", "runtime_id": "alder.single", "incarnation_id": "inc-two"})).unwrap(),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
        let again = store.enforce_account_limits(&policy(), now).unwrap();
        assert!(again.stopped.is_empty());
        assert!(again.switched.is_empty());
        assert_eq!(again.notified.len(), 1);
        assert!(live(&store, seat));
    }

    #[test]
    fn missing_stale_and_reset_weekly_evidence_are_reported_as_unknown() {
        let store = Store::open_memory("alder").unwrap();
        let intent = crate::graph::parse_internal_intent(
            "version 2\nagent \"worker\" { workspace \"/tmp\"; harness \"claude\" {}; }",
            "alder",
        )
        .unwrap();
        store.apply_internal(&intent, "weekly-health").unwrap();
        let now = now_ms();
        let check = store.account_limits_check(now, HOUR as u64).unwrap();
        assert_eq!(check.status, "warn");
        assert!(check.message.contains("no weekly reading"));
        assert!(check.message.contains("not below the limit"));
        store
            .append_claim(&ClaimInput {
                subject: "agent/alder.worker".into(),
                kind: "harness.limits".into(),
                actor: Some("agent/alder.worker".into()),
                fields: BTreeMap::from([
                    ("driver".into(), json!("claude")),
                    ("account".into(), json!("claude/example")),
                    ("five_hour_percent".into(), json!(1.0)),
                    ("measured_at_unix_ms".into(), json!(now as u64)),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        assert!(
            store
                .account_limits_check(now, HOUR as u64)
                .unwrap()
                .message
                .contains("no weekly reading")
        );
        assert_eq!(
            store.enforce_account_limits(&policy(), now).unwrap(),
            LimitsOutcome::default()
        );
        read(
            &store,
            "agent/alder.worker",
            Some("claude/example"),
            95.0,
            now,
        );
        assert_eq!(
            store.account_limits_check(now, HOUR as u64).unwrap().status,
            "pass"
        );
        let stale = now + HOUR + 1;
        assert!(
            store
                .account_limits_check(stale, HOUR as u64)
                .unwrap()
                .message
                .contains("is stale")
        );
        assert_eq!(
            store.enforce_account_limits(&policy(), stale).unwrap(),
            LimitsOutcome::default()
        );
        assert!(live(&store, "agent/alder.worker"));
        let reset = 1_800_000_000_001;
        assert!(
            store
                .account_limits_check(reset, HOUR as u64)
                .unwrap()
                .message
                .contains("since the reset")
        );
        assert_eq!(
            store.enforce_account_limits(&policy(), reset).unwrap(),
            LimitsOutcome::default()
        );
    }

    #[test]
    fn a_declared_account_with_no_quota_source_is_reported() {
        let store = Store::open_memory("alder").unwrap();
        declare_accounts(&store);
        let check = store.account_limits_check(now_ms(), HOUR as u64).unwrap();
        assert_eq!(check.status, "warn");
        assert!(check.message.contains("account/ada/one: no weekly reading"));
        assert_eq!(
            store.enforce_account_limits(&policy(), now_ms()).unwrap(),
            LimitsOutcome::default()
        );
    }

    #[test]
    fn a_single_account_rebinding_ignores_the_previous_accounts_limits() {
        let store = Store::open_memory("alder").unwrap();
        declare_accounts(&store);
        let now = now_ms();
        let seat = "agent/alder.single";
        read_account(&store, seat, "ada/one", "codex/aaaa", 97.0, now);
        let intent = crate::graph::parse_internal_intent(
            "version 2\nagent \"single\" { workspace \"/tmp\"; harness \"codex\" { account \"ada/two\"; }; }\n",
            "alder",
        ).unwrap();
        store.apply_internal(&intent, "rebind-single").unwrap();
        assert_eq!(
            store.enforce_account_limits(&policy(), now).unwrap(),
            LimitsOutcome::default()
        );
        assert!(live(&store, seat));
    }

    #[test]
    fn an_account_with_a_reset_weekly_window_does_not_stop_its_seats() {
        let store = Store::open_memory("alder").unwrap();
        declare_accounts(&store);
        let now = 1_800_000_000_001;
        read_account(
            &store,
            "agent/alder.pooled",
            "ada/one",
            "codex/aaaa",
            97.0,
            now - 2,
        );
        assert_eq!(
            store.enforce_account_limits(&policy(), now).unwrap(),
            LimitsOutcome::default()
        );
    }

    #[test]
    fn a_pooled_seat_at_its_limit_restarts_on_another_account_and_a_single_account_seat_stops() {
        let store = Store::open_memory("alder").unwrap();
        declare_accounts(&store);
        let [pooled, single] = ["agent/alder.pooled", "agent/alder.single"];
        let now = now_ms();
        read_account(&store, pooled, "ada/one", "codex/aaaa", 96.0, now - 60_000);
        read_account(&store, single, "ada/one", "codex/aaaa", 96.0, now - 90_000);
        // Another seat on the other account shows what it has left.
        read_account(
            &store,
            "agent/alder.other",
            "ada/two",
            "codex/bbbb",
            40.0,
            now - 30_000,
        );

        let outcome = store.enforce_account_limits(&policy(), now).unwrap();

        assert_eq!(
            outcome.switched,
            [(pooled.to_owned(), "ada/two".to_owned())]
        );
        assert_eq!(
            outcome.stopped,
            [single],
            "a single account has nowhere to go"
        );
        assert!(live(&store, pooled) && !live(&store, single));
        assert_eq!(
            store.seat_account_choice(pooled).unwrap().as_deref(),
            Some("ada/two"),
            "the restart starts on the new account"
        );
        let restart = store
            .claims_for(pooled, Some("runtime.action.requested"))
            .unwrap()
            .pop()
            .expect("the seat was asked to restart");
        assert_eq!(restart.actor.as_deref(), Some(LIMITS_ACTOR));
        assert_eq!(restart.body["fields"]["action"], "restart");
        let notification = store
            .claims_for(&outcome.notified[0], Some("message.sent"))
            .unwrap()
            .pop()
            .expect("operations is notified about the stopped seat");
        assert_eq!(notification.body["fields"]["to"], "agent/alder.operations");
        assert!(
            notification.body["fields"]["title"]
                .as_str()
                .unwrap()
                .contains("1 affected seat")
        );

        // The restarted seat has not reported yet, so the old reading still names the account.
        // It does not move twice.
        let again = store
            .enforce_account_limits(&policy(), now + 60_000)
            .unwrap();
        assert!(
            again.switched.is_empty() && again.stopped.is_empty(),
            "{again:?}"
        );
    }

    #[test]
    fn a_pooled_seat_stops_when_every_account_in_its_pool_is_at_its_limit() {
        let store = Store::open_memory("alder").unwrap();
        declare_accounts(&store);
        let now = now_ms();
        read_account(
            &store,
            "agent/alder.pooled",
            "ada/one",
            "codex/aaaa",
            97.0,
            now - 60_000,
        );
        read_account(
            &store,
            "agent/alder.other",
            "ada/two",
            "codex/bbbb",
            99.0,
            now - 30_000,
        );

        let outcome = store.enforce_account_limits(&policy(), now).unwrap();

        assert!(outcome.switched.is_empty());
        let mut stopped = outcome.stopped.clone();
        stopped.sort();
        assert_eq!(
            stopped,
            ["agent/alder.other", "agent/alder.pooled"],
            "with nowhere to move, each pooled seat stops like a single-account seat"
        );
    }

    #[test]
    fn an_account_whose_weekly_window_has_reset_counts_as_unused_for_the_pool() {
        let store = Store::open_memory("alder").unwrap();
        declare_accounts(&store);
        let now = now_ms();
        read_account(
            &store,
            "agent/alder.pooled",
            "ada/one",
            "codex/aaaa",
            97.0,
            now - 60_000,
        );
        // A stale, high reading of the other account whose window ended before `now`.
        store
            .append_claim(&ClaimInput {
                subject: "agent/alder.other".into(),
                kind: "harness.limits".into(),
                actor: Some("agent/alder.other".into()),
                fields: BTreeMap::from([
                    ("driver".into(), json!("codex")),
                    ("account".into(), json!("codex/bbbb")),
                    ("account_ref".into(), json!("ada/two")),
                    ("weekly_percent".into(), json!(99.0)),
                    (
                        "weekly_resets_at_unix_ms".into(),
                        json!((now - 1000) as u64),
                    ),
                    ("measured_at_unix_ms".into(), json!((now - 2 * HOUR) as u64)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();

        let outcome = store.enforce_account_limits(&policy(), now).unwrap();

        assert_eq!(
            outcome.switched,
            [("agent/alder.pooled".to_owned(), "ada/two".to_owned())]
        );
    }

    #[test]
    fn a_reading_below_the_limit_or_too_old_stops_nothing() {
        let store = Store::open_memory("alder").unwrap();
        declare(&store, "agent/alder.seat", "alder");
        let now = now_ms();
        let policy = LimitsPolicy {
            stop_at_weekly_percent: 95,
            keep: BTreeSet::new(),
            notify: "agent/alder.operations".into(),
            fresh_ms: HOUR as u64,
        };
        read(&store, "agent/alder.seat", Some("codex/bbbb"), 94.9, now);
        assert_eq!(
            store.enforce_account_limits(&policy, now).unwrap(),
            LimitsOutcome::default()
        );
        read(
            &store,
            "agent/alder.seat",
            Some("codex/bbbb"),
            99.0,
            now - 2 * HOUR,
        );
        assert_eq!(
            store.enforce_account_limits(&policy, now).unwrap(),
            LimitsOutcome::default()
        );
        assert!(live(&store, "agent/alder.seat"));
    }
}
