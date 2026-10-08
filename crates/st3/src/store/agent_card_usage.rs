//! Current usage heads with bounded canonical suffix repair for legacy response costs.
//! Floating point response costs are folded in the exact legacy canonical order;
//! a tree sum or unordered SQL sum would produce different values. Incomplete
//! visible suffixes refuse reads. Rollup/cumulative heads may shadow a retained
//! pending legacy suffix, which becomes required again if those heads retract.

use super::*;
use serde::Deserialize;
use smallclaims::ivm::install::Namespace;

const READ_GROUP_BOUND: usize = 128;
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_agent_card_usage_inputs (
 namespace TEXT NOT NULL, claim TEXT NOT NULL, agent TEXT NOT NULL,
 incarnation TEXT NOT NULL, semantics TEXT NOT NULL, slot TEXT NOT NULL,
 rank BLOB NOT NULL, total BLOB, fields TEXT NOT NULL, at TEXT NOT NULL,
 spend INTEGER NOT NULL CHECK(spend IN(0,1)), prefix TEXT,
 PRIMARY KEY(namespace,claim)
);
CREATE INDEX IF NOT EXISTS local_agent_card_usage_order
 ON local_agent_card_usage_inputs(namespace,agent,incarnation,semantics,rank,claim);
CREATE INDEX IF NOT EXISTS local_agent_card_usage_cumulative
 ON local_agent_card_usage_inputs(namespace,agent,incarnation,semantics,total DESC,rank DESC,claim DESC) WHERE total IS NOT NULL;
CREATE INDEX IF NOT EXISTS local_agent_card_usage_rollup
 ON local_agent_card_usage_inputs(namespace,agent,incarnation,semantics,slot,rank DESC,claim DESC);
CREATE INDEX IF NOT EXISTS local_agent_card_usage_context
 ON local_agent_card_usage_inputs(namespace,agent,semantics,rank DESC,claim DESC);
CREATE TABLE IF NOT EXISTS local_agent_card_usage_groups (
 namespace TEXT NOT NULL, agent TEXT NOT NULL, incarnation TEXT NOT NULL,
 count INTEGER NOT NULL CHECK(count>0), PRIMARY KEY(namespace,agent,incarnation)
);
CREATE TABLE IF NOT EXISTS local_agent_card_usage_rollups (
 namespace TEXT NOT NULL, agent TEXT NOT NULL, incarnation TEXT NOT NULL,
 slot TEXT NOT NULL, fields TEXT NOT NULL, PRIMARY KEY(namespace,agent,incarnation,slot)
);
CREATE TABLE IF NOT EXISTS local_agent_card_usage_dirty (
 namespace TEXT NOT NULL, agent TEXT NOT NULL, incarnation TEXT NOT NULL,
 start BLOB NOT NULL, active INTEGER NOT NULL CHECK(active IN(0,1)),
 PRIMARY KEY(namespace,agent,incarnation)
);
CREATE INDEX IF NOT EXISTS local_agent_card_usage_active
 ON local_agent_card_usage_dirty(namespace,active,agent,incarnation);
CREATE TABLE IF NOT EXISTS local_agent_card_usage_rows (
 namespace TEXT NOT NULL,agent TEXT NOT NULL,body TEXT,
 PRIMARY KEY(namespace,agent)
);
"#;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    Ok(())
}

#[derive(Clone, Serialize, Deserialize, Default)]
struct Responses {
    total: u64,
    input: u64,
    output: u64,
    cached: u64,
    #[serde(with = "cost_bits")]
    cost: f64,
    has_cost: bool,
    currency: Option<String>,
}
// Intermediate sums must survive page boundaries bit-for-bit. Decimal JSON
// parsing is not an exact float round-trip under the Store's parser settings.
mod cost_bits {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(value.to_bits())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
        Ok(f64::from_bits(u64::deserialize(deserializer)?))
    }
}
impl Responses {
    fn add(&mut self, fields: &Value) {
        self.total = self.total.saturating_add(number(fields, "total_tokens"));
        self.input = self.input.saturating_add(number(fields, "input_tokens"));
        self.output = self.output.saturating_add(number(fields, "output_tokens"));
        self.cached = self.cached.saturating_add(number(fields, "cached_tokens"));
        if let Some(cost) = fields["cost"].as_f64() {
            self.cost += cost;
            self.has_cost = true;
        }
        if let Some(currency) = fields["currency"].as_str() {
            self.currency = Some(currency.to_owned());
        }
    }
}
fn number(fields: &Value, name: &str) -> u64 {
    fields[name].as_u64().unwrap_or(0)
}

