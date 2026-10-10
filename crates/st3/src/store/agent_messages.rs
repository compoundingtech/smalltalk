//! A daily planning estimate. Message subjects contribute once; receipts never contribute.
//! Only explicit probe/test metadata is excluded. No turn attribution or content classifier.
use super::*;
use serde::Deserialize;

pub(super) const DAY_MS: u64 = 86_400_000;
pub(super) struct DailyUsage {
    pub since: u64,
    pub until: u64,
    pub cost: u64,
    pub unpriced: u64,
}
const ALLOWANCES: &str = "doc/usage/agent-message-allowances";
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS agent_message_sends (
    subject TEXT PRIMARY KEY,
    sent_ms INTEGER NOT NULL,
    recipient TEXT NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS agent_message_sends_time ON agent_message_sends(sent_ms,recipient);
CREATE TABLE IF NOT EXISTS agent_message_days (
    day_ms INTEGER NOT NULL,
    recipient TEXT NOT NULL,
    count INTEGER NOT NULL CHECK(count >= 0),
    PRIMARY KEY(day_ms,recipient)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_message_pending (subject TEXT PRIMARY KEY) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS agent_message_claim_insert AFTER INSERT ON claims
WHEN NEW.kind='message.sent' BEGIN
    INSERT OR IGNORE INTO local_agent_message_pending VALUES(NEW.subject);
END;
CREATE TRIGGER IF NOT EXISTS agent_message_claim_delete AFTER DELETE ON claims
WHEN OLD.kind='message.sent' BEGIN
    INSERT OR IGNORE INTO local_agent_message_pending VALUES(OLD.subject);
END;
CREATE TRIGGER IF NOT EXISTS agent_message_claim_update
AFTER UPDATE OF subject,kind,body,accepted_at_unix_ms ON claims
WHEN OLD.kind='message.sent' OR NEW.kind='message.sent' BEGIN
    INSERT OR IGNORE INTO local_agent_message_pending VALUES(OLD.subject);
    INSERT OR IGNORE INTO local_agent_message_pending VALUES(NEW.subject);
END;
CREATE TRIGGER IF NOT EXISTS agent_message_send_insert AFTER INSERT ON agent_message_sends BEGIN
    INSERT INTO agent_message_days VALUES((NEW.sent_ms / 86400000)*86400000,NEW.recipient,1)
    ON CONFLICT(day_ms,recipient) DO UPDATE SET count=count+1;
END;
CREATE TRIGGER IF NOT EXISTS agent_message_send_delete AFTER DELETE ON agent_message_sends BEGIN
    UPDATE agent_message_days SET count=count-1
    WHERE day_ms=(OLD.sent_ms / 86400000)*86400000 AND recipient=OLD.recipient;
    DELETE FROM agent_message_days
    WHERE day_ms=(OLD.sent_ms / 86400000)*86400000 AND recipient=OLD.recipient AND count=0;
END;
"#;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    Ok(())
}

pub(super) fn open(transaction: &Transaction<'_>) -> Result<()> {
    let filled: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key='agent_message_days_v1')",
        [],
        |row| row.get(0),
    )?;
    if !filled {
        transaction.execute_batch(
            "DELETE FROM agent_message_sends;
             DELETE FROM agent_message_days;
             INSERT OR IGNORE INTO local_agent_message_pending
             SELECT DISTINCT subject FROM claims WHERE kind='message.sent';",
        )?;
    }
    flush(transaction)?;
    if !filled {
        transaction.execute(
            "INSERT INTO meta(key,value) VALUES('agent_message_days_v1','1')",
            [],
        )?;
    }
    Ok(())
}

