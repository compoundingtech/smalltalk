//! Isolated selector evidence. All writes are to fresh temporary stores or a
//! temporary copy of an explicitly supplied, checkpointed offline database.
use super::*;
use smallclaims::hash::claim_id_is_content_hash;
use smallclaims::store::append_claim_record_tx;
use std::hint::black_box;

const LIMIT: usize = 20;
const SAMPLES: usize = 5;
const BASE_TIME: u128 = 1_800_000_000_000;
const PERSON: &str = "person/selector-alex";
const ACTOR: &str = "agent/selector-narrow";
const METADATA: usize = 32;

#[derive(Clone, Copy)]
struct Case {
    name: &'static str,
    person: Option<&'static str>,
    actor: Option<&'static str>,
    history: bool,
}

const CASES: [Case; 5] = [
    Case { name: "actor_history", person: None, actor: Some(ACTOR), history: true },
    Case { name: "broad_history", person: None, actor: None, history: true },
    Case { name: "person_history", person: Some(PERSON), actor: None, history: true },
    Case { name: "broad_open", person: None, actor: None, history: false },
    Case { name: "person_open", person: Some(PERSON), actor: None, history: false },
];

fn subject(index: usize) -> String {
    format!("message/selector-{index:05}")
}

fn parties(index: usize) -> (&'static str, &'static str) {
    (if index.is_multiple_of(3) { ACTOR } else { "agent/selector-sender" },
     if index.is_multiple_of(2) { PERSON } else { "agent/selector-recipient" })
}

fn sent_fields(index: usize) -> BTreeMap<String, Value> {
    let (from, to) = parties(index);
    BTreeMap::from([
        ("from".into(), json!(from)), ("to".into(), json!(to)),
        ("content".into(), json!(format!("selector fixture {index}: λ\nsecond line"))),
        ("status".into(), json!("sent")),
        ("title".into(), json!(format!("selector title {index}"))),
        ("session_id".into(), json!(format!("session/selector-{index}"))),
    ])
}

fn storage(path: &Path) -> Value {
    let bytes = |suffix: &str| {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        match fs::metadata(Path::new(&name)) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => panic!("storage measurement: {error}"),
        }
    };
    json!({"database_bytes": bytes(""), "wal_bytes": bytes("-wal"), "shm_bytes": bytes("-shm")})
}

fn distribution(samples: &mut [Duration]) -> Value {
    assert!(!samples.is_empty());
    samples.sort_unstable();
    let percentile = |p: usize| samples[(samples.len() * p).div_ceil(100) - 1].as_secs_f64() * 1_000.0;
    json!({"samples": samples.len(), "p50_ms": percentile(50), "p95_ms": percentile(95)})
}

