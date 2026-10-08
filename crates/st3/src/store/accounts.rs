//! Declared model accounts, and the account each pooled seat on this node runs on.
//!
//! The declarations are shared graph state. Which of its owner's accounts a pooled seat runs on is
//! this node's own decision, because only the node that starts a seat has the logins, so it lives in
//! a local table that no shared projection reads.

use super::*;
use crate::accounts::{
    AccountDecl, Binding, Candidate, HarnessBinding, harness_binding, most_usage_left,
    parse_account,
};

/// The account a seat starts on, and the directory its login lives in on this host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeatAccount {
    pub account: AccountDecl,
    /// The login path as declared, before `~/` is expanded.
    pub login: String,
}

impl Store {
    /// Every declared account, in name order.
    pub fn declared_accounts(&self) -> Result<Vec<AccountDecl>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT subject, body FROM desired WHERE kind='account' ORDER BY subject",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut accounts = Vec::new();
        for row in rows {
            let (subject, body) = row?;
            if let Some(account) = serde_json::from_str(&body)
                .ok()
                .and_then(|desired: Value| parse_account(&subject, &desired))
            {
                accounts.push(account);
            }
        }
        Ok(accounts)
    }

    /// The account a pooled seat on this node was last placed on.
    pub fn seat_account_choice(&self, seat: &str) -> Result<Option<String>> {
        self.readers
            .get()
            .query_row(
                "SELECT account FROM local_seat_accounts WHERE seat=?1",
                [seat],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn choose_seat_account(&self, seat: &str, account: &str, now: u128) -> Result<()> {
        self.connection
            .batched(|tx| {
                tx.execute(
                    "INSERT INTO local_seat_accounts(seat, account, chosen_at_unix_ms) VALUES (?1, ?2, ?3)
                     ON CONFLICT(seat) DO UPDATE SET account=excluded.account,
                                                     chosen_at_unix_ms=excluded.chosen_at_unix_ms",
                    params![seat, account, now as i64],
                )
            })
            .map_err(|error| anyhow::anyhow!("{error}"))??;
        Ok(())
    }

    /// What each account in `accounts` has left, from the shared account limits selection. A weekly
    /// window that has reset since the reading counts as unused.
    fn candidates(&self, accounts: &[&AccountDecl], now: u128) -> Result<Vec<Candidate>> {
        let limits = self.account_limits()?;
        Ok(accounts
            .iter()
            .map(|account| {
                let reading = limits
                    .iter()
                    .filter(|limit| limit.account_ref.as_deref() == Some(account.name.as_str()))
                    .max_by_key(|limit| limit.measured_at_unix_ms);
                let current = |percent: Option<f64>, resets: Option<u64>| {
                    percent.filter(|_| resets.is_none_or(|at| u128::from(at) > now))
                };
                Candidate {
                    name: account.name.clone(),
                    weekly_percent: reading.and_then(|limit| {
                        current(limit.weekly_percent, limit.weekly_resets_at_unix_ms)
                    }),
                    five_hour_percent: reading.and_then(|limit| {
                        current(limit.five_hour_percent, limit.five_hour_resets_at_unix_ms)
                    }),
                }
            })
            .collect())
    }

    /// The accounts a bound seat could run on here: one named account, or every account its pool's
    /// owner declares for the harness whose login this host holds.
    fn bindable_accounts(
        accounts: &[AccountDecl],
        binding: &HarnessBinding,
        host: &str,
    ) -> Vec<AccountDecl> {
        accounts
            .iter()
            .filter(|account| {
                account.driver() == Some(binding.driver.as_str())
                    && account.login_for(host).is_some()
                    && match &binding.binding {
                        Binding::Account(name) => &account.name == name,
                        Binding::Pool(owner) => account.owner.as_ref() == Some(owner),
                    }
            })
            .cloned()
            .collect()
    }

    /// The account `seat` starts on at `host`, or why it cannot start. A single binding names its
    /// account. A pool keeps the account the seat already runs on, and otherwise takes the one
    /// with the most usage left.
    pub fn account_for_start(
        &self,
        seat: &str,
        binding: &HarnessBinding,
        host: &str,
        now: u128,
    ) -> Result<SeatAccount, String> {
        let accounts = self
            .declared_accounts()
            .map_err(|error| error.to_string())?;
        let bindable = Self::bindable_accounts(&accounts, binding, host);
        let account = match &binding.binding {
            Binding::Account(name) => bindable.into_iter().next().ok_or_else(|| match accounts
                .iter()
                .find(|account| &account.name == name)
            {
                None => {
                    format!("the seat is bound to account `{name}`, which no declaration defines")
                }
                Some(account) if account.driver() != Some(binding.driver.as_str()) => format!(
                    "account `{name}` belongs to provider `{}`, not the {} harness",
                    account.provider, binding.driver
                ),
                Some(_) => format!("account `{name}` has no login on host `{host}`"),
            })?,
            Binding::Pool(owner) => {
                let kept = self
                    .seat_account_choice(seat)
                    .map_err(|error| error.to_string())?;
                if let Some(account) = kept
                    .as_deref()
                    .and_then(|name| bindable.iter().find(|account| account.name == name))
                {
                    account.clone()
                } else {
                    // Until the limits projection has filled after an upgrade no account has a
                    // reading, so every account would look unused and the choice, which is kept,
                    // could pin the seat to an exhausted one. Choose and store nothing yet.
                    if !self
                        .account_limits_ready()
                        .map_err(|error| error.to_string())?
                    {
                        return Err("account limits are catching up, try again".to_owned());
                    }
                    let refs = bindable.iter().collect::<Vec<_>>();
                    let candidates = self
                        .candidates(&refs, now)
                        .map_err(|error| error.to_string())?;
                    let name = most_usage_left(&candidates)
                        .map(|candidate| candidate.name.clone())
                        .ok_or_else(|| {
                            format!(
                                "the seat's pool `{owner}` has no {} account with a login on host `{host}`",
                                binding.driver
                            )
                        })?;
                    self.choose_seat_account(seat, &name, now)
                        .map_err(|error| error.to_string())?;
                    bindable
                        .into_iter()
                        .find(|account| account.name == name)
                        .expect("the chosen account is bindable")
                }
            }
        };
        let login = account
            .login_for(host)
            .expect("a bindable account has a login here")
            .to_owned();
        Ok(SeatAccount { account, login })
    }

    /// The pool accounts a seat on `host` could move to when `leaving` runs out: the ones other than
    /// `leaving` whose weekly reading is under `limit_percent`, most usage left first.
    pub fn pool_alternatives(
        &self,
        binding: &HarnessBinding,
        host: &str,
        leaving: &str,
        limit_percent: f64,
        now: u128,
    ) -> Result<Vec<String>> {
        if !matches!(binding.binding, Binding::Pool(_)) || !self.account_limits_ready()? {
            // Not yet known is no alternative: it must not read as every account being unused.
            return Ok(Vec::new());
        }
        let accounts = self.declared_accounts()?;
        let bindable = Self::bindable_accounts(&accounts, binding, host);
        let refs = bindable
            .iter()
            .filter(|account| account.name != leaving)
            .collect::<Vec<_>>();
        let mut candidates = self
            .candidates(&refs, now)?
            .into_iter()
            .filter(|candidate| {
                candidate
                    .weekly_percent
                    .is_none_or(|used| used < limit_percent)
            })
            .collect::<Vec<_>>();
        let mut ordered = Vec::new();
        while let Some(best) = most_usage_left(&candidates).map(|best| best.name.clone()) {
            candidates.retain(|candidate| candidate.name != best);
            ordered.push(best);
        }
        Ok(ordered)
    }

    /// The seat's declared binding, when its current declaration is an agent that names one.
    pub fn seat_binding(&self, seat: &str) -> Result<Option<HarnessBinding>> {
        let row = self
            .readers
            .get()
            .query_row(
                "SELECT body FROM desired WHERE subject=?1 AND kind='agent'",
                [seat],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(row.and_then(|body| {
            serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|desired| harness_binding(&desired))
        }))
    }

    /// Commit the local choice, fenced restart intent and once-per-window marker together.
    pub(super) fn switch_seat_account(
        &self,
        seat: &str,
        limit: &AccountLimit,
        next: &str,
        episode: &str,
        now: u128,
    ) -> Result<bool, St3Error> {
        let episode_account = super::limits::episode_account(limit);
        self.connection.batched(|tx| -> Result<bool, St3Error> {
            let handled: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM local_limit_stops WHERE account=?1 AND episode=?2 AND seat=?3)",
                params![episode_account, episode, seat], |row| row.get(0),
            ).map_err(internal)?;
            if handled || !limit_binding_is_current(tx, &self.origin, seat, limit).map_err(internal)? {
                return Ok(false);
            }
            let Some(desired) = current_desired_row_tx(tx, seat).map_err(internal)? else { return Ok(false); };
            let Some(member) = desired.member.and_then(|text| serde_json::from_str::<crate::model::MemberSpec>(&text).ok()) else { return Ok(false); };
            if desired.kind != "agent" || member.host != self.origin { return Ok(false); }
            let incarnation = observation_at(tx, &self.origin, seat, "runtime.observed").map_err(internal)?
                .and_then(|claim| claim.body.pointer("/fields/incarnation_id").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_default();
            let input = ClaimInput {
                subject: seat.into(), kind: "runtime.action.requested".into(), actor: Some(LIMITS_ACTOR.into()),
                fields: BTreeMap::from([
                    ("action".into(), json!("restart")),
                    ("runtime_id".into(), json!(member.runtime_id)),
                    ("incarnation_id".into(), json!(incarnation)),
                ]),
                evidence: vec![desired.claim_id], expected_subject: None,
                idempotency_key: Some(format!("limits-switch:{episode_account}:{episode}:{seat}")),
            };
            let operation = claim_operation(&input)?;
            let mut body = json!({"fields": input.fields, "evidence": input.evidence});
            if let Some((id, digest)) = &operation {
                body["_operation"] = json!({"id": id, "request_digest": digest});
            }
            tx.execute(
                "INSERT INTO local_seat_accounts(seat,account,chosen_at_unix_ms) VALUES (?1,?2,?3)
                 ON CONFLICT(seat) DO UPDATE SET account=excluded.account, chosen_at_unix_ms=excluded.chosen_at_unix_ms",
                params![seat, next, now as i64],
            ).map_err(internal)?;
            let predecessors = latest_claim_id_tx(tx, seat).map_err(internal)?.into_iter().collect::<Vec<_>>();
            let claim = append_claim_tx(tx, &self.origin, seat, &input.kind, input.actor.as_deref(), &body, &predecessors, None)
                .map_err(claim_append_error)?;
            register_operation_tx(tx, &claim).map_err(internal)?;
            tx.execute(
                "INSERT INTO local_limit_stops(account,episode,seat,stopped_at_unix_ms) VALUES (?1,?2,?3,?4)",
                params![episode_account, episode, seat, now as i64],
            ).map_err(internal)?;
            Ok(true)
        }).map_err(internal)?
    }
}

