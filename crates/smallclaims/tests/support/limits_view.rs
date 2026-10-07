//! Historical limits compatibility operator. This is not the latest-per-seat telemetry adapter.
//! Eight radix summaries per reading support an exact inclusive one-hour range maximum.
use super::real_views::{SCHEMA, fields, put};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;
use smallclaims::{
    ClaimRecord,
    ivm::{Definition, View},
    store::canonical,
};
use std::collections::BTreeSet;

pub struct AccountLimits;
pub fn account_key(f: &Value) -> String {
    serde_json::json!([f["driver"], f["account"], f["account_ref"]]).to_string()
}
const LIMIT_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS app_limit_candidates(account TEXT NOT NULL,window TEXT NOT NULL,id TEXT PRIMARY KEY,
    measured INTEGER NOT NULL,priority BLOB NOT NULL,payload TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS app_limit_fresh ON app_limit_candidates(account,window,measured DESC,priority DESC,id DESC);
CREATE TABLE IF NOT EXISTS app_limit_nodes(account TEXT NOT NULL,window TEXT NOT NULL,level INTEGER NOT NULL,
    prefix INTEGER NOT NULL,parent INTEGER NOT NULL,priority BLOB NOT NULL,payload TEXT NOT NULL,
    PRIMARY KEY(account,window,level,prefix));
CREATE INDEX IF NOT EXISTS app_limit_children ON app_limit_nodes(account,window,level,parent,priority DESC,prefix);
"#;

fn number(f: &Value, field: &str) -> Result<u64> {
    f[field].as_u64().with_context(|| field.to_owned())
}
fn optional_number(out: &mut Vec<u8>, value: &Value) -> Result<()> {
    if value.is_null() {
        out.push(0);
        out.extend_from_slice(&0u64.to_be_bytes());
    } else {
        out.push(1);
        out.extend_from_slice(&value.as_u64().context("invalid reset")?.to_be_bytes());
    }
    Ok(())
}
fn priority(f: &Value, window: &str, rank: &canonical::ClaimKey) -> Result<Vec<u8>> {
    let mut p = vec![];
    // Even five-hour-only selection first considers the weekly reset, as limits.rs does.
    optional_number(&mut p, &f["weekly_resets_at_unix_ms"])?;
    if window == "five_hour" {
        optional_number(&mut p, &f["five_hour_resets_at_unix_ms"])?;
    }
    let percent = f[format!("{window}_percent")]
        .as_f64()
        .context("missing percent")?;
    ensure!(
        percent.is_finite() && (0.0..=100.0).contains(&percent),
        "unsupported percent"
    );
    // Positive IEEE-754 numbers have the same byte order as their numeric order; normalize -0.
    p.extend_from_slice(
        &(if percent == 0.0 { 0.0f64 } else { percent })
            .to_bits()
            .to_be_bytes(),
    );
    p.extend_from_slice(&number(f, "measured_at_unix_ms")?.to_be_bytes());
    for byte in f["measured_by"]
        .as_str()
        .context("measuring seat missing")?
        .bytes()
    {
        p.push(byte);
        if byte == 0 {
            p.push(255);
        }
    }
    p.extend_from_slice(&[0, 0]);
    // limits.rs walks canonical history and max_by takes the later claim on an exact tie.
    p.extend_from_slice(&canonical::sortable_key(rank));
    Ok(p)
}

fn refresh_path(tx: &Transaction<'_>, account: &str, window: &str, time: u64) -> Result<()> {
    for level in 0..8u32 {
        let prefix = time >> (level * 8);
        let next: Option<(Vec<u8>, String)> = if level == 0 {
            tx.query_row(
                "SELECT priority,payload FROM app_limit_candidates WHERE account=?1 AND window=?2
                AND measured=?3 ORDER BY priority DESC,id DESC LIMIT 1",
                params![account, window, time],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
        } else {
            tx.query_row(
                "SELECT priority,payload FROM app_limit_nodes WHERE account=?1 AND window=?2
                AND level=?3 AND parent=?4 ORDER BY priority DESC,prefix LIMIT 1",
                params![account, window, level - 1, prefix],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
        };
        if let Some((priority, payload)) = next {
            let changed=tx.execute("INSERT INTO app_limit_nodes VALUES(?1,?2,?3,?4,?5,?6,?7)
                ON CONFLICT(account,window,level,prefix) DO UPDATE
                SET priority=excluded.priority,payload=excluded.payload
                WHERE app_limit_nodes.priority<>excluded.priority OR app_limit_nodes.payload<>excluded.payload",
                params![account,window,level,prefix,prefix>>8,priority,payload])?;
            if changed == 0 {
                break;
            }
        } else {
            let changed=tx.execute("DELETE FROM app_limit_nodes WHERE account=?1 AND window=?2 AND level=?3 AND prefix=?4",
            params![account,window,level,prefix])?;
            if changed == 0 {
                break;
            }
        }
    }
    Ok(())
}

/// Maximal aligned radix blocks partition [low, high], independent of retained history size.
pub fn blocks(mut low: u64, high: u64) -> Vec<(u32, u64)> {
    let mut result = vec![];
    while low <= high {
        let mut level = 0u32;
        while level < 7 {
            let span = 1u64 << ((level + 1) * 8);
            if !low.is_multiple_of(span) || span - 1 > high - low {
                break;
            }
            level += 1;
        }
        result.push((level, low >> (level * 8)));
        let span = 1u64 << (level * 8);
        if span > high - low {
            break;
        }
        low += span;
    }
    result
}

impl View for AccountLimits {
    fn definition(&self) -> Definition {
        Definition {
            name: "account-limits",
            fingerprint: "limits.rs.hour-window.v1;radix8.v1;original-time.v1;driver-account-ref.v1",
            kinds: &["harness.limits"],
            local_kinds: &[],
            max_contributions: 2,
        }
    }
    fn create_schema(&self, c: &Connection) -> Result<()> {
        c.execute_batch(SCHEMA)?;
        c.execute_batch(LIMIT_SCHEMA)?;
        Ok(())
    }
    fn affected_keys(
        &self,
        tx: &Transaction<'_>,
        old: Option<&ClaimRecord>,
        new: Option<&ClaimRecord>,
    ) -> Result<BTreeSet<String>> {
        let mut keys = BTreeSet::new();
        if let Some(old) = old {
            let prior = tx
                .query_row(
                    "SELECT account,window,measured FROM app_limit_candidates WHERE id=?1",
                    [&old.id],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, u64>(2)?,
                        ))
                    },
                )
                .optional()?;
            if let Some((account, window, time)) = prior {
                tx.execute("DELETE FROM app_limit_candidates WHERE id=?1", [&old.id])?;
                refresh_path(tx, &account, &window, time)?;
                keys.insert(account);
            }
        }
        if let Some(new) = new {
            let mut normalized = fields(new).clone();
            normalized["measured_by"] = Value::String(new.subject.clone());
            normalized["host"] = Value::String(new.origin.clone());
            let f = &normalized;
            ensure!(
                f["driver"].is_string() && f["account"].is_string(),
                "account identity missing"
            );
            let window = if !f["weekly_percent"].is_null() {
                "weekly"
            } else {
                "five_hour"
            };
            let account = account_key(f);
            let time = number(f, "measured_at_unix_ms")?;
            ensure!(
                time <= i64::MAX as u64,
                "measurement exceeds SQLite integer range"
            );
            tx.execute(
                "INSERT INTO app_limit_candidates VALUES(?1,?2,?3,?4,?5,?6)
                ON CONFLICT(id) DO NOTHING",
                params![
                    account,
                    window,
                    new.id,
                    time,
                    priority(f, window, &canonical::claim_key(tx, &new.id)?)?,
                    f.to_string()
                ],
            )?;
            refresh_path(tx, &account, window, time)?;
            keys.insert(account);
        }
        Ok(keys)
    }
    fn maintain_key(
        &self,
        tx: &Transaction<'_>,
        account: &str,
        _: Option<&ClaimRecord>,
        _: Option<&ClaimRecord>,
    ) -> Result<Option<bool>> {
        let mut fresh = None;
        for window in ["weekly", "five_hour"] {
            let latest = tx
                .query_row(
                    "SELECT measured FROM app_limit_candidates WHERE account=?1 AND window=?2
                ORDER BY measured DESC,priority DESC,id DESC LIMIT 1",
                    params![account, window],
                    |r| r.get::<_, u64>(0),
                )
                .optional()?;
            if let Some(time) = latest {
                fresh = Some((window, time));
                break;
            }
        }
        let mut best: Option<(Vec<u8>, String)> = None;
        if let Some((window, time)) = fresh {
            for (level, prefix) in blocks(time.saturating_sub(3_600_000), time) {
                let candidate = tx
                    .query_row(
                        "SELECT priority,payload FROM app_limit_nodes
                    WHERE account=?1 AND window=?2 AND level=?3 AND prefix=?4",
                        params![account, window, level, prefix],
                        |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?)),
                    )
                    .optional()?;
                if let Some(candidate) = candidate
                    && best.as_ref().is_none_or(|b| candidate.0 > b.0)
                {
                    best = Some(candidate);
                }
            }
        }
        let next = best
            .map(|(_, payload)| serde_json::from_str(&payload))
            .transpose()?;
        Ok(Some(put(tx, "account-limits", account, account, next)?))
    }
}