#[derive(Serialize, Deserialize)]
struct StoredFields {
    value: Value,
    cost_bits: Option<u64>,
    percent_bits: Option<u64>,
}
fn encode_fields(fields: &Value) -> Result<String> {
    Ok(serde_json::to_string(&StoredFields {
        value: fields.clone(),
        cost_bits: fields["cost"].as_f64().map(f64::to_bits),
        percent_bits: fields["context_used_percent"].as_f64().map(f64::to_bits),
    })?)
}
fn decode_fields(encoded: &str) -> Result<Value> {
    let mut fields: StoredFields = serde_json::from_str(encoded)?;
    for (name, bits) in [
        ("cost", fields.cost_bits),
        ("context_used_percent", fields.percent_bits),
    ] {
        if let Some(bits) = bits {
            fields.value[name] = json!(f64::from_bits(bits));
        }
    }
    Ok(fields.value)
}

#[derive(Clone)]
struct Input {
    agent: String,
    incarnation: String,
    semantics: String,
    slot: String,
    rank: Vec<u8>,
    fields: Value,
    at: u128,
    total: Option<Vec<u8>>,
    spend: bool,
}
fn input(claim: &ClaimRecord, key: &canonical::ClaimKey) -> Option<Input> {
    if claim.kind != "harness.usage" || !claim.subject.starts_with("agent/") {
        return None;
    }
    let fields = claim.body.get("fields").unwrap_or(&claim.body);
    let semantics = fields["semantics"].as_str().unwrap_or("").to_owned();
    let total = fields["total_tokens"]
        .as_u64()
        .map(|n| n.to_be_bytes().to_vec());
    Some(Input {
        agent: claim.subject.clone(),
        incarnation: fields["incarnation_id"]
            .as_str()
            .unwrap_or("unknown")
            .to_owned(),
        spend: semantics == "response_rollup"
            || (matches!(semantics.as_str(), "response" | "session_cumulative") && total.is_some()),
        semantics,
        slot: ["model", "account", "owner_run", "owner_step", "host"]
            .iter()
            .map(|name| fields[*name].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\0"),
        rank: canonical::sortable_key(key),
        fields: fields.clone(),
        at: claim.accepted_at_unix_ms,
        total,
    })
}
fn stored(connection: &Connection, namespace: &str, claim: &str) -> Result<Option<Input>> {
    let value = connection.query_row("SELECT agent,incarnation,semantics,slot,rank,fields,at,total,spend FROM local_agent_card_usage_inputs WHERE namespace=?1 AND claim=?2",params![namespace,claim],|r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,Vec<u8>>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,Option<Vec<u8>>>(7)?,r.get::<_,bool>(8)?))).optional()?;
    value
        .map(
            |(agent, incarnation, semantics, slot, rank, fields, at, total, spend)| {
                Ok(Input {
                    agent,
                    incarnation,
                    semantics,
                    slot,
                    rank,
                    fields: decode_fields(&fields)?,
                    at: at.parse()?,
                    total,
                    spend,
                })
            },
        )
        .transpose()
}

fn remove(
    tx: &Transaction<'_>,
    namespace: &str,
    claim: &str,
    affected: &mut Vec<Input>,
) -> Result<()> {
    if let Some(old) = stored(tx, namespace, claim)? {
        if old.spend {
            tx.execute("DELETE FROM local_agent_card_usage_groups WHERE namespace=?1 AND agent=?2 AND incarnation=?3 AND count=1",params![namespace,old.agent,old.incarnation])?;
            tx.execute("UPDATE local_agent_card_usage_groups SET count=count-1 WHERE namespace=?1 AND agent=?2 AND incarnation=?3",params![namespace,old.agent,old.incarnation])?;
        }
        tx.execute(
            "DELETE FROM local_agent_card_usage_inputs WHERE namespace=?1 AND claim=?2",
            params![namespace, claim],
        )?;
        affected.push(old);
    }
    Ok(())
}