fn seed_bulk(store: &Store, count: usize, closed: usize, metadata_per_message: usize) -> Value {
    assert!(closed <= count);
    store.client_messages_selector_benchmark_set_enabled(false).unwrap();
    store.set_write_clock_at(BASE_TIME).unwrap();
    let start = Instant::now();
    let mut final_claim = None;
    {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        // One canonical batch per message/phase bounds the legacy wire-position
        // fallback to at most 32 records, just like ordinary bounded batches.
        // Equal accepted timestamps deliberately exercise the subject tie seek.
        for phase in 0..3 {
            let n = if phase == 2 { closed } else { count };
            for index in 0..n {
                let mut batch = None;
                let repeats = if phase == 1 { metadata_per_message } else { 1 };
                for metadata in 0..repeats {
                    let (kind, actor, fields) = match phase {
                        0 => ("message.sent", parties(index).0, sent_fields(index)),
                        1 => ("publication.operation", "agent/selector-sender", BTreeMap::from([
                            ("operation".into(), json!(format!("selector/{index}/{metadata}"))),
                            ("action".into(), json!("fixture-history")),
                            ("status".into(), json!("accepted")),
                        ])),
                        _ => ("message.closed", parties(index).1,
                              BTreeMap::from([("status".into(), json!("closed"))])),
                    };
                    let last_phase = if metadata_per_message>0 { 1 } else { 0 };
                    if phase==last_phase && index+1==n && metadata+1==repeats {
                        final_claim = Some(ClaimInput {
                            subject: subject(index), kind: kind.into(), actor: Some(actor.into()),
                            fields, evidence: Vec::new(), expected_subject: None, idempotency_key: None,
                        });
                        continue;
                    }
                    let claim = append_claim_record_tx(&tx, "selector-benchmark", &subject(index), kind,
                        Some(actor), &json!({"fields": fields}), &[], batch.as_deref()).unwrap();
                    if batch.is_none() {
                        assert!(claim_id_is_content_hash(&claim).unwrap());
                        batch = Some(claim.batch_id.clone());
                    }
                    if index + 1 == n && metadata + 1 == repeats {
                        assert!(claim_id_is_content_hash(&claim).unwrap());
                    }
                }
            }
            eprintln!("{}",json!({"benchmark":"client_messages_selector_fixture_phase","phase":phase,
                "messages":count,"elapsed_ms":start.elapsed().as_secs_f64()*1000.0}));
        }
        tx.commit().unwrap();
    }
    let append = start.elapsed();
    let start = Instant::now();
    // Native message_index is maintained by authoritative claim insert triggers;
    // these read fixtures have no desired declarations or other graph aggregates.
    // A final real append publishes the committed index without an unrelated
    // full graph replay. Writer throughput is measured separately, never here.
    store.append_claim(&final_claim.unwrap()).unwrap();
    let projection = start.elapsed();
    store.client_messages_selector_benchmark_set_enabled(true).unwrap();
    let start = Instant::now();
    store.client_messages_selector_benchmark_rebuild().unwrap();
    let selectors = start.elapsed();
    {
        let reader = store.readers.get();
        let claims: usize = reader.query_row("SELECT COUNT(*) FROM claims", [], |row| row.get(0)).unwrap();
        let indexed: usize = reader.query_row("SELECT COUNT(*) FROM message_index WHERE created_index>0", [], |row| row.get(0)).unwrap();
        let closed_indexed: usize = reader.query_row("SELECT COUNT(*) FROM message_index WHERE closed=1", [], |row| row.get(0)).unwrap();
        let headers: usize = reader.query_row(
            "SELECT COUNT(*) FROM local_client_message_selectors_v1 WHERE born_index>=0 AND retired_index IS NULL",
            [], |row| row.get(0)).unwrap();
        assert_eq!(claims, count * (metadata_per_message + 1) + closed);
        assert_eq!(indexed, count, "fixture must have complete message_index");
        assert_eq!(closed_indexed, closed);
        assert_eq!(headers, count, "fixture must have one complete current selector header per indexed message");
    }
    // Fixtures must actually contain canonical bodies, not just synthetic selector
    // rows that accidentally make a timing pass. Sample both ends of each phase.
    for index in [0, count / 2, count - 1] {
        let message = store.message(&subject(index)).unwrap().unwrap();
        assert_eq!(message.from, parties(index).0);
        assert_eq!(message.to, parties(index).1);
        assert_eq!(message.content, sent_fields(index)["content"].as_str().unwrap());
        let claims = store.claims_for(&subject(index), None).unwrap();
        assert_eq!(claims.len(), metadata_per_message + 1 + usize::from(index < closed));
        assert!(claims.iter().all(|claim| claim_id_is_content_hash(claim).unwrap()));
    }
    json!({"bulk_append_single_transaction_ms": append.as_secs_f64() * 1_000.0,
        "final_real_append_index_publication_ms": projection.as_secs_f64() * 1_000.0,
        "explicit_selector_header_backfill_ms": selectors.as_secs_f64() * 1_000.0,
        "claims": count * (metadata_per_message + 1) + closed, "messages": count,
        "metadata_claims_per_message": metadata_per_message, "closed_messages": closed})
}

