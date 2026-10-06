//! Numeric reader staging before the explicit capability barrier. Compatibility claims
//! still publish; each source also keeps its latest aggregate/account-window reading in
//! the same transaction. This does not enable a cutover or remove observation history.
use super::*;

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS numeric_values (
    origin TEXT NOT NULL, kind TEXT NOT NULL, slot TEXT NOT NULL,
    subject TEXT NOT NULL, claim_id TEXT NOT NULL, store_index INTEGER NOT NULL,
    canonical_key BLOB NOT NULL, measured_at INTEGER, reset_at INTEGER, body TEXT NOT NULL,
    PRIMARY KEY(origin,kind,slot)
);
CREATE INDEX IF NOT EXISTS numeric_values_subject_index ON numeric_values(subject,kind);
CREATE TABLE IF NOT EXISTS numeric_account_windows (
    slot TEXT PRIMARY KEY, window TEXT NOT NULL,
    origin TEXT NOT NULL, subject TEXT NOT NULL, claim_id TEXT NOT NULL,
    canonical_key BLOB NOT NULL, measured_at INTEGER NOT NULL, reset_at INTEGER,
    body TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS numeric_limit_seats (
    subject TEXT PRIMARY KEY, canonical_key BLOB NOT NULL,
    origin TEXT NOT NULL, body TEXT NOT NULL
);
"#;

pub(super) fn open(tx: &Transaction<'_>) -> Result<()> {
    let initialized: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key='numeric-value-staging' AND value='2')",
        [],
        |row| row.get(0),
    )?;
    if initialized {
        return Ok(());
    }
    // Only compatibility-derived staging exists at this phase. Rebuild it once when
    // adding the bounded account/seat projections; no graph claim or source guard is removed.
    tx.execute("DELETE FROM numeric_values", [])?;
    tx.execute("DELETE FROM numeric_account_windows", [])?;
    tx.execute("DELETE FROM numeric_limit_seats", [])?;
    let claims = tx.prepare(&canonical_sql(
        "SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms
         FROM claims WHERE kind IN ('harness.usage','harness.limits') ORDER BY CANONICAL_ASC(claims)"
    ))?.query_map([], claim_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    for claim in claims {
        stage(tx, &claim)?;
    }
    tx.execute(
        "INSERT OR REPLACE INTO meta(key,value) VALUES ('numeric-value-staging','2')",
        [],
    )?;
    Ok(())
}