fn eligible(fields: &Value) -> bool {
    let from = fields["from"].as_str().unwrap_or_default();
    let to = fields["to"].as_str().unwrap_or_default();
    if !from.starts_with("agent/") || !to.starts_with("agent/") {
        return false;
    }
    if [from, to]
        .iter()
        .any(|seat| seat.contains("delivery-probe/") || seat.contains("delivery-soak"))
    {
        return false;
    }
    if fields["tags"].as_array().is_some_and(|tags| {
        tags.iter().any(|tag| {
            matches!(
                tag.as_str(),
                Some(
                    "delivery-probe"
                        | "delivery-test"
                        | "channel-check"
                        | "test"
                        | "soak"
                        | "canary"
                )
            )
        })
    }) {
        return false;
    }
    let title = fields["title"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut title = title.trim();
    while let Some(rest) = title.strip_prefix("re:") {
        title = rest.trim_start();
    }
    ![
        "soak request",
        "soak reply",
        "channel check",
        "delivery test",
        "delivery probe",
        "canary reply",
    ]
    .iter()
    .any(|prefix| {
        title.strip_prefix(prefix).is_some_and(|rest| {
            rest.is_empty() || rest.starts_with(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        })
    })
}

pub(super) fn flush(transaction: &Transaction<'_>) -> Result<()> {
    let pending = transaction
        .prepare_cached("SELECT subject FROM local_agent_message_pending")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for subject in pending {
        // Earliest send time is stable across replication order and duplicate send claims.
        let sent: Option<(u64, String)> = transaction
            .query_row(
                "SELECT CAST(accepted_at_unix_ms AS INTEGER),body FROM claims
             WHERE subject=?1 AND kind='message.sent'
             ORDER BY CAST(accepted_at_unix_ms AS INTEGER),id LIMIT 1",
                [&subject],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if sent.is_none() {
            coordination::sync(transaction, &subject, None)?;
        }
        let desired = sent
            .map(|(at, body)| -> Result<_> {
                let body: Value = serde_json::from_str(&body)?;
                coordination::sync(transaction, &subject, Some((at, &body["fields"])))?;
                Ok(eligible(&body["fields"])
                    .then(|| (at, body["fields"]["to"].as_str().unwrap().to_owned())))
            })
            .transpose()?
            .flatten();
        let current: Option<(u64, String)> = transaction
            .query_row(
                "SELECT sent_ms,recipient FROM agent_message_sends WHERE subject=?1",
                [&subject],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if current != desired {
            transaction.execute(
                "DELETE FROM agent_message_sends WHERE subject=?1",
                [&subject],
            )?;
            if let Some((at, recipient)) = desired {
                transaction.execute(
                    "INSERT INTO agent_message_sends VALUES(?1,?2,?3)",
                    params![subject, at, recipient],
                )?;
            }
        }
        transaction.execute(
            "DELETE FROM local_agent_message_pending WHERE subject=?1",
            [&subject],
        )?;
    }
    coordination::backfill(transaction)?;
    Ok(())
}

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct Allowance {
    low_microusd: u64,
    high_microusd: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Calibration {
    source: String,
    method: String,
    recipients: BTreeMap<String, Allowance>,
}

/// At most the last 31 UTC days, clipped to the requested period. Zero-message days stay.
pub(super) fn windows(since: u64, until: u64) -> Vec<(u64, u64)> {
    if since >= until {
        return Vec::new();
    }
    let last = (until - 1) / DAY_MS * DAY_MS;
    let first = (since / DAY_MS * DAY_MS).max(last.saturating_sub(30 * DAY_MS));
    (first..=last)
        .step_by(DAY_MS as usize)
        .map(|day| (day.max(since), day.saturating_add(DAY_MS).min(until)))
        .collect()
}

impl Store {
    pub(super) fn agent_message_estimate(&self, days: &[DailyUsage]) -> Result<Value> {
        let connection = self.readers.get();
        let calibration: Option<(String, Vec<u8>)> = connection
            .query_row(
                "SELECT d.hash,b.bytes FROM documents d JOIN blobs b ON b.hash=d.hash
             WHERE d.name=?1 ORDER BY d.binding_key DESC LIMIT 1",
                [ALLOWANCES],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        // An absent or invalid calibration must not break spend reads or imply zero cost.
        let calibration = calibration.and_then(|(hash, bytes)| {
            let value: Calibration = serde_json::from_slice(&bytes).ok()?;
            if value.source.is_empty()
                || value.method.is_empty()
                || value.recipients.len() > 4096
                || value.recipients.iter().any(|(seat, a)| {
                    !seat.starts_with("agent/") || a.low_microusd > a.high_microusd
                })
            {
                return None;
            }
            Some((format!("{ALLOWANCES}@{hash}"), value))
        });
        let fallback = Allowance {
            low_microusd: 220_000,
            high_microusd: 330_000,
        };
        let mut result = Vec::new();
        for &DailyUsage {
            since,
            until,
            cost,
            unpriced,
        } in days
        {
            let day = since / DAY_MS * DAY_MS;
            let counts = if since == day && until == day.saturating_add(DAY_MS) {
                connection
                    .prepare_cached(
                        "SELECT recipient,count FROM agent_message_days WHERE day_ms=?1",
                    )?
                    .query_map([day], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            } else {
                // Only the two clipped edge days need the indexed timestamp range.
                connection
                    .prepare_cached(
                        "SELECT recipient,COUNT(*) FROM agent_message_sends
                     WHERE sent_ms>=?1 AND sent_ms<?2 GROUP BY recipient",
                    )?
                    .query_map(params![since, until], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            let (mut messages, mut calibrated, mut low, mut high) = (0_u64, 0_u64, 0_u64, 0_u64);
            for (recipient, count) in counts {
                let allowance = calibration
                    .as_ref()
                    .and_then(|(_, c)| c.recipients.get(&recipient));
                if allowance.is_some() {
                    calibrated = calibrated.saturating_add(count);
                }
                let allowance = allowance.copied().unwrap_or(fallback);
                messages = messages.saturating_add(count);
                low = low.saturating_add(count.saturating_mul(allowance.low_microusd));
                high = high.saturating_add(count.saturating_mul(allowance.high_microusd));
            }
            result.push(json!({
                "day_start_ms": day, "since_ms": since, "until_ms": until,
                "messages": messages, "calibrated_messages": calibrated,
                "low_microusd": low, "high_microusd": high,
                "usage_cost_microusd": cost, "unpriced_tokens": unpriced,
                "low_percent": (cost > 0).then(|| low as f64 * 100.0 / cost as f64),
                "high_percent": (cost > 0).then(|| high as f64 * 100.0 / cost as f64),
            }));
        }
        Ok(json!({
            "calibration": calibration.as_ref().map(|(reference, _)| reference.as_str()),
            "source": calibration.as_ref().map_or("fleet fallback", |(_, c)| c.source.as_str()),
            "method": calibration.as_ref().map_or("Count times fleet allowance", |(_, c)| c.method.as_str()),
            "fallback_low_microusd": fallback.low_microusd,
            "fallback_high_microusd": fallback.high_microusd,
            "days": result,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn estimate_days(store: &Store, days: &[(u64, u64, u64, u64)]) -> Result<Value> {
        let days = days
            .iter()
            .map(|&(since, until, cost, unpriced)| DailyUsage {
                since,
                until,
                cost,
                unpriced,
            })
            .collect::<Vec<_>>();
        store.agent_message_estimate(&days)
    }

    fn send(store: &Store, subject: &str, at: u64, fields: Value) {
        store
            .connection
            .batched(|tx| {
                let claim = append_claim_tx(
                    tx,
                    "node",
                    subject,
                    "message.sent",
                    None,
                    &json!({"fields": fields}),
                    &[],
                    None,
                )?;
                tx.execute(
                    "UPDATE claims SET accepted_at_unix_ms=?1 WHERE id=?2",
                    params![at.to_string(), claim.id],
                )?;
                flush(tx)
            })
            .unwrap()
            .unwrap();
    }

    #[test]
    fn counts_distinct_sends_excludes_explicit_tests_and_rebuilds() {
        let store = Store::open_memory("node").unwrap();
        let fields =
            json!({"from":"agent/alder", "to":"agent/birch", "status":"sent", "title":"Update"});
        send(&store, "message/one", DAY_MS + 10, fields.clone());
        // A legacy/replicated second send claim for the same subject cannot double count.
        store.connection.batched(|tx| {
            append_claim_record_tx(tx, "node", "message/one", "message.sent", None,
                &json!({"fields": {"from":"agent/alder", "to":"agent/birch", "status":"sent", "title":"Retry"}}), &[], None)?;
            append_claim_record_tx(tx, "node", "message/one", "message.delivered", None,
                &json!({"fields":{"status":"delivered"}}), &[], None)?;
            flush(tx)
        }).unwrap().unwrap();
        send(&store, "message/two", 2 * DAY_MS, fields.clone());
        for (i, (from, to, title, tags)) in [
            ("person/ada", "agent/birch", "Question", json!([])),
            ("daemon/runtime", "agent/birch", "Ready", json!([])),
            ("agent/alder", "person/ada", "Answer", json!([])),
            (
                "agent/alder",
                "agent/birch",
                "Re: Re: Channel check",
                json!([]),
            ),
            (
                "agent/alder",
                "agent/birch",
                "Hello",
                json!(["delivery-probe"]),
            ),
            (
                "agent/delivery-soak/worker",
                "agent/birch",
                "Hello",
                json!([]),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            send(
                &store,
                &format!("message/exclude-{i}"),
                DAY_MS + 100,
                json!({"from":from,"to":to,"title":title,"tags":tags,"status":"sent"}),
            );
        }
        let daily = [
            (DAY_MS, 2 * DAY_MS, 1_000_000, 0),
            (2 * DAY_MS, 3 * DAY_MS, 0, 0),
        ];
        let estimate = estimate_days(&store, &daily).unwrap();
        assert_eq!(estimate["days"][0]["messages"], 1);
        assert_eq!(estimate["days"][1]["messages"], 1);
        assert_eq!(estimate["days"][0]["low_percent"], 22.0);
        assert!(estimate["days"][1]["low_percent"].is_null());
        assert_eq!(
            estimate_days(&store, &[(DAY_MS + 11, 2 * DAY_MS, 1, 0)]).unwrap()["days"][0]["messages"],
            0
        );
        store
            .connection
            .batched(|tx| {
                tx.execute("DELETE FROM meta WHERE key='agent_message_days_v1'", [])?;
                open(tx)
            })
            .unwrap()
            .unwrap();
        assert_eq!(estimate_days(&store, &daily).unwrap(), estimate);
        store
            .connection
            .batched(|tx| {
                tx.execute("DELETE FROM claims WHERE subject='message/one'", [])?;
                flush(tx)
            })
            .unwrap()
            .unwrap();
        assert_eq!(
            estimate_days(&store, &daily).unwrap()["days"][0]["messages"],
            0
        );
        let plan: String = store
            .readers
            .get()
            .query_row(
                "EXPLAIN QUERY PLAN SELECT recipient,count FROM agent_message_days WHERE day_ms=1",
                [],
                |r| r.get(3),
            )
            .unwrap();
        assert!(plan.contains("PRIMARY KEY"), "{plan}");
    }

    #[test]
    fn receiving_seat_calibration_and_fallback_use_the_same_daily_denominator() {
        let store = Store::open_memory("node").unwrap();
        let binding = store
            .put_document(
                ALLOWANCES,
                serde_json::to_vec(&json!({
                    "source":"dated audit", "method":"associated budgets / messages",
                    "recipients":{"agent/birch":{"low_microusd":100_000,"high_microusd":200_000}}
                }))
                .unwrap()
                .as_slice(),
                &None,
                "allowances",
            )
            .unwrap();
        for (subject, to) in [("message/a", "agent/birch"), ("message/b", "agent/cedar")] {
            send(
                &store,
                subject,
                DAY_MS + 1,
                json!({"from":"agent/alder","to":to,"title":"Update","status":"sent"}),
            );
        }
        let estimate = estimate_days(&store, &[(DAY_MS, 2 * DAY_MS, 2_000_000, 999)]).unwrap();
        let day = &estimate["days"][0];
        assert_eq!(day["messages"], 2);
        assert_eq!(day["calibrated_messages"], 1);
        assert_eq!(day["low_microusd"], 320_000);
        assert_eq!(day["high_microusd"], 530_000);
        assert_eq!(day["low_percent"], 16.0);
        assert_eq!(day["unpriced_tokens"], 999);
        assert!(
            estimate["calibration"]
                .as_str()
                .unwrap()
                .starts_with(ALLOWANCES)
        );
        store
            .put_document(
                ALLOWANCES,
                b"{}",
                &Some(binding.binding_claim_id),
                "invalid-allowances",
            )
            .unwrap();
        let fallback = estimate_days(&store, &[(DAY_MS, 2 * DAY_MS, 1_000_000, 0)]).unwrap();
        assert_eq!(fallback["days"][0]["calibrated_messages"], 0);
        assert_eq!(fallback["days"][0]["low_microusd"], 440_000);
    }

    #[test]
    fn daily_usage_matches_separate_periods_including_baselines_and_restarts() {
        let store = Store::open_memory("node").unwrap();
        for (i, (at, tokens, cost, unpriced)) in [
            (DAY_MS, 100, 100_000, 0),
            (DAY_MS + 1, 120, 150_000, 0),
            (2 * DAY_MS, 200, 250_000, 10),
            (2 * DAY_MS + 1, 10, 30_000, 0),
            (3 * DAY_MS, 50, 80_000, 0),
        ]
        .into_iter()
        .enumerate()
        {
            store
                .append_claim(&ClaimInput {
                    subject: "agent/alder".into(),
                    kind: "harness.usage".into(),
                    actor: None,
                    fields: serde_json::from_value(json!({"driver":"codex","incarnation_id":"one",
                    "semantics":"response_rollup","observed_at_unix_ms":at,"total_tokens":tokens,
                    "cost_microusd":cost,"unpriced_tokens":unpriced,"model":"test-model"}))
                    .unwrap(),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: Some(format!("usage-{i}")),
                })
                .unwrap();
        }
        let (_, estimate) = store.usage_period_report(DAY_MS, 3 * DAY_MS).unwrap();
        for day in estimate["days"].as_array().unwrap() {
            let rows = store
                .usage_period_rows(
                    day["since_ms"].as_u64().unwrap(),
                    day["until_ms"].as_u64().unwrap(),
                )
                .unwrap();
            assert_eq!(
                day["usage_cost_microusd"].as_u64().unwrap(),
                rows.iter()
                    .map(|r| r["cost_microusd"].as_u64().unwrap())
                    .sum::<u64>()
            );
            assert_eq!(
                day["unpriced_tokens"].as_u64().unwrap(),
                rows.iter()
                    .map(|r| r["unpriced_tokens"].as_u64().unwrap())
                    .sum::<u64>()
            );
        }
        assert!(windows(1, 1).is_empty());
        assert_eq!(windows(0, 100 * DAY_MS).len(), 31);
    }

    #[test]
    fn replicated_sends_are_counted_once_on_each_node() {
        let source = Store::open_memory("node").unwrap();
        source.append_claim(&ClaimInput {
            subject:"message/replicated".into(),kind:"message.sent".into(),actor:None,
            fields:serde_json::from_value(json!({"from":"agent/alder","to":"agent/birch","title":"Update","status":"sent"})).unwrap(),
            evidence:vec![],expected_subject:None,idempotency_key:None,
        }).unwrap();
        let peer = Store::open_memory("peer").unwrap();
        let exchange =
            super::super::tests::exchange_from(&source, &peer.replication_inventory().unwrap());
        super::super::tests::receive_and_project(&peer, "node", &exchange);
        super::super::tests::receive_and_project(&peer, "node", &exchange);
        let until = now_ms() as u64 + 1;
        let (_, expected) = source.usage_period_report(0, until).unwrap();
        assert_eq!(peer.usage_period_report(0, until).unwrap().1, expected);
        assert_eq!(
            expected["days"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d["messages"].as_u64().unwrap())
                .sum::<u64>(),
            1
        );
    }
}