struct Page {
    items: Vec<Value>,
    subjects: Vec<String>,
    key: Option<(u128, String)>,
    more: bool,
}

fn page(store: &Store, case: Case, cut: u64, after: Option<&(u128, String)>) -> Page {
    let mut rows = store.read_snapshot(|_| {
        store.client_messages_page(case.person, case.actor, case.history, cut, after, LIMIT)
    }).unwrap();
    let more = rows.len() > LIMIT;
    rows.truncate(LIMIT);
    let key = rows.last().map(|(message, metadata, _)|
        (metadata["sent_at"].as_str().unwrap().parse().unwrap(), message.subject.clone()));
    let subjects = rows.iter().map(|(message, _, _)| message.subject.clone()).collect();
    let items = rows.into_iter().map(|(message, metadata, current)| {
        client_message_resource(message, &metadata, current, BASE_TIME + 100_000).unwrap()
    }).collect::<Vec<_>>();
    black_box(serde_json::to_vec(&items).unwrap());
    Page { items, subjects, key, more }
}

fn expected_subjects(count: usize, closed: usize, case: Case) -> Vec<String> {
    (0..count).filter(|index| {
        let (from, to) = parties(*index);
        case.person.is_none_or(|person| to == person)
            && case.actor.is_none_or(|actor| from == actor || to == actor)
            && (case.history || *index >= closed)
    }).map(subject).collect()
}

fn eligible_indexes(store: &Store, case: Case) -> Vec<String> {
    let reader = store.readers.get();
    let mut statement = reader.prepare(
        "SELECT name,sql FROM sqlite_master WHERE type='index' AND tbl_name='local_client_message_selectors_v1' AND sql IS NOT NULL",
    ).unwrap();
    let flag = if case.person.is_some() { "recipient_current=1" } else { "global_current=1" };
    statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).unwrap()
        .collect::<rusqlite::Result<Vec<_>>>().unwrap().into_iter().filter_map(|(name, sql)| {
            let normalized = sql.to_ascii_lowercase().chars().filter(|c| !c.is_ascii_whitespace()).collect::<String>();
            normalized.split_once("where").is_some_and(|(_, predicate)| predicate.contains(flag)).then_some(name)
        }).collect()
}

fn measure_case(store: &Store, count: usize, closed: usize, case: Case) -> bool {
    let cut = store.index().unwrap();
    let expected = expected_subjects(count, closed, case);
    assert!(expected.len() > LIMIT * 2, "fixture needs first and next pages plus sentinel");
    let start = Instant::now();
    let first = page(store, case, cut, None);
    let first_observed = start.elapsed();
    assert!(first.more);
    assert_eq!(first.subjects, expected[..LIMIT]);
    let key = first.key.unwrap();
    let oracle = (count == 128 && closed == 0).then(||
        client_message_resources_at(store, case.person, case.history, case.actor, BASE_TIME + 100_000).unwrap());
    let mut within_target = true;
    let eligible = if closed > 0 { eligible_indexes(store, case) } else { Vec::new() };
    assert!(closed == 0 || !eligible.is_empty(), "closed-heavy {} needs a partial eligibility index", case.name);
    for (phase, offset, after) in [("first", 0, None), ("next", LIMIT, Some(&key))] {
        let observed = page(store, case, cut, after);
        assert!(observed.more);
        assert_eq!(observed.subjects, expected[offset..offset + LIMIT]);
        if let Some(oracle) = &oracle {
            assert_eq!(observed.items, oracle[offset..offset + LIMIT], "full canonical oracle/{}/{phase}", case.name);
        }
        for _ in 0..3 { black_box(page(store, case, cut, after)); }
        let mut samples = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let start = Instant::now();
            black_box(page(store, case, cut, after));
            samples.push(start.elapsed());
        }
        let timing = distribution(&mut samples);
        let plan = store.client_messages_selector_benchmark_plan(case.person, case.actor, case.history, after.is_some()).unwrap();
        if closed > 0 {
            assert!(plan.iter().any(|line| eligible.iter().any(|index| line.contains(index))
                && (after.is_none() || line.contains("SEARCH"))),
                "closed-heavy {}/{phase} must use the ordered current eligible partial index (and seek continuations): {plan:?}", case.name);
        }
        eprintln!("{}", json!({"benchmark": "client_messages_selector_page", "messages": count,
            "scale": count / 128, "closed_messages": closed,
            "case": case.name, "phase": phase, "limit": LIMIT, "cut": cut,
            "snapshot_page_resource_json_wall": timing, "query_plan": plan,
            "first_observed_case_page_ms": first_observed.as_secs_f64() * 1_000.0,
            "os_cache_evicted": false}));
        within_target &= timing["p95_ms"].as_f64().unwrap() < 100.0;
    }
    within_target
}