/// A native acknowledgement and its staged reading commit together with the existing
/// compatibility operation. Replay cannot advance the reading or change its source time.
pub(super) fn stage(tx: &Transaction<'_>, claim: &ClaimRecord) -> Result<()> {
    let Some(fields) = claim.body.get("fields") else {
        return Ok(());
    };
    let mut slots = Vec::new();
    match claim.kind.as_str() {
        "harness.usage" => {
            if !matches!(
                fields["semantics"].as_str(),
                Some("session_cumulative" | "response_rollup")
            ) {
                // Response deltas require aggregation, never last-delta replacement.
                // Occupancy already belongs to the independent best-effort register.
                return Ok(());
            }
            let fields_map = serde_json::from_value::<BTreeMap<String, Value>>(fields.clone())?;
            slots.push((
                json!([claim.subject, usage_slot(&fields_map)]).to_string(),
                fields["observed_at_unix_ms"].as_u64(),
                None,
            ));
        }
        "harness.limits" => {
            let Some(driver) = fields["driver"].as_str() else {
                return Ok(());
            };
            let Some(measured_at) = fields["measured_at_unix_ms"].as_u64() else {
                return Ok(());
            };
            let account =
                if let Some(name) = fields["account_ref"].as_str().filter(|s| !s.is_empty()) {
                    json!(["declared", name])
                } else if let Some(label) = fields["account"].as_str().filter(|s| !s.is_empty()) {
                    json!(["provider", label])
                } else {
                    // An unidentified seat cannot provide quota proof for a different seat.
                    json!(["unknown", claim.subject])
                };
            for window in ["weekly", "five_hour"] {
                if fields[format!("{window}_percent")]
                    .as_f64()
                    .is_some_and(f64::is_finite)
                {
                    slots.push((
                        json!([driver, account, window]).to_string(),
                        Some(measured_at),
                        fields[format!("{window}_resets_at_unix_ms")].as_u64(),
                    ));
                }
            }
        }
        _ => return Ok(()),
    }
    let key = canonical::sortable_key(&canonical::claim_key(tx, &claim.id)?);
    if claim.kind == "harness.limits" {
        tx.execute(
            "INSERT INTO numeric_limit_seats(subject,canonical_key,origin,body) VALUES (?1,?2,?3,?4)
             ON CONFLICT(subject) DO UPDATE SET canonical_key=excluded.canonical_key,
                origin=excluded.origin,body=excluded.body
             WHERE excluded.canonical_key>numeric_limit_seats.canonical_key",
            params![claim.subject,key,claim.origin,canonical_json_text(&claim.body)?],
        )?;
    }
    for (slot, measured_at, reset_at) in slots {
        let previous = tx
            .query_row(
                "SELECT canonical_key,measured_at,reset_at,body FROM numeric_values
             WHERE origin=?1 AND kind=?2 AND slot=?3",
                params![claim.origin, claim.kind, slot],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Option<u64>>(1)?,
                        row.get::<_, Option<u64>>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        if let Some((old_key, old_measured, old_reset, old_body)) = previous {
            if claim.kind == "harness.limits" {
                // Reset advancement wins over a delayed exhausted old window. A re-stamp
                // cannot freshen evidence: only the actual provider source time orders it.
                if reset_at < old_reset
                    || (reset_at == old_reset && (measured_at, &key) <= (old_measured, &old_key))
                {
                    continue;
                }
            } else {
                let old: Value = serde_json::from_str(&old_body)?;
                if fields["semantics"] == "session_cumulative" {
                    let total = fields["total_tokens"].as_u64().unwrap_or(0);
                    let old_total = old["fields"]["total_tokens"].as_u64().unwrap_or(0);
                    if total < old_total || (total == old_total && key <= old_key) {
                        continue;
                    }
                } else if key <= old_key {
                    continue;
                }
            }
        }
        tx.execute(
            "INSERT INTO numeric_values(origin,kind,slot,subject,claim_id,store_index,canonical_key,measured_at,reset_at,body)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(origin,kind,slot) DO UPDATE SET subject=excluded.subject,claim_id=excluded.claim_id,
                store_index=excluded.store_index,canonical_key=excluded.canonical_key,measured_at=excluded.measured_at,
                reset_at=excluded.reset_at,body=excluded.body",
            params![claim.origin,claim.kind,slot,claim.subject,claim.id,claim.store_index,key,
                measured_at,reset_at,canonical_json_text(&claim.body)?],
        )?;
        if claim.kind == "harness.limits" {
            let window = serde_json::from_str::<Value>(&slot)?[2]
                .as_str()
                .context("numeric account slot has no quota window")?
                .to_owned();
            tx.execute(
                "INSERT INTO numeric_account_windows(slot,window,origin,subject,claim_id,canonical_key,measured_at,reset_at,body)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
                 ON CONFLICT(slot) DO UPDATE SET origin=excluded.origin,subject=excluded.subject,
                    claim_id=excluded.claim_id,canonical_key=excluded.canonical_key,measured_at=excluded.measured_at,
                    reset_at=excluded.reset_at,body=excluded.body
                 WHERE coalesce(excluded.reset_at,-1)>coalesce(numeric_account_windows.reset_at,-1)
                    OR (excluded.reset_at IS numeric_account_windows.reset_at
                        AND (excluded.measured_at,excluded.canonical_key)>
                            (numeric_account_windows.measured_at,numeric_account_windows.canonical_key))",
                params![slot,window,claim.origin,claim.subject,claim.id,key,measured_at,reset_at,
                    canonical_json_text(&claim.body)?],
            )?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
pub struct NumericLimitSource {
    pub measured_at_unix_ms: u64,
    pub measured_by: String,
    pub host: String,
    pub source_claim: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct NumericAccountLimit {
    #[serde(flatten)]
    pub reading: AccountLimit,
    pub weekly_source: Option<NumericLimitSource>,
    pub five_hour_source: Option<NumericLimitSource>,
}

type AccountKey = (String, String, Option<String>, Option<String>);

fn account_key(reading: &AccountLimit, seat: &str) -> AccountKey {
    (
        reading.driver.clone(),
        reading.account.clone(),
        reading.account_ref.clone(),
        (!reading.identified).then(|| seat.to_owned()),
    )
}

impl Store {
    /// Reader-first quota view. It keeps independent source clocks for each window and
    /// reads bounded account/seat values. Existing policy remains on compatibility reads
    /// until an explicitly named, reviewed capability cutover switches its authority.
    pub fn numeric_account_limits(&self) -> Result<Vec<NumericAccountLimit>> {
        self.read_snapshot(|_| {
            let connection = self.readers.get();
            let mut accounts = BTreeMap::<AccountKey, NumericAccountLimit>::new();
            let mut statement = connection.prepare_cached(
                "SELECT window,origin,subject,claim_id,body FROM numeric_account_windows
                 ORDER BY slot",
            )?;
            for row in statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })? {
                let (window, origin, seat, source_claim, body) = row?;
                let mut body: Value = serde_json::from_str(&body)?;
                let other = if window == "weekly" {
                    "five_hour"
                } else {
                    "weekly"
                };
                let fields = body["fields"]
                    .as_object_mut()
                    .context("numeric limit has no fields")?;
                fields.remove(&format!("{other}_percent"));
                fields.remove(&format!("{other}_resets_at_unix_ms"));
                let Some((mut reading, _)) = limits::reading(&origin, &body) else {
                    continue;
                };
                reading.measured_by = seat.clone();
                let source = NumericLimitSource {
                    measured_at_unix_ms: reading.measured_at_unix_ms,
                    measured_by: seat.clone(),
                    host: origin,
                    source_claim,
                };
                let entry = accounts
                    .entry(account_key(&reading, &seat))
                    .or_insert_with(|| NumericAccountLimit {
                        reading: reading.clone(),
                        weekly_source: None,
                        five_hour_source: None,
                    });
                if window == "weekly" {
                    entry.reading.weekly_percent = reading.weekly_percent;
                    entry.reading.weekly_resets_at_unix_ms = reading.weekly_resets_at_unix_ms;
                    entry.reading.measured_at_unix_ms = source.measured_at_unix_ms;
                    entry.reading.measured_by = source.measured_by.clone();
                    entry.reading.host = source.host.clone();
                    entry.weekly_source = Some(source);
                } else {
                    entry.reading.five_hour_percent = reading.five_hour_percent;
                    entry.reading.five_hour_resets_at_unix_ms = reading.five_hour_resets_at_unix_ms;
                    entry.five_hour_source = Some(source);
                }
            }
            let mut seats = connection.prepare_cached(
                "SELECT subject,origin,body FROM numeric_limit_seats ORDER BY subject",
            )?;
            for row in seats.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })? {
                let (seat, origin, body) = row?;
                let body: Value = serde_json::from_str(&body)?;
                if let Some((reading, _)) = limits::reading(&origin, &body)
                    && let Some(account) = accounts.get_mut(&account_key(&reading, &seat))
                {
                    account.reading.seats.push(seat);
                }
            }
            Ok(accounts.into_values().collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(
        store: &Store,
        seat: &str,
        at: u64,
        weekly: Option<(f64, u64)>,
        five_hour: Option<(f64, u64)>,
    ) {
        let mut fields = BTreeMap::from([
            ("driver".into(), json!("codex")),
            ("account_ref".into(), json!("owner/account")),
            ("measured_at_unix_ms".into(), json!(at)),
        ]);
        for (window, reading) in [("weekly", weekly), ("five_hour", five_hour)] {
            if let Some((percent, reset)) = reading {
                fields.insert(format!("{window}_percent"), json!(percent));
                fields.insert(format!("{window}_resets_at_unix_ms"), json!(reset));
            }
        }
        store
            .append_claim(&ClaimInput {
                subject: seat.into(),
                kind: "harness.limits".into(),
                actor: Some(seat.into()),
                fields,
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }

    #[test]
    fn partial_limits_keep_independent_sources_and_delayed_resets_cannot_resurrect_exhaustion() {
        let store = Store::open_memory("owner").unwrap();
        limits(
            &store,
            "agent/cedar",
            10,
            Some((96.0, 100)),
            Some((20.0, 50)),
        );
        limits(&store, "agent/birch", 20, None, Some((30.0, 50)));
        let rows = || {
            store.readers.get().prepare(
            "SELECT measured_at,reset_at,body FROM numeric_values WHERE kind='harness.limits' ORDER BY slot"
        ).unwrap().query_map([],|row| Ok((row.get::<_,u64>(0)?,row.get::<_,u64>(1)?,
            serde_json::from_str::<Value>(&row.get::<_,String>(2)?).unwrap())))
            .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()
        };
        assert_eq!(rows().len(), 2);
        assert!(
            rows()
                .iter()
                .any(|(at, _, body)| *at == 10 && body["fields"]["weekly_percent"] == 96.0)
        );
        limits(&store, "agent/cedar", 30, Some((2.0, 200)), None);
        limits(&store, "agent/birch", 40, Some((99.0, 100)), None);
        assert!(rows().iter().any(|(at, reset, body)| *at == 30
            && *reset == 200
            && body["fields"]["weekly_percent"] == 2.0));
        assert!(
            rows()
                .iter()
                .any(|(at, _, body)| *at == 20 && body["fields"]["five_hour_percent"] == 30.0)
        );
        assert_eq!(
            store
                .readers
                .get()
                .query_row(
                    "SELECT count(*) FROM claims WHERE kind='harness.limits'",
                    [],
                    |r| r.get::<_, u64>(0)
                )
                .unwrap(),
            4
        );
        let current = store.numeric_account_limits().unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].reading.weekly_percent, Some(2.0));
        assert_eq!(current[0].reading.five_hour_percent, Some(30.0));
        assert_eq!(current[0].reading.measured_at_unix_ms, 30);
        assert_eq!(
            current[0]
                .weekly_source
                .as_ref()
                .unwrap()
                .measured_at_unix_ms,
            30
        );
        assert_eq!(
            current[0]
                .five_hour_source
                .as_ref()
                .unwrap()
                .measured_at_unix_ms,
            20
        );
        assert_eq!(current[0].reading.seats, ["agent/birch", "agent/cedar"]);
    }

    #[test]
    fn usage_staging_preserves_completed_billing_keys_and_cumulative_maxima() {
        let store = Store::open_memory("owner").unwrap();
        let publish = |semantics, step, total| {
            store.append_claim(&ClaimInput {
            subject:if semantics == "session_cumulative" {"agent/birch"} else {"agent/cedar"}.into(),
            kind:"harness.usage".into(),actor:Some(if semantics == "session_cumulative" {"agent/birch"} else {"agent/cedar"}.into()),
            fields:serde_json::from_value(json!({"driver":"codex","incarnation_id":"one","semantics":semantics,
                "model":"model","account":"provider/account","owner_step":step,"total_tokens":total})).unwrap(),
            evidence:vec![],expected_subject:None,idempotency_key:None,
        }).unwrap()
        };
        publish("response_rollup", "completed", 10);
        publish("response_rollup", "current", 20);
        publish("session_cumulative", "current", 40);
        publish("session_cumulative", "current", 30);
        let connection = store.readers.get();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM numeric_values", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            3
        );
        assert_eq!(connection.query_row("SELECT json_extract(body,'$.fields.total_tokens') FROM numeric_values WHERE json_extract(body,'$.fields.semantics')='session_cumulative'",[],|r|r.get::<_,u64>(0)).unwrap(),40);
        assert_eq!(
            store
                .usage_summary_at("agent/birch", None, None)
                .unwrap()
                .unwrap()
                .total_tokens,
            40
        );
        // Attributed response rollups remain distinct from provider cumulative totals.
        assert_eq!(
            store
                .usage_summary_at("agent/cedar", None, None)
                .unwrap()
                .unwrap()
                .total_tokens,
            30
        );
    }
    #[test]
    fn a_failed_staged_value_rolls_back_the_compatibility_claim_and_publication_guard() {
        let store = Store::open_memory("owner").unwrap();
        let input = ClaimInput {
            subject: "agent/cedar".into(),
            kind: "harness.limits".into(),
            actor: Some("agent/cedar".into()),
            fields: serde_json::from_value(json!({"driver":"codex","account":"provider/account",
                "weekly_percent":96.0,"weekly_resets_at_unix_ms":100,"measured_at_unix_ms":10}))
            .unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some("numeric-source:one".into()),
        };
        let connection = store.readers.get();
        store
            .connection
            .write()
            .execute_batch(
                "CREATE TRIGGER fail_numeric_staging BEFORE INSERT ON numeric_values
            BEGIN SELECT RAISE(ABORT,'injected numeric value failure'); END",
            )
            .unwrap();
        assert!(store.append_claim(&input).is_err());
        assert!(
            store
                .claims_for("agent/cedar", Some("harness.limits"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM operations", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            0
        );
        store
            .connection
            .write()
            .execute_batch("DROP TRIGGER fail_numeric_staging")
            .unwrap();
        let accepted = store.append_claim(&input).unwrap();
        assert_eq!(store.append_claim(&input).unwrap().id, accepted.id);
        assert_eq!(
            store
                .claims_for("agent/cedar", Some("harness.limits"))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM numeric_values", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row("SELECT measured_at FROM numeric_values", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            10
        );
    }
}
