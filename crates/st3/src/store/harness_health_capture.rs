//! Source-only health capture proposal. Callers borrow the card fold's snapshot
//! connection. These rows are candidates, never an admitted native capability.
//! No caller or public positive-health selection is installed by this module.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use rusqlite::{Connection, Statement, StatementStatus, params};
use serde_json::Value;

const CARDS_PER_CAPTURE: usize = 32;
const MAIL_CANDIDATES: usize = 32;
const MAX_METADATA_BYTES: usize = 16 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_MAIL_BYTES: usize = 4 * 1024;
const MAX_KEY_BYTES: usize = 1_024;
const MAX_KIND_BYTES: usize = 128;
const MAX_TIME_BYTES: usize = 20;
const MAX_CAPTURE_VM_STEPS: u64 = 200_000;

const METADATA_SQL: &str = "SELECT id,subject,kind,origin,actor,store_index,accepted_at_unix_ms,
                    CASE WHEN octet_length(body)<=?3 THEN body END
             FROM claims WHERE id IN (SELECT value FROM json_each(?1)) AND store_index<=?2
               AND octet_length(id)<=?4 AND octet_length(subject)<=?4
               AND octet_length(origin)<=?4 AND (actor IS NULL OR octet_length(actor)<=?4)
               AND octet_length(kind)<=?5 AND octet_length(accepted_at_unix_ms)<=?6";

const OUTPUT_SQL: &str = "SELECT output.id,output.subject,output.kind,output.origin,output.actor,
                    output.store_index,output.accepted_at_unix_ms,
                    CASE WHEN octet_length(output.body)<=?3 THEN output.body END
             FROM json_each(?1) subjects JOIN claims output ON output.store_index=(
                 SELECT store_index FROM claims INDEXED BY claims_subject_kind_index
                 WHERE subject=subjects.value AND kind='harness.output.observed' AND store_index<=?2
                 ORDER BY store_index DESC LIMIT 1)
             WHERE octet_length(output.id)<=?4 AND octet_length(output.subject)<=?4
               AND octet_length(output.origin)<=?4
               AND (output.actor IS NULL OR octet_length(output.actor)<=?4)
               AND octet_length(output.kind)<=?5 AND octet_length(output.accepted_at_unix_ms)<=?6";

const PENDING_SQL: &str = "WITH candidates AS MATERIALIZED (
             SELECT json_extract(scopes.value,'$[0]') recipient,
                    json_extract(scopes.value,'$[2]') runtime_at,
                    CASE WHEN octet_length(sends.subject)<=?5 THEN sends.subject END subject,
                    CASE WHEN octet_length(sends.actor)<=?5 THEN sends.actor END actor,
                    sends.store_index,
                    CASE WHEN octet_length(sends.accepted_at_unix_ms)<=?6
                         THEN sends.accepted_at_unix_ms END accepted_at_unix_ms,
                    CASE WHEN octet_length(sends.body)<=?4 THEN sends.body END bounded_body
             FROM json_each(?1) scopes JOIN claims sends ON sends.store_index IN (
                 SELECT store_index FROM claims INDEXED BY claims_message_to_order_index
                 WHERE kind='message.sent'
                   AND json_extract(body,'$.fields.to')=json_extract(scopes.value,'$[0]')
                   AND store_index>json_extract(scopes.value,'$[1]') AND store_index<=?2
                 ORDER BY store_index DESC LIMIT ?3))
         SELECT recipient,runtime_at,subject,actor,
                CASE WHEN json_type(bounded_body,'$.fields.from')='text'
                     THEN json_extract(bounded_body,'$.fields.from') END sender,
                accepted_at_unix_ms,
                CASE WHEN json_type(bounded_body,'$.fields.tags')='array'
                     THEN json_extract(bounded_body,'$.fields.tags')
                     WHEN json_type(bounded_body,'$.fields.tags') IS NULL THEN NULL
                     ELSE 'invalid-tags' END tags,
                EXISTS(SELECT 1 FROM claims INDEXED BY claims_subject_kind_index
                       WHERE subject=candidates.subject AND kind IN
                           ('message.staged','message.delivered','message.read','message.closed')
                         AND store_index<=?2),
                EXISTS(SELECT 1 FROM claims INDEXED BY claims_subject_kind_index
                       WHERE subject=candidates.subject AND kind='message.sent'
                         AND store_index<candidates.store_index)
         FROM candidates ORDER BY recipient,store_index DESC";

#[derive(Debug)]
struct VmExhausted;

impl std::fmt::Display for VmExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("health-capture-vm-exhausted")
    }
}

impl std::error::Error for VmExhausted {}

/// Counts actual VM work across all three statements, including correlated
/// seeks and materialization. Refuse the entire capture on exhaustion. This is
/// an evidence/result guard, not a replacement for the existing read-budget
/// progress handler: that handler still interrupts SQLite while it is stepping.
struct QueryWork {
    spent: u64,
    limit: u64,
}