#[test]
#[ignore = "isolated selector 1x/10x/100x read evidence; no live databases"]
fn benchmark_client_messages_selector_scaling() {
    let mut largest_within_target = true;
    for count in [128, 1_280, 12_800] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("selector-scaling.sqlite");
        let store = Store::open(&path, "selector-benchmark").unwrap();
        store.client_messages_benchmark_prepare_io().unwrap();
        let fixture = seed_bulk(&store, count, 0, METADATA);
        store.client_messages_benchmark_checkpoint().unwrap();
        eprintln!("{}", json!({"benchmark": "client_messages_selector_fixture", "fixture": fixture,
            "storage": storage(&path)}));
        for case in CASES {
            let within_target = measure_case(&store, count, 0, case);
            if count == 12_800 { largest_within_target &= within_target; }
        }
    }
    assert!(largest_within_target, "all 100x first/next page p95 values must be <100ms; see emitted distributions");
}

#[test]
#[ignore = "isolated closed-heavy eligible range-seek evidence"]
fn benchmark_client_messages_selector_closed_heavy() {
    let count = 12_800;
    let closed = count - 128;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("selector-closed-heavy.sqlite");
    let store = Store::open(&path, "selector-benchmark").unwrap();
    store.client_messages_benchmark_prepare_io().unwrap();
    let fixture = seed_bulk(&store, count, closed, 0);
    store.client_messages_benchmark_checkpoint().unwrap();
    eprintln!("{}", json!({"benchmark": "client_messages_selector_closed_fixture", "fixture": fixture,
        "storage": storage(&path), "closed_rows_before_first_eligible": closed}));
    let mut within_target = true;
    for case in CASES.into_iter().filter(|case| !case.history) {
        within_target &= measure_case(&store, count, closed, case);
    }
    assert!(within_target, "all closed-heavy first/next page p95 values must be <100ms");
}

fn commit_claim(store: &Store, index: usize, kind: &str, clock: usize) -> Duration {
    store.set_write_clock_at(BASE_TIME + u128::try_from(clock).unwrap()).unwrap();
    let input = ClaimInput {
        subject: subject(index), kind: kind.into(),
        actor: Some(if kind == "message.sent" { parties(index).0 } else { parties(index).1 }.into()),
        fields: if kind == "message.sent" { sent_fields(index) }
                else { BTreeMap::from([("status".into(), json!(kind.strip_prefix("message.").unwrap()))]) },
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    };
    let start = Instant::now();
    store.append_claim(&input).unwrap();
    start.elapsed()
}