/// The newest durable or local observation, within the caller's snapshot or write transaction.
fn observation_at(
    connection: &Connection,
    origin: &str,
    seat: &str,
    kind: &str,
) -> Result<Option<ClaimRecord>> {
    let claim = connection.query_row(
        &canonical_sql("SELECT claims.id, claims.store_index, claims.batch_id, claims.subject, claims.kind,
            claims.origin, claims.actor, claims.body, claims.predecessors, claims.accepted_at_unix_ms
            FROM claims WHERE subject=?1 AND kind=?2 ORDER BY CANONICAL_DESC(claims) LIMIT 1"),
        params![seat, kind], claim_from_row,
    ).optional()?;
    let local = connection
        .query_row(
            &format!(
                "{LOCAL_OBSERVATION_COLUMNS} WHERE subject=?1 AND kind=?2 ORDER BY id DESC LIMIT 1"
            ),
            params![seat, kind],
            |row| local_observation_from_row(origin, row),
        )
        .optional()?;
    Ok(claim.into_iter().chain(local).max_by_key(claim_log_order))
}

/// Quota belongs to an account, not an incarnation. A new binding cannot use the previous
/// login's reading; a relaunch on the same declared account can use its account's evidence.
pub(super) fn limit_binding_is_current(
    connection: &Connection,
    origin: &str,
    seat: &str,
    limit: &AccountLimit,
) -> Result<bool> {
    let desired: Option<(String, String)> = connection
        .query_row(
            "SELECT kind,body FROM desired WHERE subject=?1",
            [seat],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((kind, body)) = desired else {
        return Ok(false);
    };
    if kind != "agent" {
        return Ok(false);
    }
    let Some(binding) = harness_binding(&serde_json::from_str::<Value>(&body)?) else {
        return Ok(limit.account_ref.is_none());
    };
    let Some(leaving) = limit.account_ref.as_deref() else {
        return Ok(false);
    };
    if binding.driver != limit.driver {
        return Ok(false);
    }
    match &binding.binding {
        Binding::Account(name) => return Ok(name == leaving),
        Binding::Pool(owner) => {
            let choice: Option<String> = connection
                .query_row(
                    "SELECT account FROM local_seat_accounts WHERE seat=?1",
                    [seat],
                    |row| row.get(0),
                )
                .optional()?;
            if choice.as_ref().is_some_and(|chosen| chosen != leaving) {
                return Ok(false);
            }
            let account_subject = format!("account/{leaving}");
            let account: Option<String> = connection
                .query_row(
                    "SELECT body FROM desired WHERE subject=?1 AND kind='account'",
                    [&account_subject],
                    |row| row.get(0),
                )
                .optional()?;
            let account = account
                .and_then(|body| serde_json::from_str::<Value>(&body).ok())
                .and_then(|body| parse_account(&account_subject, &body));
            if account.is_none_or(|account| {
                account.owner.as_ref() != Some(owner)
                    || account.driver() != Some(binding.driver.as_str())
            }) {
                return Ok(false);
            }
            if choice.is_some() {
                // The durable local pool choice survives member restarts. Without a choice,
                // retain the legacy source/incarnation fence rather than guessing a login.
                return Ok(true);
            }
        }
    }
    let Some(source) = observation_at(connection, origin, seat, "harness.limits")? else {
        return Ok(false);
    };
    if source
        .body
        .pointer("/fields/account_ref")
        .and_then(Value::as_str)
        != Some(leaving)
    {
        return Ok(false);
    }
    if let Some(runtime) = observation_at(connection, origin, seat, "runtime.observed")? {
        let current = runtime
            .body
            .pointer("/fields/incarnation_id")
            .and_then(Value::as_str);
        let reported = source
            .body
            .pointer("/fields/incarnation_id")
            .and_then(Value::as_str);
        if current.is_some() && current != reported {
            return Ok(false);
        }
    }
    Ok(true)
}
