//! Affected-key activity timestamps used by agent cards.
//!
//! The canonical card oracle selects each activity category by local arrival position,
//! then takes the greatest timestamp across categories. This source preserves that rule;
//! it does not substitute greatest timestamps or canonical claim ranks for arrival order.
//! Local capture, trim, rollback and prefix promotions must dispatch their old/new keys
//! transactionally. Default production readers do not register this partial card source.
use super::*;
use serde::Deserialize;
use smallclaims::ivm::{Contribution, Definition, LocalChange, View, Views};

pub const VIEW: &str = "st3.agents.activity.v1";
const KINDS: &[&str] = &[
    "harness.timeline",
    "work.progress",
    "work.submitted",
    "message.sent",
];
const REGISTERS: &[&str] = &["work", "sent", "received", "timeline"];

pub struct ActivityView;
pub fn definitions() -> Vec<Box<dyn View>> {
    vec![Box::new(ActivityView)]
}

/// Private dependency keys are never public collection IDs or authorization evidence.
/// An adapter maps them to the agent ID, then checks its actor in the same read snapshot.
pub fn dependency_key(agent: &str, incarnation: Option<&str>) -> Result<String> {
    anyhow::ensure!(
        agent.starts_with("agent/"),
        "activity source is not an agent"
    );
    Ok(serde_json::to_string(&(agent, incarnation))?)
}
pub fn dependency_agent(key: &str) -> Result<String> {
    Ok(decode_key(key)?.0)
}
fn decode_key(key: &str) -> Result<(String, Option<String>)> {
    let key: (String, Option<String>) = serde_json::from_str(key)?;
    anyhow::ensure!(
        key.0.starts_with("agent/"),
        "activity source is not an agent"
    );
    Ok(key)
}

#[derive(Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Times {
    // Strings retain the claim log's full u128 timestamp domain in persisted JSON.
    claims: BTreeMap<String, String>,
    local: Option<String>,
}
impl Times {
    fn latest(&self) -> Result<Option<u128>> {
        self.claims
            .values()
            .chain(self.local.iter())
            .map(|time| Ok(time.parse()?))
            .collect::<Result<Vec<u128>>>()
            .map(|times| times.into_iter().max())
    }
}
fn stored(connection: &Connection, key: &str) -> Result<Times> {
    let row: Option<String> = connection
        .query_row(
            "SELECT times FROM local_agent_activity_rows WHERE key=?1",
            [key],
            |r| r.get(0),
        )
        .optional()?;
    row.map(|row| serde_json::from_str(&row))
        .transpose()
        .map(Option::unwrap_or_default)
        .map_err(Into::into)
}
fn save(transaction: &Transaction<'_>, key: &str, before: &Times, next: &Times) -> Result<bool> {
    if before == next {
        return Ok(false);
    }
    transaction.execute(
        "INSERT INTO local_agent_activity_rows VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET times=excluded.times",
        params![key, serde_json::to_string(next)?],
    )?;
    Ok(before.latest()? != next.latest()?)
}