pub(super) fn apply_claim(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    old: Option<&ClaimRecord>,
    new: Option<(&ClaimRecord, &canonical::ClaimKey)>,
) -> Result<BTreeSet<String>> {
    apply(tx, namespace.as_str(), old, new)
}
fn apply(
    tx: &Transaction<'_>,
    namespace: &str,
    old: Option<&ClaimRecord>,
    new: Option<(&ClaimRecord, &canonical::ClaimKey)>,
) -> Result<BTreeSet<String>> {
    let mut affected = Vec::new();
    if let Some(old) = old {
        remove(tx, namespace, &old.id, &mut affected)?;
    }
    if let Some((claim, key)) = new {
        remove(tx, namespace, &claim.id, &mut affected)?;
        if let Some(next) = input(claim, key) {
            tx.execute("INSERT INTO local_agent_card_usage_inputs VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,NULL)",params![namespace,claim.id,next.agent,next.incarnation,next.semantics,next.slot,next.rank,next.total,encode_fields(&next.fields)?,next.at.to_string(),next.spend])?;
            if next.spend {
                tx.execute("INSERT INTO local_agent_card_usage_groups VALUES(?1,?2,?3,1) ON CONFLICT(namespace,agent,incarnation) DO UPDATE SET count=count+1",params![namespace,next.agent,next.incarnation])?;
            }
            affected.push(next);
        }
    }
    let mut agents = BTreeSet::new();
    let mut groups = BTreeSet::new();
    for value in affected {
        agents.insert(value.agent.clone());
        groups.insert((value.agent.clone(), value.incarnation.clone()));
        if value.semantics == "response" && value.total.is_some() {
            tx.execute("INSERT INTO local_agent_card_usage_dirty VALUES(?1,?2,?3,?4,1) ON CONFLICT(namespace,agent,incarnation) DO UPDATE SET start=MIN(start,excluded.start)",params![namespace,value.agent,value.incarnation,value.rank])?;
        }
        if value.semantics == "response_rollup" {
            let fields: Option<String> = tx.query_row("SELECT fields FROM local_agent_card_usage_inputs WHERE namespace=?1 AND agent=?2 AND incarnation=?3 AND semantics='response_rollup' AND slot=?4 ORDER BY rank DESC,claim DESC LIMIT 1",params![namespace,value.agent,value.incarnation,value.slot],|r|r.get(0)).optional()?;
            if let Some(fields) = fields {
                tx.execute("INSERT INTO local_agent_card_usage_rollups VALUES(?1,?2,?3,?4,?5) ON CONFLICT(namespace,agent,incarnation,slot) DO UPDATE SET fields=excluded.fields",params![namespace,value.agent,value.incarnation,value.slot,fields])?;
            } else {
                tx.execute("DELETE FROM local_agent_card_usage_rollups WHERE namespace=?1 AND agent=?2 AND incarnation=?3 AND slot=?4",params![namespace,value.agent,value.incarnation,value.slot])?;
            }
        }
    }
    for (agent, incarnation) in groups {
        tx.execute("UPDATE local_agent_card_usage_dirty SET active=NOT(EXISTS(SELECT 1 FROM local_agent_card_usage_rollups WHERE namespace=?1 AND agent=?2 AND incarnation=?3) OR EXISTS(SELECT 1 FROM local_agent_card_usage_inputs WHERE namespace=?1 AND agent=?2 AND incarnation=?3 AND semantics='session_cumulative' AND total IS NOT NULL)) WHERE namespace=?1 AND agent=?2 AND incarnation=?3",params![namespace,agent,incarnation])?;
    }
    Ok(agents)
}