impl QueryWork {
    fn check(&self, statement: &Statement<'_>) -> Result<()> {
        smallclaims::read_budget::check()?;
        let steps =
            u64::try_from(statement.get_status(StatementStatus::VmStep)).unwrap_or(u64::MAX);
        if self.spent.saturating_add(steps) > self.limit {
            return Err(VmExhausted.into());
        }
        Ok(())
    }

    fn finish(&mut self, statement: &Statement<'_>) -> Result<()> {
        self.check(statement)?;
        self.spent +=
            u64::try_from(statement.get_status(StatementStatus::VmStep)).unwrap_or(u64::MAX);
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Request<'a> {
    pub subject: &'a str,
    /// Selected at the requested cut by the existing status/desired folds.
    pub runtime_claim: Option<&'a str>,
    pub desired_claim: Option<&'a str>,
    pub harness_claim: Option<&'a str>,
}

#[derive(Clone, Debug)]
pub(crate) struct SourceRow {
    pub id: String,
    pub subject: String,
    pub kind: String,
    pub origin: String,
    pub actor: Option<String>,
    pub index: u64,
    pub accepted_at_ms: u64,
    pub body: Value,
}

#[derive(Clone, Debug)]
pub(crate) struct CandidateOwner {
    pub source: String,
    pub origin: String,
    pub runtime_incarnation: String,
    pub provider_incarnation: String,
    pub ownership_sequence: u64,
    pub driver: String,
    pub component: String,
    /// Candidate only: replicated state history can carry a derived timestamp.
    /// Native owner-bound transition evidence must be selected independently.
    pub reported_since_ms: Option<u64>,
}

#[derive(Clone, Debug)]
pub(crate) struct PendingCandidates {
    /// A bounded subset of fresh, plain, never-offered messages, not a full inbox.
    pub subjects: BTreeSet<String>,
    pub oldest_at_ms: Option<u64>,
    pub examined: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct CardCapture {
    pub namespace: String,
    pub subject: String,
    pub index: u64,
    pub runtime: Option<SourceRow>,
    pub desired: Option<SourceRow>,
    pub harness_candidate: Option<SourceRow>,
    pub owner_candidate: Option<CandidateOwner>,
    pub output_candidate: Option<SourceRow>,
    pub pending_candidates: Option<PendingCandidates>,
    // Deliberately no conversion to SelectedProvider, Native or Pending here.
    // Runtime-authenticated harness.observed metadata alone does not certify the
    // independently current physical provider, output capability or mail policy.
}

/// At most three statements per chunk: <=96 selected metadata rows, <=32 output
/// heads and <=1056 mail candidates, with <=5 indexed existence seeks per mail
/// candidate. JSON decode is separately byte bounded, with cooperative budget
/// checks between statements and rows. Oversized/missing evidence is unavailable.
/// The latter two statements batch indexed seeks across the cards. No Store
/// fallback, snapshot acquisition, mutation, replay, initialization or unbounded
/// history/paging is permitted. The surrounding roster `Store::read_snapshot`
/// begins a transaction and reads its cut before lending this connection. The
/// autocommit guard below rejects loans outside that scope; it cannot validate
/// claim IDs/harness structures selected by some unrelated caller or snapshot.
pub(crate) fn capture(
    connection: &Connection,
    namespace: &str,
    index: u64,
    now_ms: u64,
    requests: &[Request<'_>],
) -> Result<BTreeMap<String, CardCapture>> {
    capture_with_limit(
        connection,
        namespace,
        index,
        now_ms,
        requests,
        MAX_CAPTURE_VM_STEPS,
    )
}

fn capture_with_limit(
    connection: &Connection,
    namespace: &str,
    index: u64,
    now_ms: u64,
    requests: &[Request<'_>],
    limit: u64,
) -> Result<BTreeMap<String, CardCapture>> {
    let mut work = QueryWork { spent: 0, limit };
    let result = capture_with_work(connection, namespace, index, now_ms, requests, &mut work);
    match result {
        Err(error) if error.is::<VmExhausted>() => Ok(BTreeMap::new()),
        result => result,
    }
}

fn capture_with_work(
    connection: &Connection,
    namespace: &str,
    index: u64,
    now_ms: u64,
    requests: &[Request<'_>],
    work: &mut QueryWork,
) -> Result<BTreeMap<String, CardCapture>> {
    smallclaims::read_budget::check()?;
    anyhow::ensure!(
        requests.len() <= CARDS_PER_CAPTURE,
        "health capture chunk exceeds its bound"
    );
    anyhow::ensure!(
        !connection.is_autocommit(),
        "health capture requires the card fold's already pinned read snapshot"
    );
    if namespace.len() > MAX_KEY_BYTES {
        return Ok(BTreeMap::new());
    }
    // Bound input keys before their JSON serialization. Do not clone a supplied
    // CurrentHarnessView: its derived/free-form strings are not this capture's
    // authority. Retain only the selected, width-checked raw claim image below.
    let requests = requests
        .iter()
        .copied()
        .filter(|request| {
            request.subject.len() <= MAX_KEY_BYTES
                && [
                    request.runtime_claim,
                    request.desired_claim,
                    request.harness_claim,
                ]
                .into_iter()
                .flatten()
                .all(|key| key.len() <= MAX_KEY_BYTES)
        })
        .collect::<Vec<_>>();
    let ids = requests
        .iter()
        .flat_map(|request| {
            request
                .runtime_claim
                .into_iter()
                .chain(request.desired_claim)
                .chain(request.harness_claim)
        })
        .collect::<BTreeSet<_>>();
    let mut rows = BTreeMap::new();
    if !ids.is_empty() {
        smallclaims::read_budget::check()?;
        let mut statement = connection.prepare_cached(METADATA_SQL)?;
        statement.reset_status(StatementStatus::VmStep);
        let mut items = statement.query(params![
            serde_json::to_string(&ids)?,
            index,
            MAX_METADATA_BYTES,
            MAX_KEY_BYTES,
            MAX_KIND_BYTES,
            MAX_TIME_BYTES
        ])?;
        while let Some(row) = items.next()? {
            work.check(row.as_ref())?;
            let (id, subject, kind, origin, actor, index, time, body) = (
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, u64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
            );
            let Some((accepted_at_ms, body)) = time
                .parse::<u64>()
                .ok()
                .zip(body.and_then(|body| serde_json::from_str::<Value>(&body).ok()))
            else {
                continue;
            };
            rows.insert(
                id.clone(),
                SourceRow {
                    id,
                    subject,
                    kind,
                    origin,
                    actor,
                    index,
                    accepted_at_ms,
                    body,
                },
            );
        }
        drop(items);
        work.finish(&statement)?;
    }
    // Arrival heads are candidate images only. A qualified selection must check
    // admitted origin/runtime/provider/sequence and native event ordering; it may
    // not infer current ownership or capability from this image's own fields.
    let mut outputs = BTreeMap::new();
    if !requests.is_empty() {
        smallclaims::read_budget::check()?;
        let subjects = requests
            .iter()
            .map(|request| request.subject)
            .collect::<Vec<_>>();
        let mut statement = connection.prepare_cached(OUTPUT_SQL)?;
        statement.reset_status(StatementStatus::VmStep);
        let mut items = statement.query(params![
            serde_json::to_string(&subjects)?,
            index,
            MAX_OUTPUT_BYTES,
            MAX_KEY_BYTES,
            MAX_KIND_BYTES,
            MAX_TIME_BYTES
        ])?;
        while let Some(row) = items.next()? {
            work.check(row.as_ref())?;
            let (id, subject, kind, origin, actor, index, time, body) = (
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, u64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
            );
            let Some((accepted_at_ms, body)) = time
                .parse::<u64>()
                .ok()
                .zip(body.and_then(|body| serde_json::from_str::<Value>(&body).ok()))
            else {
                continue;
            };
            outputs.insert(
                subject.clone(),
                SourceRow {
                    id,
                    subject,
                    kind,
                    origin,
                    actor,
                    index,
                    accepted_at_ms,
                    body,
                },
            );
        }
        drop(items);
        work.finish(&statement)?;
    }
    let running = requests
        .iter()
        .filter_map(|request| {
            let runtime = rows.get(request.runtime_claim?)?;
            let runtime_fields = fields(&runtime.body);
            (runtime.subject == request.subject
                && runtime.kind == "runtime.observed"
                && runtime_fields["status"] == "running"
                && runtime_fields["incarnation_id"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty()))
            .then_some((request.subject, runtime))
        })
        .collect::<BTreeMap<_, _>>();
    let mut pending = pending_candidates(connection, index, now_ms, &running, work)?;
    let mut captures = BTreeMap::new();
    for request in &requests {
        smallclaims::read_budget::check()?;
        let selected = |id: Option<&str>, kind: &str| {
            id.and_then(|id| rows.get(id))
                .filter(|row| row.subject == request.subject && row.kind == kind)
                .cloned()
        };
        let runtime = selected(request.runtime_claim, "runtime.observed");
        let desired = selected(request.desired_claim, "intent.desired");
        let state = selected(request.harness_claim, "harness.observed");
        let owner_candidate = runtime
            .as_ref()
            .zip(state.as_ref())
            .and_then(|(runtime, state)| candidate_owner(runtime, state, request.subject));
        captures.insert(
            request.subject.into(),
            CardCapture {
                namespace: namespace.into(),
                subject: request.subject.into(),
                index,
                runtime,
                desired,
                harness_candidate: state,
                owner_candidate,
                output_candidate: outputs.remove(request.subject),
                pending_candidates: pending.remove(request.subject),
            },
        );
    }
    smallclaims::read_budget::check()?;
    Ok(captures)
}

fn fields(body: &Value) -> &Value {
    body.get("fields").unwrap_or(body)
}

fn candidate_owner(
    runtime: &SourceRow,
    state: &SourceRow,
    subject: &str,
) -> Option<CandidateOwner> {
    let runtime_fields = fields(&runtime.body);
    let state_fields = fields(&state.body);
    let incarnation = runtime_fields["incarnation_id"].as_str()?;
    let provider = state_fields["evidence_incarnation"].as_str()?;
    let sequence = state_fields["ownership_sequence"].as_u64()?;
    if runtime_fields["status"] != "running"
        || incarnation.is_empty()
        || provider.is_empty()
        || sequence == 0
        || state.actor.as_deref() != Some(subject)
        || state.origin != runtime.origin
        || state_fields["incarnation_id"].as_str() != Some(incarnation)
    {
        return None;
    }
    Some(CandidateOwner {
        source: state.id.clone(),
        origin: state.origin.clone(),
        runtime_incarnation: incarnation.into(),
        provider_incarnation: provider.into(),
        ownership_sequence: sequence,
        driver: state_fields["driver"].as_str()?.into(),
        component: state_fields["transport"].as_str()?.into(),
        reported_since_ms: state_fields["observed_since_ms"].as_u64(),
    })
}

fn pending_candidates(
    connection: &Connection,
    index: u64,
    now_ms: u64,
    runtimes: &BTreeMap<&str, &SourceRow>,
    work: &mut QueryWork,
) -> Result<BTreeMap<String, PendingCandidates>> {
    let mut pending = runtimes
        .keys()
        .map(|subject| {
            (
                (*subject).to_owned(),
                PendingCandidates {
                    subjects: BTreeSet::new(),
                    oldest_at_ms: None,
                    examined: 0,
                    truncated: false,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    if runtimes.is_empty() {
        return Ok(pending);
    }
    let scopes = runtimes
        .iter()
        .map(|(subject, runtime)| (*subject, runtime.index, runtime.accepted_at_ms))
        .collect::<Vec<_>>();
    // LIMIT bounds each indexed seek BEFORE receipt/tag/time filtering. The total
    // candidates are bounded by 32 cards * 33 sends; each EXISTS is an indexed
    // subject+kind+cut probe (four receipt kinds plus one earlier-send probe).
    // The bounded output sort fixes per-recipient order. Bound the body in the
    // materialized candidate image BEFORE any JSON extraction or Rust decoding.
    // Direct-column octet_length uses bundled SQLite's byte-length column path,
    // avoiding loading an oversized body merely to cast/count it;
    // an oversized candidate still consumes a slot and cannot count as pending.
    // No MIN, desired expansion, reminder walk, full projection or recursive walk.
    smallclaims::read_budget::check()?;
    let mut statement = connection.prepare_cached(PENDING_SQL)?;
    statement.reset_status(StatementStatus::VmStep);
    let mut items = statement.query(params![
        serde_json::to_string(&scopes)?,
        index,
        MAIL_CANDIDATES + 1,
        MAX_MAIL_BYTES,
        MAX_KEY_BYTES,
        MAX_TIME_BYTES
    ])?;
    while let Some(row) = items.next()? {
        work.check(row.as_ref())?;
        let (
            recipient,
            runtime_at,
            id,
            actor,
            sender,
            time,
            tags,
            offered_or_consumed,
            duplicate_send,
        ) = (
            row.get::<_, String>(0)?,
            row.get::<_, u64>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, bool>(7)?,
            row.get::<_, bool>(8)?,
        );
        let Some(pending) = pending.get_mut(&recipient) else {
            continue;
        };
        pending.examined += 1;
        if pending.examined > MAIL_CANDIDATES {
            pending.truncated = true;
            continue;
        }
        let Some((id, at)) = id.zip(time.and_then(|time| time.parse::<u64>().ok())) else {
            continue;
        };
        let plain = tags.is_none_or(|tags| {
            serde_json::from_str::<Vec<Value>>(&tags).is_ok_and(|tags| tags.is_empty())
        });
        if !plain
            || offered_or_consumed
            || duplicate_send
            || sender.is_none()
            || actor != sender
            || at < runtime_at
            || at > now_ms
            || at <= now_ms.saturating_sub(3_600_000)
        {
            continue;
        }
        if pending.subjects.insert(id) {
            pending.oldest_at_ms = Some(pending.oldest_at_ms.map_or(at, |prior| prior.min(at)));
        }
    }
    drop(items);
    work.finish(&statement)?;
    // Even an exhausted modern-send range omits earlier/recovered/legacy/policy
    // mail. `truncated=false` is NOT inbox completeness and must never be used so.
    smallclaims::read_budget::check()?;
    Ok(pending)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn private_card_extension_retains_raw_candidates_without_fallback_or_admission() {
        use crate::model::CurrentHarnessView;
        let connection = fixture();
        insert(
            &connection,
            "desired-a",
            "agent/a",
            "intent.desired",
            2,
            102,
            json!({}),
        );
        insert(
            &connection,
            "state-a",
            "agent/a",
            "harness.observed",
            3,
            103,
            json!({"incarnation_id":"incarnation-a", "evidence_incarnation":"provider-a",
                "ownership_sequence":1, "driver":"omp", "transport":"omp-channel",
                "observed_since_ms":100}),
        );
        connection
            .execute("UPDATE claims SET actor='agent/a' WHERE id='state-a'", [])
            .unwrap();
        insert(
            &connection,
            "output-a",
            "agent/a",
            "harness.output.observed",
            4,
            104,
            json!({}),
        );
        let harness = CurrentHarnessView {
            state: "idle".into(),
            driver: Some("omp".into()),
            incarnation_id: "incarnation-a".into(),
            transport: Some("omp-channel".into()),
            reason: None,
            blocked_on: None,
            ask: None,
            input_buffer: None,
            exit: None,
            claim: "state-a".into(),
            observed_at_unix_ms: 103,
            since_unix_ms: 100,
        };
        let mut reads = super::super::AgentCardReads {
            index: 10,
            harness: std::collections::HashMap::from([("agent/a".into(), Some(harness))]),
            activity: Default::default(),
            suspensions: Default::default(),
            rollouts: Default::default(),
            health: BTreeMap::new(),
        };
        connection.execute_batch("BEGIN").unwrap();
        reads
            .prepare_health_chunk(
                &connection,
                "namespace-a",
                1_000,
                &[("agent/a", Some("runtime-a"), Some("desired-a"))],
            )
            .unwrap();
        let captured = reads.take_health("agent/a").unwrap();
        assert_eq!(captured.namespace, "namespace-a");
        assert_eq!(captured.subject, "agent/a");
        assert_eq!(captured.index, 10);
        assert_eq!(captured.desired.unwrap().id, "desired-a");
        assert_eq!(captured.harness_candidate.unwrap().id, "state-a");
        assert_eq!(captured.output_candidate.unwrap().id, "output-a");
        let owner = captured.owner_candidate.unwrap();
        assert_eq!(
            (owner.source.as_str(), owner.origin.as_str()),
            ("state-a", "host/a")
        );
        assert_eq!(
            (
                owner.runtime_incarnation.as_str(),
                owner.provider_incarnation.as_str()
            ),
            ("incarnation-a", "provider-a")
        );
        assert_eq!(
            (
                owner.ownership_sequence,
                owner.driver.as_str(),
                owner.component.as_str(),
                owner.reported_since_ms
            ),
            (1, "omp", "omp-channel", Some(100))
        );
        // These are raw candidate values, not an admitted current provider or
        // supported output capability. There is no positive-health conversion.
        assert!(reads.take_health("agent/a").is_none());
    }

    fn plan(connection: &Connection, sql: &str, values: &[&dyn rusqlite::ToSql]) -> String {
        connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(values, |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("; ")
    }

    fn fixture() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(
            "CREATE TABLE claims(id TEXT NOT NULL UNIQUE,subject TEXT,kind TEXT,origin TEXT,
                actor TEXT,store_index INTEGER PRIMARY KEY,accepted_at_unix_ms TEXT,body TEXT);
             CREATE INDEX claims_subject_kind_index ON claims(subject,kind,store_index);
             CREATE INDEX claims_message_to_order_index ON claims(json_extract(body,'$.fields.to'),store_index)
                WHERE kind='message.sent';",
        ).unwrap();
        insert(
            &connection,
            "runtime-a",
            "agent/a",
            "runtime.observed",
            1,
            100,
            json!({
                "status":"running", "incarnation_id":"incarnation-a"
            }),
        );
        connection
    }

    fn insert(
        connection: &Connection,
        id: &str,
        subject: &str,
        kind: &str,
        index: u64,
        at: u64,
        fields: Value,
    ) {
        connection
            .execute(
                "INSERT INTO claims VALUES(?1,?2,?3,'host/a','agent/sender',?4,?5,?6)",
                params![
                    id,
                    subject,
                    kind,
                    index,
                    at.to_string(),
                    json!({"fields":fields}).to_string()
                ],
            )
            .unwrap();
    }

    fn request() -> Request<'static> {
        Request {
            subject: "agent/a",
            runtime_claim: Some("runtime-a"),
            desired_claim: None,
            harness_claim: None,
        }
    }

    fn send(connection: &Connection, id: &str, index: u64, to: &str) {
        insert(
            connection,
            id,
            id,
            "message.sent",
            index,
            100 + index,
            json!({
                "to":to, "from":"agent/sender", "tags":[]
            }),
        );
    }

    #[test]
    fn pending_work_is_bounded_before_receipt_filtering_and_cross_cut_rows_are_absent() {
        let connection = fixture();
        for index in 2..=81 {
            send(&connection, &format!("message/{index}"), index, "agent/a");
        }
        send(&connection, "message/foreign", 82, "agent/b");
        send(&connection, "message/future", 200, "agent/a");
        connection.execute_batch("BEGIN").unwrap();
        let captures = capture(&connection, "namespace-a", 90, 1_000, &[request()]).unwrap();
        let captured = &captures["agent/a"];
        let pending = captured.pending_candidates.as_ref().unwrap();
        assert_eq!(pending.examined, MAIL_CANDIDATES + 1);
        assert!(pending.truncated);
        assert_eq!(pending.subjects.len(), MAIL_CANDIDATES);
        assert!(!pending.subjects.contains("message/foreign"));
        assert!(!pending.subjects.contains("message/future"));
        assert!(
            pending.subjects.iter().all(|id| id
                .strip_prefix("message/")
                .unwrap()
                .parse::<u64>()
                .unwrap()
                >= 50)
        );
        assert!(captured.owner_candidate.is_none());
    }

    #[test]
    fn filtered_candidates_do_not_refill_from_older_history() {
        let connection = fixture();
        for index in 2..=81 {
            let id = format!("message/{index}");
            send(&connection, &id, index, "agent/a");
            if index >= 49 {
                insert(
                    &connection,
                    &format!("receipt/{index}"),
                    &id,
                    "message.staged",
                    100 + index,
                    200 + index,
                    json!({}),
                );
            }
        }
        connection.execute_batch("BEGIN").unwrap();
        let captures = capture(&connection, "namespace-a", 200, 1_000, &[request()]).unwrap();
        let pending = captures["agent/a"].pending_candidates.as_ref().unwrap();
        assert_eq!(pending.examined, MAIL_CANDIDATES + 1);
        assert!(pending.truncated);
        assert!(pending.subjects.is_empty());
        assert_eq!(pending.oldest_at_ms, None);
    }

    #[test]
    fn a_small_exhausted_subset_does_not_claim_full_inbox_coverage() {
        let connection = fixture();
        send(&connection, "message/plain", 2, "agent/a");
        send(&connection, "message/read", 3, "agent/a");
        send(&connection, "message/tags", 4, "agent/a");
        insert(
            &connection,
            "receipt",
            "message/read",
            "message.read",
            5,
            110,
            json!({}),
        );
        connection
            .execute(
                "UPDATE claims SET body=?1 WHERE id='message/tags'",
                [json!({
                    "fields":{"to":"agent/a","from":"agent/sender","tags":["reminder:old"]}
                })
                .to_string()],
            )
            .unwrap();
        connection.execute_batch("BEGIN").unwrap();
        let before = capture(&connection, "namespace-a", 4, 1_000, &[request()]).unwrap();
        assert!(
            before["agent/a"]
                .pending_candidates
                .as_ref()
                .unwrap()
                .subjects
                .contains("message/read")
        );
        let after = capture(&connection, "namespace-a", 5, 1_000, &[request()]).unwrap();
        let pending = after["agent/a"].pending_candidates.as_ref().unwrap();
        assert!(!pending.truncated);
        assert_eq!(pending.subjects, BTreeSet::from(["message/plain".into()]));
        // The type has no complete flag or implicit conversion to health Pending.
        assert_eq!(pending.oldest_at_ms, Some(102));
    }

    #[test]
    fn unpinned_loan_and_oversized_evidence_cannot_supply_pending_candidates() {
        let connection = fixture();
        assert!(
            capture(&connection, "namespace-a", 10, 1_000, &[request()])
                .unwrap_err()
                .to_string()
                .contains("already pinned")
        );
        insert(
            &connection,
            "message/oversized",
            "message/oversized",
            "message.sent",
            2,
            102,
            json!({
                "to":"agent/a", "from":"agent/sender", "tags":[],
                "body":"x".repeat(MAX_MAIL_BYTES)
            }),
        );
        connection.execute_batch("BEGIN").unwrap();
        let captures = capture(&connection, "namespace-a", 10, 1_000, &[request()]).unwrap();
        let pending = captures["agent/a"].pending_candidates.as_ref().unwrap();
        assert_eq!(pending.examined, 1);
        assert!(pending.subjects.is_empty());
        assert_eq!(pending.oldest_at_ms, None);
        assert!(!pending.truncated);
        // A bounded candidate was examined, but its body was unavailable. Neither
        // this empty subset nor an untruncated range establishes inbox coverage.
    }

    #[test]
    fn wide_source_cells_are_unavailable_before_rust_retrieval() {
        let wide = "x".repeat(1024 * 1024);
        for column in ["subject", "kind", "origin", "actor", "accepted_at_unix_ms"] {
            let connection = fixture();
            connection
                .execute(
                    &format!("UPDATE claims SET {column}=?1 WHERE id='runtime-a'"),
                    [&wide],
                )
                .unwrap();
            connection.execute_batch("BEGIN").unwrap();
            let captures = capture(&connection, "namespace-a", 10, 1_000, &[request()]).unwrap();
            assert!(captures["agent/a"].runtime.is_none(), "wide {column}");
            assert!(
                captures["agent/a"].pending_candidates.is_none(),
                "wide {column}"
            );
        }
        for column in ["id", "origin", "actor", "accepted_at_unix_ms"] {
            let connection = fixture();
            insert(
                &connection,
                "output-a",
                "agent/a",
                "harness.output.observed",
                2,
                102,
                json!({}),
            );
            connection
                .execute(
                    &format!("UPDATE claims SET {column}=?1 WHERE store_index=2"),
                    [&wide],
                )
                .unwrap();
            connection.execute_batch("BEGIN").unwrap();
            let captures = capture(&connection, "namespace-a", 10, 1_000, &[request()]).unwrap();
            assert!(
                captures["agent/a"].output_candidate.is_none(),
                "wide {column}"
            );
        }
        for column in ["subject", "actor", "accepted_at_unix_ms"] {
            let connection = fixture();
            send(&connection, "message/wide", 2, "agent/a");
            connection
                .execute(
                    &format!("UPDATE claims SET {column}=?1 WHERE store_index=2"),
                    [&wide],
                )
                .unwrap();
            connection.execute_batch("BEGIN").unwrap();
            let captures = capture(&connection, "namespace-a", 10, 1_000, &[request()]).unwrap();
            let pending = captures["agent/a"].pending_candidates.as_ref().unwrap();
            assert_eq!(pending.examined, 1);
            assert!(pending.subjects.is_empty(), "wide {column}");
        }
        let connection = fixture();
        connection.execute_batch("BEGIN").unwrap();
        let input = Request {
            runtime_claim: Some(&wide),
            ..request()
        };
        assert!(
            capture(&connection, "namespace-a", 10, 1_000, &[input])
                .unwrap()
                .is_empty()
        );
        assert!(
            capture(&connection, &wide, 10, 1_000, &[request()])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn locked_sqlite_byte_length_path_skips_an_overflow_body() {
        // Cargo.lock fixes bundled libsqlite3-sys 0.30.1 to SQLite 3.46.0. If
        // that dependency changes, this control must be reviewed with it.
        assert_eq!(rusqlite::version_number(), 3_046_000);
        let connection = fixture();
        insert(
            &connection,
            "message/wide",
            "message/wide",
            "message.sent",
            2,
            102,
            json!({"to":"agent/a","from":"agent/sender","body":"x".repeat(1024*1024)}),
        );
        let flags = connection
            .prepare("EXPLAIN SELECT octet_length(body) FROM claims WHERE store_index=2")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, i32>(6)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(
            flags
                .iter()
                .any(|(opcode, flags)| opcode == "Column" && flags & 0xc0 == 0xc0),
            "{flags:?}"
        );
        // Make a real wide-cell read fail, after storing it. The byte-length
        // check must still work without transferring that overflow payload.
        let prior = unsafe {
            rusqlite::ffi::sqlite3_limit(
                connection.handle(),
                rusqlite::ffi::SQLITE_LIMIT_LENGTH,
                128 * 1024,
            )
        };
        assert!(
            connection
                .query_row("SELECT body FROM claims WHERE store_index=2", [], |row| row
                    .get::<_, String>(0))
                .is_err()
        );
        connection.execute_batch("BEGIN").unwrap();
        let captures = capture(&connection, "namespace-a", 10, 1_000, &[request()]).unwrap();
        let pending = captures["agent/a"].pending_candidates.as_ref().unwrap();
        assert_eq!(pending.examined, 1);
        assert!(pending.subjects.is_empty());
        unsafe {
            rusqlite::ffi::sqlite3_limit(
                connection.handle(),
                rusqlite::ffi::SQLITE_LIMIT_LENGTH,
                prior,
            );
        }
    }

    #[test]
    fn actual_vm_exhaustion_discards_the_entire_capture() {
        let connection = fixture();
        send(&connection, "message/a", 2, "agent/a");
        connection.execute_batch("BEGIN").unwrap();
        let mut work = QueryWork { spent: 0, limit: 0 };
        let error = capture_with_work(
            &connection,
            "namespace-a",
            10,
            1_000,
            &[request()],
            &mut work,
        )
        .unwrap_err();
        assert!(error.is::<VmExhausted>());
        // Exercise the same wrapper used by capture, with a finite smaller
        // private limit; no production switch or extra progress hook exists.
        let unavailable =
            capture_with_limit(&connection, "namespace-a", 10, 1_000, &[request()], 0).unwrap();
        assert!(unavailable.is_empty());
        assert!(
            capture(&connection, "namespace-a", 10, 1_000, &[request()]).unwrap()["agent/a"]
                .runtime
                .is_some()
        );
    }

    #[test]
    fn correlated_plans_and_vm_work_do_not_walk_retained_history() {
        let connection = fixture();
        let subjects = (0..CARDS_PER_CAPTURE)
            .map(|n| format!("agent/bench/{n}"))
            .collect::<Vec<_>>();
        let ids = (0..CARDS_PER_CAPTURE)
            .map(|n| format!("runtime/bench/{n}"))
            .collect::<Vec<_>>();
        for n in 0..CARDS_PER_CAPTURE {
            insert(
                &connection,
                &ids[n],
                &subjects[n],
                "runtime.observed",
                30_000 + n as u64,
                100,
                json!({"status":"running","incarnation_id":format!("incarnation/{n}")}),
            );
            for slot in 0..=MAIL_CANDIDATES {
                let id = format!("message/current/{n}/{slot}");
                send(
                    &connection,
                    &id,
                    40_000 + (n * (MAIL_CANDIDATES + 1) + slot) as u64,
                    &subjects[n],
                );
            }
        }
        let requests = subjects
            .iter()
            .zip(&ids)
            .map(|(subject, id)| Request {
                subject,
                runtime_claim: Some(id),
                desired_claim: None,
                harness_claim: None,
            })
            .collect::<Vec<_>>();
        let mut costs = Vec::new();
        for n in 2..=20_001 {
            // Old same-recipient mail before the selected runtime floor.
            send(
                &connection,
                &format!("message/history/{n}"),
                n,
                &subjects[n as usize % CARDS_PER_CAPTURE],
            );
            if n != 257 && n != 20_001 {
                continue;
            }
            connection.execute_batch("BEGIN").unwrap();
            let captures =
                capture(&connection, "namespace-a", 50_000, 1_000_000, &requests).unwrap();
            assert_eq!(
                captures.len(),
                CARDS_PER_CAPTURE,
                "VM budget exhausted at history {n}"
            );
            assert!(captures.values().all(|capture| {
                capture.pending_candidates.as_ref().unwrap().examined == MAIL_CANDIDATES + 1
            }));
            let mut total_vm = 0;
            for sql in [METADATA_SQL, OUTPUT_SQL, PENDING_SQL] {
                let statement = connection.prepare_cached(sql).unwrap();
                total_vm += statement.get_status(StatementStatus::VmStep) as u64;
                assert_eq!(statement.get_status(StatementStatus::AutoIndex), 0);
            }
            costs.push(total_vm);
            connection.execute_batch("COMMIT").unwrap();
        }
        let scope = serde_json::to_string(&[(subjects[0].as_str(), 30_000, 100)]).unwrap();
        let selected = serde_json::to_string(&ids).unwrap();
        let output_subjects = serde_json::to_string(&subjects).unwrap();
        let cut = 50_000_u64;
        let mail_limit = MAIL_CANDIDATES + 1;
        let metadata_plan = plan(
            &connection,
            METADATA_SQL,
            &[
                &selected,
                &cut,
                &MAX_METADATA_BYTES,
                &MAX_KEY_BYTES,
                &MAX_KIND_BYTES,
                &MAX_TIME_BYTES,
            ],
        );
        let output_plan = plan(
            &connection,
            OUTPUT_SQL,
            &[
                &output_subjects,
                &cut,
                &MAX_OUTPUT_BYTES,
                &MAX_KEY_BYTES,
                &MAX_KIND_BYTES,
                &MAX_TIME_BYTES,
            ],
        );
        let pending_plan = plan(
            &connection,
            PENDING_SQL,
            &[
                &scope,
                &cut,
                &mail_limit,
                &MAX_MAIL_BYTES,
                &MAX_KEY_BYTES,
                &MAX_TIME_BYTES,
            ],
        );
        assert!(
            metadata_plan.contains("SEARCH claims USING INDEX sqlite_autoindex_claims"),
            "{metadata_plan}"
        );
        assert!(
            output_plan.contains("SEARCH output USING INTEGER PRIMARY KEY"),
            "{output_plan}"
        );
        assert!(
            output_plan.contains("claims_subject_kind_index"),
            "{output_plan}"
        );
        assert!(
            pending_plan.contains("SEARCH sends USING INTEGER PRIMARY KEY"),
            "{pending_plan}"
        );
        assert!(
            pending_plan.contains("claims_message_to_order_index"),
            "{pending_plan}"
        );
        assert!(
            pending_plan.contains("claims_subject_kind_index"),
            "{pending_plan}"
        );
        assert!(
            costs[0] > 0 && costs[1] <= MAX_CAPTURE_VM_STEPS && costs[1] <= costs[0] * 2 + 256,
            "{costs:?}"
        );
        eprintln!(
            "health capture VM small/large={costs:?}; metadata={metadata_plan}; output={output_plan}; pending={pending_plan}"
        );
    }

    #[test]
    fn borrowed_connection_keeps_capture_on_its_original_sqlite_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("capture.sqlite");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE claims(id TEXT NOT NULL UNIQUE,subject TEXT,kind TEXT,
            origin TEXT,actor TEXT,store_index INTEGER PRIMARY KEY,accepted_at_unix_ms TEXT,body TEXT);
            CREATE INDEX claims_subject_kind_index ON claims(subject,kind,store_index);
            CREATE INDEX claims_message_to_order_index ON claims(json_extract(body,'$.fields.to'),store_index)
                WHERE kind='message.sent';").unwrap();
        insert(
            &writer,
            "runtime-a",
            "agent/a",
            "runtime.observed",
            1,
            100,
            json!({"status":"running","incarnation_id":"incarnation-a"}),
        );
        let reader = Connection::open(&path).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        let first = capture(&reader, "namespace-a", 20, 1_000, &[request()]).unwrap();
        assert!(
            first["agent/a"]
                .pending_candidates
                .as_ref()
                .unwrap()
                .subjects
                .is_empty()
        );
        send(&writer, "message/new", 2, "agent/a");
        let pinned = capture(&reader, "namespace-a", 20, 1_000, &[request()]).unwrap();
        assert!(
            pinned["agent/a"]
                .pending_candidates
                .as_ref()
                .unwrap()
                .subjects
                .is_empty()
        );
        reader.execute_batch("COMMIT; BEGIN").unwrap();
        let current = capture(&reader, "namespace-a", 20, 1_000, &[request()]).unwrap();
        assert!(
            current["agent/a"]
                .pending_candidates
                .as_ref()
                .unwrap()
                .subjects
                .contains("message/new")
        );
    }
}
