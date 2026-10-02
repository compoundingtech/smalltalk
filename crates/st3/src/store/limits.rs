//! Account limits: the freshest reading of each paying account's 5-hour and weekly limits, and
//! the policy that stops an account's seats as it nears its weekly limit.
//!
//! A harness reports its account's limits with every status refresh, so every seat on an account
//! repeats them, and an idle seat keeps its last reading. An account's limits are therefore the
//! reading measured most recently across all seats that last reported that account, labelled
//! with when and by which seat it was measured. Readings never cross accounts or providers: a
//! harness that names no account reads as `PROVIDER/unknown`.

use super::*;
use serde::Deserialize;

/// The actor st records for what its limits policy does.
pub const LIMITS_ACTOR: &str = "daemon/limits";

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AccountLimit {
    /// The account label, or `DRIVER/unknown` when the harness named none.
    pub account: String,
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
    pub person: String,
    pub fresh_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct LimitsOutcome {
    /// Seats this node stopped in this pass.
    pub stopped: Vec<String>,
    /// Person requests this pass made or found.
    pub asked: Vec<String>,
}

fn reading(origin: &str, body: &Value) -> Option<(AccountLimit, String)> {
    let fields = body.get("fields")?;
    let driver = fields["driver"].as_str()?.to_owned();
    let account = fields["account"]
        .as_str()
        .filter(|account| !account.is_empty())
        .map_or_else(|| format!("{driver}/unknown"), str::to_owned);
    let percent = |name: &str| fields[name].as_f64().filter(|value| value.is_finite());
    Some((
        AccountLimit {
            account: account.clone(),
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
    let mut newest = BTreeMap::<String, AccountLimit>::new();
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
            newest.insert(subject, limit);
        }
    }
    let mut accounts = BTreeMap::<String, AccountLimit>::new();
    // Seats in subject order, so the freshest reading of equal measurement times is the same on
    // every node.
    for (seat, limit) in newest {
        let entry = accounts
            .entry(limit.account.clone())
            .or_insert_with(|| limit.clone());
        if limit.measured_at_unix_ms > entry.measured_at_unix_ms {
            let seats = std::mem::take(&mut entry.seats);
            *entry = AccountLimit { seats, ..limit };
        }
        entry.seats.push(seat);
    }
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
    /// The freshest limits reading of every account, in account order.
    pub fn account_limits(&self) -> Result<Vec<AccountLimit>> {
        account_limits_at(&self.readers.get())
    }

    /// Stop the seats this node hosts on every account whose fresh weekly reading reached the
    /// policy's percentage, and ask the policy's person once per weekly window. Each seat is
    /// stopped at most once per window, so a person can start it again.
    pub fn enforce_account_limits(
        &self,
        policy: &LimitsPolicy,
        now: u128,
    ) -> Result<LimitsOutcome, St3Error> {
        let mut outcome = LimitsOutcome::default();
        for limit in self.account_limits().map_err(internal)? {
            let Some(weekly) = limit.weekly_percent else {
                continue;
            };
            if weekly < f64::from(policy.stop_at_weekly_percent)
                || now.saturating_sub(u128::from(limit.measured_at_unix_ms))
                    > u128::from(policy.fresh_ms)
            {
                continue;
            }
            // One episode per weekly window. Without a reported reset, a UTC day stands in.
            let episode = limit.weekly_resets_at_unix_ms.map_or_else(
                || format!("day-{}", limit.measured_at_unix_ms / 86_400_000),
                |at| at.to_string(),
            );
            let mut targets = Vec::new();
            let mut local = Vec::new();
            {
                let connection = self.readers.get();
                for seat in &limit.seats {
                    if policy.keep.contains(seat) {
                        continue;
                    }
                    let Some(host) = seat_host(&connection, seat).map_err(internal)? else {
                        continue;
                    };
                    targets.push(seat.clone());
                    let stopped_before: bool = connection
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM local_limit_stops
                             WHERE account=?1 AND episode=?2 AND seat=?3)",
                            params![limit.account, episode, seat],
                            |row| row.get(0),
                        )
                        .map_err(internal)?;
                    if host == self.origin && !stopped_before {
                        local.push(seat.clone());
                    }
                }
            }
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
                    &format!("limits-stop:{}:{episode}:{seat}", limit.account),
                    Some(LIMITS_ACTOR),
                )?;
                self.connection
                    .batched(|tx| {
                        tx.execute(
                            "INSERT OR IGNORE INTO local_limit_stops(account, episode, seat, stopped_at_unix_ms)
                             VALUES (?1, ?2, ?3, ?4)",
                            params![limit.account, episode, seat, now as i64],
                        )
                    })
                    .map_err(internal)?
                    .map_err(internal)?;
                outcome.stopped.push(seat);
            }
            // The node that measured the reading asks, so one node writes the request.
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
                "{provider} account {} is at {weekly:.0}% of its weekly limit: {} seat{plural} stopped",
                limit.account,
                targets.len()
            );
            let reason = format!(
                "{} reached {weekly:.0}% of its weekly limit ({} measured it at {}), so st stopped \
                 every seat that uses it{kept}. Stopped: {}. {reset} Nothing is needed from you \
                 unless you want a seat back sooner: start it again and st leaves it running until \
                 the reset.",
                limit.account,
                limit.measured_by,
                utc(limit.measured_at_unix_ms),
                targets.join(", ")
            );
            let ask = self.ask_person_as_daemon(
                LIMITS_ACTOR,
                &policy.person,
                &title,
                &reason,
                &format!("limits/{}", limit.account),
                &episode,
            )?;
            outcome.asked.push(ask.subject);
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
    fn an_account_past_its_weekly_limit_stops_its_local_seats_once_and_asks_once() {
        let store = Store::open_memory("alder").unwrap();
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
            person: "person/avery".into(),
            fresh_ms: HOUR as u64,
        };
        let outcome = store.enforce_account_limits(&policy, now).unwrap();
        assert_eq!(
            outcome.stopped,
            [busy, idle],
            "this node's seats, never the kept one"
        );
        assert_eq!(outcome.asked.len(), 1);
        assert!(!live(&store, busy) && !live(&store, idle));
        assert!(live(&store, coordinator) && live(&store, remote));
        assert!(live(&store, other), "a stale reading is not acted on");
        let ask = store
            .attention_items(Some("person/avery"))
            .unwrap()
            .into_iter()
            .find(|item| item.subject == outcome.asked[0])
            .expect("the request is on the person's home");
        assert!(
            ask.title
                .contains("claude/aaaa is at 96% of its weekly limit: 3 seats stopped"),
            "{}",
            ask.title
        );

        // A person starts a seat again: it stays up until the window resets, and the request
        // stays the one request.
        declare(&store, busy, "alder");
        let again = store.enforce_account_limits(&policy, now + 60_000).unwrap();
        assert!(again.stopped.is_empty(), "{again:?}");
        assert!(live(&store, busy));
        assert!(
            again
                .asked
                .iter()
                .all(|subject| subject == &outcome.asked[0])
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
            person: "person/avery".into(),
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
