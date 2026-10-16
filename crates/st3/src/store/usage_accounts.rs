//! Usage and limits read against the accounts a person declared.
//!
//! A response is recorded under the label of the account that paid for it. Seats launched on a
//! declared account carry that account's stable label; older responses, and seats that were never
//! launched on one, carry the provider's own label or none. A declaration claims its provider
//! labels with `identity`, and a harness that has exactly one declared account on a host
//! (Codex, the key file omp reads) owns what its seats recorded there without a label. Each such
//! row is read as that account. A row no declaration claims keeps what it carried: nothing is
//! hidden to make a panel look right.

use super::accounts::declared_accounts_at;
use super::limits::AccountLimit;
use super::*;
use crate::accounts::{AccountDecl, harness_driver};

/// One usage read, every row and limit against the declared accounts.
pub struct AttributedUsage {
    pub rows: Vec<Value>,
    pub agent_messages: Value,
    pub limits: Vec<AccountLimit>,
    /// Every declared account, in name order, with its credits when it has a budget.
    pub accounts: Vec<Value>,
}

/// The start of the UTC calendar month containing `at_ms`.
fn month_start_ms(at_ms: u64) -> u64 {
    use chrono::{Datelike, TimeZone};
    let at = chrono::Utc
        .timestamp_millis_opt(at_ms as i64)
        .single()
        .unwrap_or_default();
    chrono::Utc
        .with_ymd_and_hms(at.year(), at.month(), 1, 0, 0, 0)
        .single()
        .map_or(0, |start| start.timestamp_millis() as u64)
}

struct Attribution<'a> {
    accounts: &'a [AccountDecl],
    claimed: BTreeMap<String, &'a AccountDecl>,
    seat_drivers: BTreeMap<String, Option<String>>,
}

impl<'a> Attribution<'a> {
    fn new(accounts: &'a [AccountDecl]) -> Self {
        let mut claimed = BTreeMap::new();
        for account in accounts {
            if let Some(label) = account.label() {
                claimed.insert(label, account);
            }
            for identity in &account.identities {
                claimed.entry(identity.clone()).or_insert(account);
            }
        }
        Self {
            accounts,
            claimed,
            seat_drivers: BTreeMap::new(),
        }
    }

    /// The harness a seat is declared with, when it still is.
    fn seat_driver(&mut self, connection: &Connection, seat: &str) -> Option<String> {
        self.seat_drivers
            .entry(seat.to_owned())
            .or_insert_with(|| {
                let body: Option<String> = connection
                    .query_row("SELECT body FROM desired WHERE subject=?1", [seat], |row| {
                        row.get(0)
                    })
                    .optional()
                    .ok()
                    .flatten();
                let desired = serde_json::from_str::<Value>(&body?).ok()?;
                harness_driver(&desired).map(str::to_owned)
            })
            .clone()
    }

    fn account_for(&mut self, connection: &Connection, row: &Value) -> Option<&'a AccountDecl> {
        let label = row["account"].as_str().unwrap_or("");
        if let Some(account) = self.claimed.get(label) {
            return Some(account);
        }
        if !label.is_empty() {
            return None;
        }
        let driver = self.seat_driver(connection, row["agent"].as_str()?)?;
        let host = row["host"].as_str().filter(|host| !host.is_empty())?;
        let mut owners = self.accounts.iter().filter(|account| {
            account.driver() == Some(driver.as_str()) && account.login_for(host).is_some()
        });
        let account = owners.next()?;
        owners.next().is_none().then_some(account)
    }

    fn attribute(&mut self, connection: &Connection, rows: &mut [Value]) {
        for row in rows {
            let Some(account) = self.account_for(connection, row) else {
                continue;
            };
            let Some(label) = account.label() else {
                continue;
            };
            if let Some(fields) = row.as_object_mut() {
                fields.insert("account".into(), Value::String(label));
                fields.insert("account_ref".into(), Value::String(account.name.clone()));
            }
        }
    }
}