impl View for ActivityView {
    fn definition(&self) -> Definition {
        Definition {
            name: VIEW,
            fingerprint: "agent-activity.v1;arrival-category-max;local-id;content-only;u128",
            kinds: KINDS,
            local_kinds: &["harness.timeline"],
            max_contributions: 2,
        }
    }
    fn create_schema(&self, connection: &Connection) -> Result<()> {
        connection.execute_batch("CREATE TABLE IF NOT EXISTS local_agent_activity_rows(key TEXT PRIMARY KEY, times TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS local_agent_content_latest_index
            ON local_observations(subject,json_extract(body,'$.fields.incarnation_id'),id)
            WHERE kind='harness.timeline' AND json_extract(body,'$.fields.entry_type') IN ('message','content','tool_call','tool_result');")?;
        Ok(())
    }
    fn contributions(
        &self,
        claim: &ClaimRecord,
        _canonical: &canonical::ClaimKey,
    ) -> Result<Vec<Contribution>> {
        // The existing SQL oracle reads nested fields only for messages and timeline.
        let fields = &claim.body["fields"];
        let mut sources = Vec::new();
        let mut source =
            |agent: Option<&str>, incarnation: Option<&str>, register: &str| -> Result<()> {
                if let Some(agent) = agent.filter(|agent| agent.starts_with("agent/")) {
                    sources.push(Contribution {
                        key: dependency_key(agent, incarnation)?,
                        register: register.into(),
                        value: Value::String(claim.accepted_at_unix_ms.to_string()),
                        rank: claim.store_index.to_be_bytes().to_vec(),
                    });
                }
                Ok(())
            };
        match claim.kind.as_str() {
            "work.progress" | "work.submitted" => source(claim.actor.as_deref(), None, "work")?,
            "message.sent" => {
                source(fields["from"].as_str(), None, "sent")?;
                source(fields["to"].as_str(), None, "received")?;
            }
            "harness.timeline"
                if matches!(
                    fields["entry_type"].as_str(),
                    Some("message" | "content" | "tool_call" | "tool_result")
                ) =>
            {
                if let Some(incarnation) = fields["incarnation_id"].as_str() {
                    source(Some(&claim.subject), Some(incarnation), "timeline")?;
                }
            }
            _ => {}
        }
        Ok(sources)
    }
    fn maintain_key(
        &self,
        transaction: &Transaction<'_>,
        key: &str,
        _old: Option<&ClaimRecord>,
        _new: Option<&ClaimRecord>,
    ) -> Result<Option<bool>> {
        decode_key(key)?;
        let before = stored(transaction, key)?;
        let mut next = before.clone();
        let mut statement = transaction.prepare_cached("SELECT register,value FROM ivm_heads WHERE view=?1 AND key=?2 ORDER BY register LIMIT 5")?;
        next.claims.clear();
        for row in statement.query_map(params![VIEW, key], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (register, value) = row?;
            anyhow::ensure!(
                REGISTERS.contains(&register.as_str()),
                "unknown activity register"
            );
            let time: String = serde_json::from_str(&value)?;
            time.parse::<u128>()?;
            next.claims.insert(register, time);
        }
        Ok(Some(save(transaction, key, &before, &next)?))
    }
    fn maintain_local_key(
        &self,
        transaction: &Transaction<'_>,
        key: &str,
        _change: &LocalChange,
    ) -> Result<bool> {
        let (agent, incarnation) = decode_key(key)?;
        let incarnation =
            incarnation.context("local timeline requires an incarnation dependency")?;
        let cut =
            smallclaims::ivm::source_cut(transaction)?.context("activity source unavailable")?;
        // One indexed latest-source seek, including after trim/removal. The capture owner
        // must also refresh this key when a pending source becomes eligible at a new cut.
        let local: Option<i64> = transaction.query_row(
            "SELECT observed_at_unix_ms FROM local_observations
             WHERE subject=?1 AND kind='harness.timeline'
               AND json_extract(body,'$.fields.incarnation_id')=?2
               AND json_extract(body,'$.fields.entry_type') IN ('message','content','tool_call','tool_result')
               AND after_store_index<=?3 ORDER BY id DESC LIMIT 1",
            params![agent,incarnation,cut.projected], |r| r.get(0),
        ).optional()?;
        let before = stored(transaction, key)?;
        let mut next = before.clone();
        next.local = local.map(|time| time.max(0).to_string());
        save(transaction, key, &before, &next)
    }
}

/// Read only inside the caller's authorized snapshot. This partial dependency's token
/// does not certify declaration/person/queue/fault/placement or full card readiness.
pub fn read_activity(
    connection: &Connection,
    views: &Views,
    agent: &str,
    incarnation: Option<&str>,
) -> Result<Option<u128>> {
    let cut = smallclaims::ivm::source_cut(connection)?.context("activity source unavailable")?;
    views.token(connection, VIEW, cut.epoch)?;
    let mut latest = stored(connection, &dependency_key(agent, None)?)?.latest()?;
    if let Some(incarnation) = incarnation {
        latest =
            latest.max(stored(connection, &dependency_key(agent, Some(incarnation))?)?.latest()?);
    }
    Ok(latest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use smallclaims::ivm::{Readiness, SourceCut};
    const ALDER: &str = "agent/grove.alder";
    const BIRCH: &str = "agent/grove.birch";
    struct Fixture {
        store: Store,
        views: Views,
    }
    impl Fixture {
        fn new() -> Self {
            let store = Store::open_memory("grove").unwrap();
            Self::registered(store)
        }
        fn registered(store: Store) -> Self {
            let views = Views::new(definitions()).unwrap();
            let mut connection = store.connection.write();
            let tx = connection.transaction().unwrap();
            views.create_schema(&tx).unwrap();
            views
                .initialize_empty(
                    &tx,
                    SourceCut {
                        epoch: 1,
                        admitted: 0,
                        projected: 0,
                        local_generation: 0,
                    },
                )
                .unwrap();
            tx.commit().unwrap();
            drop(connection);
            Self { store, views }
        }
        fn append(
            &self,
            subject: &str,
            kind: &str,
            actor: Option<&str>,
            fields: Value,
            time: u128,
        ) -> ClaimRecord {
            self.append_from("grove", subject, kind, actor, fields, time)
        }
        fn append_from(
            &self,
            origin: &str,
            subject: &str,
            kind: &str,
            actor: Option<&str>,
            fields: Value,
            time: u128,
        ) -> ClaimRecord {
            self.store.set_write_clock_at(time).unwrap();
            let mut connection = self.store.connection.write();
            let tx = connection.transaction().unwrap();
            // Raw canonical legacy fixtures isolate the card selector from current work/
            // message admission policy; they still run against the real Store schema.
            let claim = smallclaims::store::append_claim_record_tx(
                &tx,
                origin,
                subject,
                kind,
                actor,
                &json!({"fields":fields}),
                &[],
                None,
            )
            .unwrap();
            let key = canonical::claim_key(&tx, &claim.id).unwrap();
            let changes = self
                .views
                .change(&tx, None, Some((&claim, &key)), 1)
                .unwrap();
            assert!(changes.deferred.is_empty());
            let index = current_index(&tx).unwrap();
            let local = smallclaims::ivm::source_cut(&tx)
                .unwrap()
                .unwrap()
                .local_generation;
            self.views
                .publish_cut(
                    &tx,
                    SourceCut {
                        epoch: 1,
                        admitted: index,
                        projected: index,
                        local_generation: local,
                    },
                )
                .unwrap();
            tx.commit().unwrap();
            drop(connection);
            self.check();
            claim
        }
        fn local(&self, agent: &str, incarnation: &str, entry_type: &str, time: i64) -> i64 {
            let mut connection = self.store.connection.write();
            let tx = connection.transaction().unwrap();
            let index = current_index(&tx).unwrap();
            tx.execute("INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms) VALUES(?1,?2,'harness.timeline',?3,?4)",params![index,agent,json!({"fields":{"incarnation_id":incarnation,"entry_type":entry_type}}).to_string(),time]).unwrap();
            let id = tx.last_insert_rowid();
            self.capture_local(&tx, agent, incarnation);
            tx.commit().unwrap();
            drop(connection);
            self.check();
            id
        }
        fn capture_local(&self, tx: &Transaction<'_>, agent: &str, incarnation: &str) {
            let mut cut = smallclaims::ivm::source_cut(tx).unwrap().unwrap();
            cut.local_generation += 1;
            let keys = BTreeSet::from([dependency_key(agent, Some(incarnation)).unwrap()]);
            let changes = self
                .views
                .local_change(
                    tx,
                    &LocalChange {
                        kind: "harness.timeline".into(),
                        old_keys: keys.clone(),
                        new_keys: keys,
                        evaluation_time_unix_ms: 0,
                    },
                    cut,
                )
                .unwrap();
            assert!(changes.deferred.is_empty());
        }
        fn check(&self) {
            let connection = self.store.readers.get();
            for agent in [ALDER, BIRCH] {
                for incarnation in [None, Some("one"), Some("two")] {
                    assert_eq!(
                        read_activity(&connection, &self.views, agent, incarnation).unwrap(),
                        self.store
                            .agent_last_activity_at(agent, incarnation, self.store.index().unwrap())
                            .unwrap(),
                        "{agent} {incarnation:?}"
                    );
                }
            }
        }
        fn generation(&self) -> u64 {
            self.store
                .readers
                .get()
                .query_row(
                    "SELECT generation FROM ivm_views WHERE name=?1",
                    [VIEW],
                    |r| r.get(0),
                )
                .unwrap()
        }
    }
    #[test]
    fn agent_activity_categories_sparse_sources_and_incarnations_match_real_store() {
        let f = Fixture::new();
        f.append(
            ALDER,
            "harness.timeline",
            None,
            json!({"entry_type":"content","incarnation_id":"one"}),
            100,
        );
        f.append(
            ALDER,
            "harness.timeline",
            None,
            json!({"entry_type":"status","incarnation_id":"one"}),
            200,
        );
        f.append(
            ALDER,
            "harness.timeline",
            None,
            json!({"entry_type":"tool_call","incarnation_id":"two"}),
            110,
        );
        f.append(
            "step-run/grove",
            "work.progress",
            Some(ALDER),
            json!({"reason":"progress"}),
            120,
        );
        f.append(
            "step-run/grove",
            "work.claimed",
            Some(ALDER),
            json!({}),
            400,
        );
        f.append(
            "message/grove.one",
            "message.sent",
            None,
            json!({"from":ALDER,"to":BIRCH}),
            140,
        );
        f.append(
            "message/grove.two",
            "message.sent",
            None,
            json!({"from":BIRCH,"to":ALDER}),
            130,
        );
        f.append_from(
            "grove.remote",
            "step-run/grove",
            "work.submitted",
            Some(BIRCH),
            json!({}),
            180,
        );
        f.local(ALDER, "one", "content", 190);
        f.local(ALDER, "two", "tool_result", 160);
        f.local(ALDER, "one", "status", 500);
    }
    #[test]
    fn agent_activity_latest_arrival_is_not_greatest_time_within_category() {
        let f = Fixture::new();
        f.append(
            "step-run/grove",
            "work.progress",
            Some(ALDER),
            json!({}),
            900,
        );
        f.append_from(
            "grove.remote",
            "step-run/grove",
            "work.submitted",
            Some(ALDER),
            json!({}),
            100,
        );
        assert_eq!(
            read_activity(&f.store.readers.get(), &f.views, ALDER, None).unwrap(),
            Some(100)
        );
        f.local(ALDER, "one", "content", 800);
        f.local(ALDER, "one", "message", 200);
        assert_eq!(
            read_activity(&f.store.readers.get(), &f.views, ALDER, Some("one")).unwrap(),
            Some(200)
        );
        f.local(ALDER, "one", "content", -7);
        assert_eq!(
            read_activity(&f.store.readers.get(), &f.views, ALDER, Some("one")).unwrap(),
            Some(100)
        );
    }
    #[test]
    fn agent_activity_duplicates_unrelated_inputs_and_hidden_categories_change_no_output() {
        let f = Fixture::new();
        let claim = f.append(
            "message/grove",
            "message.sent",
            None,
            json!({"from":ALDER,"to":ALDER}),
            900,
        );
        let generation = f.generation();
        f.append(
            "step-run/grove",
            "work.progress",
            Some(ALDER),
            json!({}),
            100,
        );
        f.append(
            BIRCH,
            "harness.timeline",
            None,
            json!({"entry_type":"status","incarnation_id":"one"}),
            1000,
        );
        f.append(
            "step-run/grove",
            "work.progress",
            Some("person/grove"),
            json!({}),
            1100,
        );
        assert_eq!(f.generation(), generation);
        let mut connection = f.store.connection.write();
        let tx = connection.transaction().unwrap();
        let key = canonical::claim_key(&tx, &claim.id).unwrap();
        let changes = f.views.change(&tx, None, Some((&claim, &key)), 1).unwrap();
        assert!(changes.changed.is_empty());
        tx.commit().unwrap();
        drop(connection);
        assert_eq!(f.generation(), generation);
        f.check();
    }
    #[test]
    fn agent_activity_local_trim_and_rollback_preserve_selected_sources() {
        let f = Fixture::new();
        f.local(ALDER, "one", "content", 100);
        let newest = f.local(ALDER, "one", "tool_result", 200);
        let generation = f.generation();
        let before = smallclaims::ivm::source_cut(&f.store.readers.get()).unwrap();
        {
            let mut connection = f.store.connection.write();
            let tx = connection.transaction().unwrap();
            tx.execute("DELETE FROM local_observations WHERE id=?1", [newest])
                .unwrap();
            f.capture_local(&tx, ALDER, "one");
            assert_eq!(
                read_activity(&tx, &f.views, ALDER, Some("one")).unwrap(),
                Some(100)
            );
            tx.rollback().unwrap();
        }
        assert_eq!(f.generation(), generation);
        assert_eq!(
            smallclaims::ivm::source_cut(&f.store.readers.get()).unwrap(),
            before
        );
        f.check();
        {
            let mut connection = f.store.connection.write();
            let tx = connection.transaction().unwrap();
            tx.execute("DELETE FROM local_observations WHERE id=?1", [newest])
                .unwrap();
            f.capture_local(&tx, ALDER, "one");
            tx.commit().unwrap();
        }
        f.check();
        assert_eq!(
            read_activity(&f.store.readers.get(), &f.views, ALDER, Some("one")).unwrap(),
            Some(100)
        );
    }
    #[test]
    fn agent_activity_pending_claims_and_deleted_sources_are_unavailable() {
        let f = Fixture::new();
        let claim = f.append(
            "step-run/grove",
            "work.progress",
            Some(ALDER),
            json!({}),
            100,
        );
        let connection = f.store.connection.write();
        connection
            .execute("DELETE FROM claims WHERE id=?1", [claim.id])
            .unwrap();
        drop(connection);
        assert!(matches!(
            f.views.readiness(&f.store.readers.get(), VIEW, 1).unwrap(),
            Readiness::Fenced
        ));
        assert!(read_activity(&f.store.readers.get(), &f.views, ALDER, None).is_err());
        let g = Fixture::new();
        let mut connection = g.store.connection.write();
        let tx = connection.transaction().unwrap();
        smallclaims::store::append_claim_record_tx(
            &tx,
            "grove",
            "step-run/grove",
            "work.progress",
            Some(ALDER),
            &json!({"fields":{}}),
            &[],
            None,
        )
        .unwrap();
        tx.commit().unwrap();
        drop(connection);
        assert!(matches!(
            g.views.readiness(&g.store.readers.get(), VIEW, 1).unwrap(),
            Readiness::SourcePending
        ));
        assert!(read_activity(&g.store.readers.get(), &g.views, ALDER, None).is_err());
    }
    #[test]
    fn agent_activity_seeded_capture_permutations_and_duplicates_match_real_store() {
        for seed in [1_u64, 7, 23] {
            let f = Fixture::new();
            let mut connection = f.store.connection.write();
            let tx = connection.transaction().unwrap();
            let mut claims = Vec::new();
            for i in 0..48_u64 {
                let time = 100 + (i * 97 % 701);
                tx.execute("DELETE FROM temp.write_clock", []).unwrap();
                tx.execute(
                    "INSERT INTO temp.write_clock(offset_ms,at_ms) VALUES(0,?1)",
                    [time],
                )
                .unwrap();
                let agent = if i & 1 == 0 { ALDER } else { BIRCH };
                let (subject, kind, actor, fields) = match i % 4 {
                    0 => (
                        agent,
                        "harness.timeline",
                        None,
                        json!({"incarnation_id":"one","entry_type":"content"}),
                    ),
                    1 => ("step-run/grove", "work.progress", Some(agent), json!({})),
                    2 => (
                        "message/grove",
                        "message.sent",
                        None,
                        json!({"from":ALDER,"to":BIRCH}),
                    ),
                    _ => ("step-run/grove", "work.submitted", Some(agent), json!({})),
                };
                claims.push(
                    smallclaims::store::append_claim_record_tx(
                        &tx,
                        &format!("grove.{i}"),
                        subject,
                        kind,
                        actor,
                        &json!({"fields":fields}),
                        &[],
                        None,
                    )
                    .unwrap(),
                );
            }
            let mut order = (0..claims.len()).collect::<Vec<_>>();
            let mut random = seed;
            for i in (1..order.len()).rev() {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                order.swap(i, (random % (i as u64 + 1)) as usize);
            }
            // Certify only after the complete explicit fixture prefix, including duplicates.
            for &i in order.iter().chain(order.iter().rev()) {
                let key = canonical::claim_key(&tx, &claims[i].id).unwrap();
                assert!(
                    f.views
                        .change(&tx, None, Some((&claims[i], &key)), 1)
                        .unwrap()
                        .deferred
                        .is_empty()
                );
            }
            let index = current_index(&tx).unwrap();
            f.views
                .publish_cut(
                    &tx,
                    SourceCut {
                        epoch: 1,
                        admitted: index,
                        projected: index,
                        local_generation: 0,
                    },
                )
                .unwrap();
            tx.commit().unwrap();
            drop(connection);
            f.check();
        }
    }
    #[test]
    fn agent_activity_content_seek_and_keyed_read_do_not_scan_status_history() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let mut costs = Vec::new();
        for history in [16, 256] {
            let f = Fixture::new();
            f.local(ALDER, "one", "content", 100);
            let mut connection = f.store.connection.write();
            {
                let tx = connection.transaction().unwrap();
                for i in 0..history {
                    tx.execute("INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms) VALUES(0,?1,'harness.timeline',?2,?3)",params![ALDER,json!({"fields":{"incarnation_id":"one","entry_type":"status"}}).to_string(),i+200]).unwrap();
                }
                tx.commit().unwrap();
            }
            let vm = Arc::new(AtomicU64::new(0));
            let counter = vm.clone();
            connection.progress_handler(
                1,
                Some(move || {
                    counter.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            );
            {
                let tx = connection.transaction().unwrap();
                f.capture_local(&tx, ALDER, "one");
                tx.commit().unwrap();
            }
            let maintenance = vm.swap(0, Ordering::Relaxed);
            assert_eq!(
                read_activity(&connection, &f.views, ALDER, Some("one")).unwrap(),
                Some(100)
            );
            let read = vm.load(Ordering::Relaxed);
            connection.progress_handler(0, None::<fn() -> bool>);
            drop(connection);
            f.check();
            eprintln!(
                "agent-activity status_history={history} maintenance_vm={maintenance} read_vm={read}"
            );
            costs.push((maintenance, read));
        }
        assert!(
            costs[1].0 <= costs[0].0 + 64,
            "status history leaked into content seek: {costs:?}"
        );
        assert_eq!(
            costs[1].1, costs[0].1,
            "keyed read grew with status history"
        );
    }
    #[test]
    fn agent_activity_plain_reopen_retains_rows_and_uncaptured_writes_stay_pending() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("activity.sqlite3");
        {
            let f = Fixture::registered(Store::open(&path, "grove").unwrap());
            f.append(
                ALDER,
                "harness.timeline",
                None,
                json!({"incarnation_id":"one","entry_type":"content"}),
                100,
            );
            f.local(ALDER, "one", "tool_result", 200);
        }
        let store = Store::open(&path, "grove").unwrap();
        let views = Views::new(definitions()).unwrap();
        assert_eq!(
            read_activity(&store.readers.get(), &views, ALDER, Some("one")).unwrap(),
            Some(200)
        );
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        smallclaims::store::append_claim_record_tx(
            &tx,
            "grove",
            ALDER,
            "harness.timeline",
            None,
            &json!({"fields":{"incarnation_id":"one","entry_type":"content"}}),
            &[],
            None,
        )
        .unwrap();
        tx.commit().unwrap();
        drop(connection);
        assert!(matches!(
            views.readiness(&store.readers.get(), VIEW, 1).unwrap(),
            Readiness::SourcePending
        ));
        assert!(read_activity(&store.readers.get(), &views, ALDER, Some("one")).is_err());
    }
}