/// Process at most `limit` response rows over all affected groups. Every iteration
/// starts from the immediately preceding valid prefix, preserving float rounding.
pub(super) fn drain(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    limit: usize,
) -> Result<(usize, BTreeSet<String>)> {
    drain_at(tx, namespace.as_str(), limit)
}
fn drain_at(
    tx: &Transaction<'_>,
    namespace: &str,
    limit: usize,
) -> Result<(usize, BTreeSet<String>)> {
    anyhow::ensure!(
        (1..=128).contains(&limit),
        "invalid usage repair page bound"
    );
    let mut processed = 0;
    let mut agents = BTreeSet::new();
    while processed < limit {
        let dirty: Option<(String,String,Vec<u8>)> = tx.query_row("SELECT agent,incarnation,start FROM local_agent_card_usage_dirty WHERE namespace=?1 AND active=1 ORDER BY agent,incarnation LIMIT 1",[namespace],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((agent, incarnation, start)) = dirty else {
            break;
        };
        let before: Option<Option<String>> = tx.query_row("SELECT prefix FROM local_agent_card_usage_inputs WHERE namespace=?1 AND agent=?2 AND incarnation=?3 AND semantics='response' AND total IS NOT NULL AND rank<?4 ORDER BY rank DESC,claim DESC LIMIT 1",params![namespace,agent,incarnation,start],|r|r.get(0)).optional()?;
        let mut prefix = match before {
            None => Responses::default(),
            Some(prefix) => {
                serde_json::from_str(&prefix.context("usage suffix missing preceding prefix")?)?
            }
        };
        let remaining = limit - processed;
        let mut statement = tx.prepare("SELECT claim,rank,fields FROM local_agent_card_usage_inputs WHERE namespace=?1 AND agent=?2 AND incarnation=?3 AND semantics='response' AND total IS NOT NULL AND rank>=?4 ORDER BY rank,claim LIMIT ?5")?;
        let rows = statement
            .query_map(
                params![namespace, agent, incarnation, start, remaining + 1],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (claim, _, fields) in rows.iter().take(remaining) {
            prefix.add(&decode_fields(fields)?);
            anyhow::ensure!(prefix.cost.is_finite(), "non-finite canonical usage cost");
            tx.execute("UPDATE local_agent_card_usage_inputs SET prefix=?3 WHERE namespace=?1 AND claim=?2",params![namespace,claim,serde_json::to_string(&prefix)?])?;
            processed += 1;
        }
        if let Some((_, rank, _)) = rows.get(remaining) {
            tx.execute("UPDATE local_agent_card_usage_dirty SET start=?4 WHERE namespace=?1 AND agent=?2 AND incarnation=?3",params![namespace,agent,incarnation,rank])?;
        } else {
            tx.execute("DELETE FROM local_agent_card_usage_dirty WHERE namespace=?1 AND agent=?2 AND incarnation=?3",params![namespace,agent,incarnation])?;
        }
        agents.insert(agent);
        // Empty retracted suffixes still consume a scheduling unit, so a page
        // cannot walk an unbounded number of zero-row dirty groups.
        if rows.is_empty() {
            processed += 1;
        }
    }
    Ok((processed, agents))
}

pub(super) fn clean(connection: &Connection, namespace: &Namespace) -> Result<bool> {
    Ok(!connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM local_agent_card_usage_dirty WHERE namespace=?1 AND active=1)",
        [namespace.as_str()],
        |r| r.get::<_, bool>(0),
    )?)
}

pub(super) fn summary(
    connection: &Connection,
    namespace: &Namespace,
    agent: &str,
) -> Result<Option<UsageSummary>> {
    read_published(connection, namespace.as_str(), agent)
}
fn read_published(
    connection: &Connection,
    namespace: &str,
    agent: &str,
) -> Result<Option<UsageSummary>> {
    let pending: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM local_agent_card_usage_dirty WHERE namespace=?1 AND active=1 AND agent=?2)",params![namespace,agent],|r|r.get(0))?;
    anyhow::ensure!(!pending, "usage response suffix is pending");
    let body: Option<Option<String>> = connection
        .query_row(
            "SELECT body FROM local_agent_card_usage_rows WHERE namespace=?1 AND agent=?2",
            params![namespace, agent],
            |r| r.get(0),
        )
        .optional()?;
    body.context("usage output not captured")?
        .map(|body| {
            let stored: StoredSummary = serde_json::from_str(&body)?;
            let mut value = stored.value;
            value.cost = stored.cost_bits.map(f64::from_bits);
            if let Some(context) = &mut value.context {
                context.used_percent = stored.percent_bits.map(f64::from_bits);
            }
            Ok(value)
        })
        .transpose()
}