#[test]
#[ignore = "isolated real send/close commit and WAL comparison against source-only baseline"]
fn benchmark_client_messages_selector_write_bursts() {
    const BURST: usize = 128;
    // Reverse the order on round two to expose warm-machine/order bias. Each
    // design has a fresh store; selector disabling is per Store, never global.
    for (round, designs) in [[false, true], [true, false]].into_iter().enumerate() {
        let mut results = BTreeMap::new();
        for enabled in designs {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("selector-write.sqlite");
            let store = Store::open(&path, "selector-benchmark").unwrap();
            store.client_messages_selector_benchmark_set_enabled(enabled).unwrap();
            store.client_messages_benchmark_prepare_io().unwrap();
            let before = storage(&path);
            let mut classes = serde_json::Map::new();
            let mut final_views = Vec::new();
            for (kind, phase) in [("message.sent", 0), ("message.closed", 1)] {
                if phase==1 {
                    // Close is legal only after delivered/read. Setup commits are
                    // real but outside the close distribution and its WAL segment.
                    for index in 0..BURST {
                        commit_claim(&store,index,"message.delivered",BURST+index*2);
                        commit_claim(&store,index,"message.read",BURST+index*2+1);
                    }
                }
                store.client_messages_benchmark_checkpoint().unwrap();
                let mut samples = Vec::with_capacity(BURST);
                for index in 0..BURST {
                    samples.push(commit_claim(&store, index, kind, if phase==0 {index} else {3*BURST+index}));
                }
                let total = samples.iter().copied().sum::<Duration>();
                let wal = storage(&path)["wal_bytes"].as_u64().unwrap();
                let timing = distribution(&mut samples);
                classes.insert(kind.into(), json!({"commit_inclusive": timing,
                    "burst_commits_wall_ms": total.as_secs_f64() * 1_000.0,
                    "wal_bytes": wal, "wal_bytes_per_commit": wal as f64 / BURST as f64}));
                for index in 0..BURST {
                    let view = store.message(&subject(index)).unwrap().unwrap();
                    assert_eq!(view.content, sent_fields(index)["content"].as_str().unwrap());
                    assert_eq!(view.status, if phase == 0 { "sent" } else { "closed" });
                    final_views.push(serde_json::to_value(view).unwrap());
                }
            }
            let claims: usize = store.readers.get().query_row("SELECT COUNT(*) FROM claims", [], |row| row.get(0)).unwrap();
            assert_eq!(claims, BURST * 4);
            store.client_messages_benchmark_checkpoint().unwrap();
            let result = json!({"benchmark": "client_messages_selector_write", "round": round,
                "selector_maintenance": enabled, "commits_per_burst": BURST, "classes": classes,
                "before_storage": before, "checkpointed_storage": storage(&path),
                "unmeasured_delivered_read_setup_commits": BURST*2,
                "automatic_checkpoint_disabled": true, "synchronous": "Store production default"});
            eprintln!("{result}");
            results.insert(enabled, (result, final_views));
        }
        assert_eq!(results[&false].1, results[&true].1, "selector maintenance must not change authoritative message views");
        for kind in ["message.sent", "message.closed"] {
            let baseline = &results[&false].0["classes"][kind];
            let selectors = &results[&true].0["classes"][kind];
            eprintln!("{}", json!({"benchmark": "client_messages_selector_write_delta", "round": round,
                "kind": kind,
                "p95_ms_delta": selectors["commit_inclusive"]["p95_ms"].as_f64().unwrap() - baseline["commit_inclusive"]["p95_ms"].as_f64().unwrap(),
                "burst_wall_ms_delta": selectors["burst_commits_wall_ms"].as_f64().unwrap() - baseline["burst_commits_wall_ms"].as_f64().unwrap(),
                "wal_bytes_delta": i128::from(selectors["wal_bytes"].as_u64().unwrap()) - i128::from(baseline["wal_bytes"].as_u64().unwrap())}));
        }
    }
}

fn offline_source_counts(path: &Path) -> (u64, u64) {
    assert_eq!(storage(path)["wal_bytes"].as_u64().unwrap(), 0,
        "migration input must be a checkpointed OFFLINE COPY, never a live/WAL database");
    let connection = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    connection.query_row("SELECT (SELECT COUNT(*) FROM claims), (SELECT COUNT(*) FROM message_index)", [],
        |row| Ok((row.get(0)?, row.get(1)?))).unwrap()
}