impl Store {
    /// The usage of a period against the declared accounts: every row and limit, and each
    /// account's credits used so far in the calendar month of `until_ms`.
    pub fn attributed_usage(&self, since_ms: u64, until_ms: u64) -> Result<AttributedUsage> {
        let (mut rows, agent_messages) = self.usage_period_report(since_ms, until_ms)?;
        let connection = self.readers.get();
        let declared = declared_accounts_at(&connection)?;
        let mut attribution = Attribution::new(&declared);
        attribution.attribute(&connection, &mut rows);
        let limits = self.account_limits()?;

        let month_start = month_start_ms(until_ms);
        let mut month_rows = if declared.iter().any(|account| account.monthly_credits_usd.is_some())
        {
            let (mut rows, _) = self.usage_period_data(month_start, until_ms)?;
            attribution.attribute(&connection, &mut rows);
            rows
        } else {
            Vec::new()
        };
        let accounts = declared
            .iter()
            .map(|account| {
                let label = account.label();
                let mut value = json!({
                    "name": account.name,
                    "account": label,
                    "driver": account.driver(),
                    "provider": account.provider,
                    "kind": if account.is_api_key() { "api-key" } else { "subscription" },
                    "plan": account.plan,
                });
                if let Some(budget) = account.monthly_credits_usd {
                    let (mut cost, mut unpriced) = (0_u64, 0_u64);
                    for row in month_rows
                        .iter_mut()
                        .filter(|row| row["account_ref"].as_str() == Some(account.name.as_str()))
                    {
                        cost = cost.saturating_add(row["cost_microusd"].as_u64().unwrap_or(0));
                        unpriced =
                            unpriced.saturating_add(row["unpriced_tokens"].as_u64().unwrap_or(0));
                    }
                    value["credits"] = json!({
                        "budget_microusd": budget.saturating_mul(1_000_000),
                        "used_microusd": cost,
                        "unpriced_tokens": unpriced,
                        "period_start_ms": month_start,
                    });
                }
                if let Some(fields) = value.as_object_mut() {
                    fields.retain(|_, value| !value.is_null());
                }
                value
            })
            .collect();
        Ok(AttributedUsage {
            rows,
            agent_messages,
            limits,
            accounts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DECLARATIONS: &str = r#"version 2
account "ada/claude-1" { provider "anthropic"; owner "person/ada"; identity "claude/1111111111111111"; login "/srv/logins/one" host="alder"; }
account "ada/claude-2" { provider "anthropic"; owner "person/ada"; login "/srv/logins/two" host="alder"; }
account "ada/codex" { provider "openai"; owner "person/ada"; identity "codex/2222222222222222"; login "~/.codex" host="alder"; }
account "ada/key" { provider "anthropic"; owner "person/ada"; auth-type "api-key"; harness "omp"; monthly-credits-usd 200; login "~/.config/anthropic/api-key" host="alder"; }
agent "coder" { workspace "/tmp"; harness "codex" {}; }
agent "keyed" { workspace "/tmp"; harness "omp" {}; }
"#;

    fn spend(store: &Store, seat: &str, driver: &str, account: Option<&str>, model: &str, id: &str) {
        let mut fields = serde_json::from_value::<BTreeMap<String, Value>>(json!({
            "operation": "append", "entry_id": id, "source_id": id, "sequence": 1, "revision": 1,
            "role": "system", "entry_type": "usage", "final": true, "driver": driver,
            "incarnation_id": "inc-one", "observed_at_unix_ms": now_ms() as u64,
            "body": {"semantics": "response", "model": model, "input_tokens": 1_000_000, "output_tokens": 1000, "total_tokens": 1_001_000}
        }))
        .unwrap();
        if let Some(account) = account {
            fields.get_mut("body").unwrap()["account"] = json!(account);
        }
        let claim = ClaimInput {
            subject: seat.into(),
            kind: "harness.timeline".into(),
            actor: Some(seat.into()),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        };
        let observation = store.append_claim(&claim).unwrap();
        let rollup = store.usage_rollup_for_timeline(&observation).unwrap().unwrap();
        store.append_claim(&rollup).unwrap();
    }

    #[test]
    fn every_row_is_read_against_the_account_that_declares_it_and_nothing_else_is_hidden() {
        let store = Store::open_memory("alder").unwrap();
        let intent = crate::graph::parse_internal_intent(DECLARATIONS, "alder").unwrap();
        store.apply_internal(&intent, "declare-accounts").unwrap();
        let declared = store.declared_accounts().unwrap();
        let label = |name: &str| declared.iter().find(|a| a.name == name).unwrap().label().unwrap();

        // Before the account was declared: the provider's own label.
        spend(&store, "agent/alder.old-claude", "claude", Some("claude/1111111111111111"), "claude-sonnet-5-5", "r1");
        // Launched on the account: its stable label.
        spend(&store, "agent/alder.new-claude", "claude", Some(&label("ada/claude-2")), "claude-sonnet-5-5", "r2");
        // A harness that named no account, with exactly one declared account on this host.
        spend(&store, "agent/alder.coder", "codex", None, "claude-sonnet-5-5", "r3");
        spend(&store, "agent/alder.keyed", "omp", None, "claude-sonnet-5-5", "r4");
        // A label no declaration claims stays as recorded.
        spend(&store, "agent/alder.stranger", "claude", Some("claude/ffffffffffffffff"), "claude-sonnet-5-5", "r5");

        let usage = store.attributed_usage(0, now_ms() as u64 + 1).unwrap();
        let account_of = |agent: &str| {
            let row = usage.rows.iter().find(|row| row["agent"] == agent).unwrap();
            (row["account"].as_str().unwrap().to_owned(), row["account_ref"].as_str().map(str::to_owned))
        };
        assert_eq!(account_of("agent/alder.old-claude"), (label("ada/claude-1"), Some("ada/claude-1".into())));
        assert_eq!(account_of("agent/alder.new-claude"), (label("ada/claude-2"), Some("ada/claude-2".into())));
        assert_eq!(account_of("agent/alder.coder"), (label("ada/codex"), Some("ada/codex".into())));
        assert_eq!(account_of("agent/alder.keyed"), (label("ada/key"), Some("ada/key".into())));
        assert_eq!(account_of("agent/alder.stranger"), ("claude/ffffffffffffffff".into(), None));

        let names = usage.accounts.iter().map(|a| a["name"].as_str().unwrap()).collect::<Vec<_>>();
        assert_eq!(names, ["ada/claude-1", "ada/claude-2", "ada/codex", "ada/key"]);
        let key = usage.accounts.iter().find(|a| a["name"] == "ada/key").unwrap();
        assert_eq!(key["kind"], "api-key");
        assert_eq!(key["credits"]["budget_microusd"], 200_000_000);
        assert!(key["credits"]["used_microusd"].as_u64().unwrap() > 0, "{key}");
        assert!(usage.accounts.iter().filter(|a| a["name"] != "ada/key").all(|a| a.get("credits").is_none()));
    }

    #[test]
    fn a_harness_with_two_accounts_on_a_host_leaves_unlabelled_rows_unclaimed() {
        let store = Store::open_memory("alder").unwrap();
        let intent = crate::graph::parse_internal_intent(
            r#"version 2
account "ada/one" { provider "openai"; login "~/.codex" host="alder"; }
account "ada/two" { provider "openai"; login "~/.codex-two" host="alder"; }
agent "coder" { workspace "/tmp"; harness "codex" {}; }
"#,
            "alder",
        )
        .unwrap();
        store.apply_internal(&intent, "declare-accounts").unwrap();
        spend(&store, "agent/alder.coder", "codex", None, "claude-sonnet-5-5", "r1");
        let usage = store.attributed_usage(0, now_ms() as u64 + 1).unwrap();
        assert_eq!(usage.rows[0]["account"], "");
        assert!(usage.rows[0].get("account_ref").is_none());
    }
}