#[derive(Serialize, Deserialize)]
struct StoredSummary {
    value: UsageSummary,
    cost_bits: Option<u64>,
    percent_bits: Option<u64>,
}
/// Recompute only one affected agent from its bounded current heads after repair.
/// The production read then seeks one published row, never these source groups.
pub(super) fn publish(tx: &Transaction<'_>, namespace: &Namespace, agent: &str) -> Result<bool> {
    publish_at(tx, namespace.as_str(), agent)
}
fn publish_at(tx: &Transaction<'_>, namespace: &str, agent: &str) -> Result<bool> {
    let body = read_summary(tx, namespace, agent)?
        .map(|value| {
            let cost_bits = value.cost.map(f64::to_bits);
            let percent_bits = value
                .context
                .as_ref()
                .and_then(|c| c.used_percent)
                .map(f64::to_bits);
            serde_json::to_string(&StoredSummary {
                value,
                cost_bits,
                percent_bits,
            })
        })
        .transpose()?;
    let old: Option<Option<String>> = tx
        .query_row(
            "SELECT body FROM local_agent_card_usage_rows WHERE namespace=?1 AND agent=?2",
            params![namespace, agent],
            |r| r.get(0),
        )
        .optional()?;
    if old.as_ref() == Some(&body) {
        return Ok(false);
    }
    tx.execute("INSERT INTO local_agent_card_usage_rows VALUES(?1,?2,?3) ON CONFLICT(namespace,agent) DO UPDATE SET body=excluded.body",params![namespace,agent,body])?;
    Ok(true)
}
fn read_summary(
    connection: &Connection,
    namespace: &str,
    agent: &str,
) -> Result<Option<UsageSummary>> {
    let saw: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM local_agent_card_usage_inputs WHERE namespace=?1 AND agent=?2)",params![namespace,agent],|r|r.get(0))?;
    if !saw {
        return Ok(None);
    }
    let pending: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM local_agent_card_usage_dirty WHERE namespace=?1 AND active=1 AND agent=?2)",params![namespace,agent],|r|r.get(0))?;
    anyhow::ensure!(!pending, "usage response suffix is pending");
    let mut statement = connection.prepare("SELECT incarnation FROM local_agent_card_usage_groups WHERE namespace=?1 AND agent=?2 ORDER BY incarnation LIMIT ?3")?;
    let groups = statement
        .query_map(params![namespace, agent, READ_GROUP_BOUND + 1], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    anyhow::ensure!(
        groups.len() <= READ_GROUP_BOUND,
        "usage incarnation bound exceeded"
    );
    let context: Option<(String,String)> = connection.query_row("SELECT fields,at FROM local_agent_card_usage_inputs WHERE namespace=?1 AND agent=?2 AND semantics='context_occupancy' ORDER BY rank DESC,claim DESC LIMIT 1",params![namespace,agent],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let context = context
        .map(|(fields, at)| -> Result<ContextUsage> {
            let fields = decode_fields(&fields)?;
            Ok(ContextUsage {
                used_tokens: fields["context_used_tokens"].as_u64(),
                window_tokens: fields["context_window_tokens"].as_u64(),
                used_percent: fields["context_used_percent"].as_f64(),
                model: fields["model"].as_str().map(str::to_owned),
                compactions: number(&fields, "compactions"),
                last_compaction_ms: fields["last_compaction_ms"].as_u64(),
                last_compaction_trigger: fields["last_compaction_trigger"]
                    .as_str()
                    .map(str::to_owned),
                observed_at_unix_ms: at.parse()?,
            })
        })
        .transpose()?;
    let mut summary = UsageSummary {
        aggregation: "cumulative-per-incarnation-else-response-deltas".into(),
        context,
        ..UsageSummary::default()
    };
    let mut cost = 0.0;
    let mut has_cost = false;
    for incarnation in groups {
        summary.incarnation_count += 1;
        let mut statement = connection.prepare("SELECT fields FROM local_agent_card_usage_rollups WHERE namespace=?1 AND agent=?2 AND incarnation=?3 ORDER BY slot LIMIT ?4")?;
        let rollups = statement
            .query_map(
                params![namespace, agent, incarnation, READ_GROUP_BOUND + 1],
                |r| r.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        anyhow::ensure!(
            rollups.len() <= READ_GROUP_BOUND,
            "usage rollup slot bound exceeded"
        );
        if !rollups.is_empty() {
            for fields in rollups {
                let fields = decode_fields(&fields)?;
                summary.total_tokens = summary
                    .total_tokens
                    .saturating_add(number(&fields, "total_tokens"));
                summary.input_tokens = summary
                    .input_tokens
                    .saturating_add(number(&fields, "input_tokens"));
                summary.output_tokens = summary
                    .output_tokens
                    .saturating_add(number(&fields, "output_tokens"));
                summary.cache_write_tokens = summary
                    .cache_write_tokens
                    .saturating_add(number(&fields, "cache_write_tokens"));
                summary.cached_tokens = summary
                    .cached_tokens
                    .saturating_add(number(&fields, "cached_tokens"));
            }
            continue;
        }
        let cumulative: Option<String> = connection.query_row("SELECT fields FROM local_agent_card_usage_inputs WHERE namespace=?1 AND agent=?2 AND incarnation=?3 AND semantics='session_cumulative' AND total IS NOT NULL ORDER BY total DESC,rank DESC,claim DESC LIMIT 1",params![namespace,agent,incarnation],|r|r.get(0)).optional()?;
        let response = if let Some(fields) = cumulative {
            let fields = decode_fields(&fields)?;
            Responses {
                total: number(&fields, "total_tokens"),
                input: number(&fields, "input_tokens"),
                output: number(&fields, "output_tokens"),
                cached: number(&fields, "cached_tokens"),
                cost: fields["cost"].as_f64().unwrap_or(0.0),
                has_cost: fields["cost"].as_f64().is_some(),
                currency: fields["currency"].as_str().map(str::to_owned),
            }
        } else {
            let prefix: Option<Option<String>> = connection.query_row("SELECT prefix FROM local_agent_card_usage_inputs WHERE namespace=?1 AND agent=?2 AND incarnation=?3 AND semantics='response' AND total IS NOT NULL ORDER BY rank DESC,claim DESC LIMIT 1",params![namespace,agent,incarnation],|r|r.get(0)).optional()?;
            serde_json::from_str(
                &prefix
                    .flatten()
                    .context("usage response prefix unavailable")?,
            )?
        };
        summary.total_tokens = summary.total_tokens.saturating_add(response.total);
        summary.input_tokens = summary.input_tokens.saturating_add(response.input);
        summary.output_tokens = summary.output_tokens.saturating_add(response.output);
        summary.cached_tokens = summary.cached_tokens.saturating_add(response.cached);
        if response.has_cost {
            cost += response.cost;
            has_cost = true;
        }
        summary.currency = summary.currency.or(response.currency);
    }
    anyhow::ensure!(cost.is_finite(), "non-finite aggregate usage cost");
    summary.cost = has_cost.then_some(cost);
    Ok(Some(summary))
}

#[cfg(test)]
mod tests {
    use super::*;
    const AGENT: &str = "agent/grove.cedar";
    struct Fixture {
        store: Store,
    }
    impl Fixture {
        fn new() -> Self {
            let store = Store::open_memory("grove").unwrap();
            create_schema(&store.connection.write()).unwrap();
            Self { store }
        }
        fn append(&self, origin: &str, fields: Value, at: u128) -> ClaimRecord {
            self.store.set_write_clock_at(at).unwrap();
            let mut connection = self.store.connection.write();
            let tx = connection.transaction().unwrap();
            let claim = smallclaims::store::append_claim_record_tx(
                &tx,
                origin,
                AGENT,
                "harness.usage",
                None,
                &json!({"fields":fields}),
                &[],
                None,
            )
            .unwrap();
            let key = canonical::claim_key(&tx, &claim.id).unwrap();
            apply(&tx, "live", None, Some((&claim, &key))).unwrap();
            while drain_at(&tx, "live", 128).unwrap().0 > 0 {}
            publish_at(&tx, "live", AGENT).unwrap();
            tx.commit().unwrap();
            drop(connection);
            self.check();
            claim
        }
        fn check(&self) {
            let connection = self.store.readers.get();
            let index = current_index(&connection).unwrap();
            let actual = read_summary(&connection, "live", AGENT).unwrap();
            assert_eq!(actual, read_published(&connection, "live", AGENT).unwrap());
            let expected = self
                .store
                .usage_summaries_at(&[AGENT.to_owned()], Some(index))
                .unwrap()
                .remove(AGENT);
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
    }

    #[test]
    fn canonical_cost_rounding_late_arrival_saturation_and_context_match_store() {
        let f = Fixture::new();
        for (origin, at, cost, total) in [
            ("grove", 100, 1.0e16, u64::MAX),
            ("grove", 300, 1.0, 17),
            ("remote", 200, -1.0e16, 23),
        ] {
            f.append(origin,json!({"incarnation_id":"one","semantics":"response","total_tokens":total,"input_tokens":4,"cost":cost,"currency":"USD"}),at);
        }
        assert_eq!(
            read_summary(&f.store.readers.get(), "live", AGENT)
                .unwrap()
                .unwrap()
                .cost,
            Some(1.0)
        );
        f.append("grove",json!({"semantics":"context_occupancy","context_used_tokens":40,"context_window_tokens":100,"context_used_percent":40.0,"model":"invented-model","compactions":2}),400);
        f.append("grove",json!({"incarnation_id":"two","semantics":"response","total_tokens":11,"cost":0.1,"currency":"EUR"}),500);
        f.append("grove",json!({"incarnation_id":"two","semantics":"session_cumulative","total_tokens":50,"cost":0.5,"currency":"EUR"}),600);
        // A lower cumulative receipt is not the winner; equal totals use later canonical rank.
        f.append("grove",json!({"incarnation_id":"two","semantics":"session_cumulative","total_tokens":49,"cost":200.0}),700);
        f.append("grove",json!({"incarnation_id":"two","semantics":"session_cumulative","total_tokens":50,"cost":0.7,"currency":"GBP"}),800);
    }

    #[test]
    fn partial_suffix_is_unavailable_and_resumable_without_touching_published_namespace() {
        let f = Fixture::new();
        let mut claims = Vec::new();
        for i in 0..12 {
            claims.push(f.append("grove",json!({"incarnation_id":"one","semantics":"response","total_tokens":i+1,"cost":0.1}),100+i));
        }
        let mut connection = f.store.connection.write();
        let tx = connection.transaction().unwrap();
        for claim in claims.iter().rev() {
            let key = canonical::claim_key(&tx, &claim.id).unwrap();
            apply(&tx, "staging", None, Some((claim, &key))).unwrap();
        }
        assert!(read_summary(&tx, "staging", AGENT).is_err());
        let page = drain_at(&tx, "staging", 2).unwrap();
        assert_eq!(page.0, 2);
        assert!(read_summary(&tx, "staging", AGENT).is_err());
        let published = serde_json::to_value(read_summary(&tx, "live", AGENT).unwrap()).unwrap();
        tx.commit().unwrap();
        let tx = connection.transaction().unwrap();
        while drain_at(&tx, "staging", 2).unwrap().0 > 0 {}
        assert_eq!(
            serde_json::to_value(read_summary(&tx, "staging", AGENT).unwrap()).unwrap(),
            published
        );
        tx.rollback().unwrap();
        assert!(read_summary(&connection, "staging", AGENT).is_err());
        assert_eq!(
            serde_json::to_value(read_summary(&connection, "live", AGENT).unwrap()).unwrap(),
            published
        );
    }

    #[test]
    fn rollup_shadow_retraction_and_unknown_usage_match_store() {
        let f = Fixture::new();
        f.append("grove",json!({"incarnation_id":"one","semantics":"response","total_tokens":10,"cost":0.1,"currency":"USD"}),100);
        let rollup = f.append("grove",json!({"incarnation_id":"one","semantics":"response_rollup","model":"invented-model","owner_step":"step/invented","total_tokens":40,"cache_write_tokens":9,"cost":500.0}),200);
        f.append("grove",json!({"incarnation_id":"one","semantics":"response","total_tokens":20,"cost":0.2,"currency":"USD"}),300);
        f.append(
            "grove",
            json!({"incarnation_id":"unknown","semantics":"unsupported","total_tokens":999}),
            400,
        );
        assert_eq!(
            read_summary(&f.store.readers.get(), "live", AGENT)
                .unwrap()
                .unwrap()
                .cost,
            None
        );
        let mut connection = f.store.connection.write();
        let tx = connection.transaction().unwrap();
        tx.execute("DELETE FROM claims WHERE id=?1", [&rollup.id])
            .unwrap();
        apply(&tx, "live", Some(&rollup), None).unwrap();
        assert!(read_summary(&tx, "live", AGENT).is_err());
        while drain_at(&tx, "live", 128).unwrap().0 > 0 {}
        publish_at(&tx, "live", AGENT).unwrap();
        tx.commit().unwrap();
        drop(connection);
        f.check();
        assert_eq!(
            read_summary(&f.store.readers.get(), "live", AGENT)
                .unwrap()
                .unwrap()
                .cost,
            Some(0.1 + 0.2)
        );
    }
}