#[test]
#[ignore = "requires ST3_CLIENT_MESSAGES_SELECTOR_MIGRATION_COPY pointing to checkpointed offline 118k-claim copy"]
fn benchmark_client_messages_selector_migration_copy() {
    let source = std::env::var_os("ST3_CLIENT_MESSAGES_SELECTOR_MIGRATION_COPY")
        .map(std::path::PathBuf::from).expect("set explicit offline copy path; this benchmark never discovers or opens a daemon database");
    let source = source.canonicalize().unwrap();
    let source_counts = offline_source_counts(&source);
    assert!((118_000..119_000).contains(&source_counts.0), "expected the supplied 118k-claim copy, got {source_counts:?}");
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("selector-migration-copy.sqlite");
    fs::copy(&source, &path).unwrap();
    assert_eq!(offline_source_counts(&path), source_counts);
    let before = storage(&path);
    let start = Instant::now();
    let store = Store::open(&path, "selector-migration-benchmark").unwrap();
    let migration = start.elapsed();
    let live = storage(&path);
    let counts: (u64, u64) = store.readers.get().query_row(
        "SELECT (SELECT COUNT(*) FROM claims), (SELECT COUNT(*) FROM message_index)", [],
        |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
    assert_eq!(counts, source_counts, "migration cannot alter claim authority or message coverage");
    // Explicit rebuild separately isolates selector/header work from other Store
    // startup migrations. Both costs and their WAL are reported, not conflated.
    store.client_messages_benchmark_prepare_io().unwrap();
    let start = Instant::now();
    store.client_messages_selector_benchmark_rebuild().unwrap();
    let rebuild = start.elapsed();
    let rebuilt_live = storage(&path);
    store.client_messages_benchmark_checkpoint().unwrap();
    let checkpointed = storage(&path);
    let cut = store.index().unwrap();
    let case = CASES[1];
    let first = page(&store, case, cut, None);
    assert!(!first.items.is_empty(), "backfilled copy must serve real canonical message headers");
    drop(store);
    let start = Instant::now();
    let reopened = Store::open(&path, "selector-migration-benchmark").unwrap();
    let reopen = start.elapsed();
    assert_eq!(page(&reopened, case, cut, None).items, first.items, "backfill must remain complete across reopen");
    assert_eq!(offline_source_counts(&source), source_counts, "source copy remains read-only and unchanged");
    eprintln!("{}", json!({"benchmark": "client_messages_selector_migration", "source_copy": source,
        "claims": source_counts.0, "messages": source_counts.1,
        "open_including_backfill_ms": migration.as_secs_f64() * 1_000.0,
        "explicit_selector_header_rebuild_ms": rebuild.as_secs_f64() * 1_000.0,
        "populated_reopen_ms": reopen.as_secs_f64() * 1_000.0,
        "before_storage": before, "after_open_live_storage": live,
        "after_explicit_rebuild_live_storage": rebuilt_live, "checkpointed_storage": checkpointed,
        "source_open_flags": "READ_ONLY", "mutated_database": "fresh temporary copy only"}));
}

#[test]
fn client_messages_selector_non_lifecycle_status_does_not_churn_headers() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("claims.sqlite"), "selector-field-regression").unwrap();
    for (kind, fields) in [
        ("message.sent", sent_fields(0)),
        ("publication.operation", BTreeMap::from([
            ("operation".into(), json!("selector-status-regression")),
            ("action".into(), json!("fixture-history")),
            ("status".into(), json!("accepted")),
        ])),
    ] {
        store.append_claim(&ClaimInput {
            subject: subject(0), kind: kind.into(), actor: Some(ACTOR.into()), fields,
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
        let versions: u64 = store.readers.get().query_row(
            "SELECT COUNT(*) FROM local_client_message_selectors_v1 WHERE born_index>=0",
            [], |row|row.get(0)).unwrap();
        assert_eq!(versions,1,"non-selection metadata must not version selector headers");
        for case in CASES {
            let actual = page(&store, case, store.index().unwrap(), None).items;
            let expected = client_message_resources_at(&store, case.person, case.history, case.actor,
                BASE_TIME + 100_000).unwrap();
            assert_eq!(actual, expected, "{}", case.name);
        }
    }
}
