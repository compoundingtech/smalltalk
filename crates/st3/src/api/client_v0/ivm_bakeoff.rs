//! Offline per-subject materialization experiment. Never runs a daemon or background worker.
//! The extra snapshot transaction is deliberately separate from production admission: this
//! measures reducer and persistence cost, not a proposed production consistency boundary.
use super::*;
use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, params};
use std::path::PathBuf;

// Time and presence are explicit captured inputs, not values dropped from parity hashes.
// This is opt-in research-only state; ordinary daemon builds have no clock override.
static CLOCK: OnceLock<(u128, Instant)> = OnceLock::new();
pub(crate) fn captured_now_ms() -> Option<u128> { CLOCK.get().map(|(now,_)| *now) }
pub(crate) fn captured_elapsed(at: Instant) -> Duration {
    CLOCK.get().map_or_else(|| at.elapsed(), |(_,now)| now.saturating_duration_since(at))
}
fn assert_parity(root: &Path, label: &str, expected: &Value, actual: &Value) -> Result<()> {
    if digest(expected)? != digest(actual)? {
        std::fs::write(root.join("failed-parity.json"),serde_json::to_vec_pretty(&json!({
            "label":label,"expected":expected,"actual":actual}))?)?;
        anyhow::bail!("{label} parity failed; see {}/failed-parity.json",root.display());
    }
    Ok(())
}

fn ms(start: Instant) -> f64 { start.elapsed().as_secs_f64() * 1000.0 }
fn digest(value: &Value) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(value)?)))
}
fn rss() -> Result<u64> {
    Ok(std::fs::read_to_string("/proc/self/status")?.lines()
        .find_map(|line| line.strip_prefix("VmRSS:").and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse().ok())).context("VmRSS unavailable")?)
}
fn agents(state: &AppState, selected: Option<&BTreeSet<String>>, previous: &[Value]) -> Result<Vec<Value>> {
    let index = state.store.index()?;
    let mut items = super::super::client_agent_resources_selected(
        &state.store, false, index, selected.map(|names| (names, previous)))?;
    let names = items.iter().filter_map(|v| v["id"].as_str().map(str::to_owned)).collect::<Vec<_>>();
    let observations = state.store.agent_todo_observations_for(&names, index)?;
    let at = super::super::client_snapshot_at(state, index).created_at;
    let host = super::super::client_host_id(state.store.origin());
    for item in &mut items {
        let claims = observations.get(item["id"].as_str().unwrap_or_default());
        item["todo"] = agent_todo_value(claims.and_then(|c| c.get("harness.todo.observed")),
            claims.and_then(|c| c.get("harness.session-file")), item["incarnation_id"].as_str());
        if item["updated_at"] == "" { item["updated_at"] = json!(at); }
        let source = item.as_object_mut().unwrap().remove("_status_source").unwrap_or(Value::Null);
        let harness: Option<crate::model::CurrentHarnessView> = serde_json::from_value(source)?;
        let observation = state.store.seat_observation_at(item["id"].as_str().unwrap(), harness.as_ref(), index, client_now_ms())?;
        item["observation"] = json!(observation);
        if observation == "stale" && item["harness_state"] == "idle" {
            item["harness_state"] = json!("indeterminate");
            if item["state"] == "running" { item["state"] = json!("waiting"); }
        }
        super::super::overlay_delivery_presence(item, &host);
    }
    super::super::overlay_subagents(&state.store, &mut items)?;
    Ok(items)
}
fn oracle(state: &AppState, session: &str, kind: &str, limit: usize) -> Result<Value> {
    let who = ClientSession::local(None).map_err(|e| anyhow::anyhow!(e.message))?;
    if kind == "conversation" {
        // Initial conversation admission captures the resume cursor, then reads the newest
        // 200-item page. A changes request with after=None intentionally has no items.
        std::hint::black_box(conversation_read_now(state, &who, session, None)
            .map_err(|e| anyhow::anyhow!(e.message))?);
    }
    // This is exactly conversation_page's local projection, without spawning or transport.
    timeline_value(state, &super::super::new_client_snapshot(state), &who, session,
        &ClientListQuery { limit: Some(limit), ..Default::default() })
        .map(|v| v.0).map_err(|e| anyhow::anyhow!(e.message))
}
fn schema(db: &Connection) -> Result<()> {
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
        CREATE TABLE IF NOT EXISTS agent_views(id TEXT PRIMARY KEY, ordinal INTEGER NOT NULL, version INTEGER NOT NULL, body TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS agent_views_order ON agent_views(ordinal,id);
        CREATE TABLE IF NOT EXISTS subject_views(subject TEXT NOT NULL, kind TEXT NOT NULL, version INTEGER NOT NULL, body TEXT NOT NULL, PRIMARY KEY(subject,kind));
        CREATE TABLE IF NOT EXISTS dependency(input TEXT NOT NULL, output TEXT NOT NULL, PRIMARY KEY(input,output));
        CREATE TABLE IF NOT EXISTS generations(subject TEXT PRIMARY KEY, generation INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS replay_log(id TEXT PRIMARY KEY, subject TEXT NOT NULL, body TEXT NOT NULL);")?;
    Ok(())
}
fn read_view(db: &Connection, kind: &str, limit: usize, session: &str) -> Result<Value> {
    if kind == "agents" {
        let values = db.prepare_cached("SELECT body FROM agent_views ORDER BY ordinal,id LIMIT ?1")?
            .query_map([limit as i64], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?.into_iter()
            .map(|s| serde_json::from_str(&s)).collect::<serde_json::Result<Vec<Value>>>()?;
        Ok(Value::Array(values))
    } else {
        let text: String = db.query_row("SELECT body FROM subject_views WHERE subject=?1 AND kind=?2", params![session,kind], |r| r.get(0))?;
        Ok(serde_json::from_str(&text)?)
    }
}
fn samples(db_path: &Path, kind: &str, limit: usize, session: &str, expected: &Value) -> Result<Value> {
    // Fresh SQLite connection = cold application cache. No global drop_caches, no live-file eviction.
    let db = Connection::open(db_path)?;
    db.execute_batch("PRAGMA cache_size=-1024; PRAGMA mmap_size=0;")?;
    let start = Instant::now();
    let first = read_view(&db, kind, limit, session)?;
    let wire = serde_json::to_vec(&first)?;
    let cold = ms(start);
    ensure!(digest(&first)? == digest(expected)?, "cold parity failed for {kind}/{limit}");
    let mut warm = Vec::new();
    for _ in 0..20 {
        let start = Instant::now();
        let value = read_view(&db, kind, limit, session)?;
        std::hint::black_box(serde_json::to_vec(&value)?);
        warm.push(ms(start));
        ensure!(digest(&value)? == digest(expected)?, "warm parity failed");
    }
    warm.sort_by(f64::total_cmp);
    Ok(json!({"surface":kind,"limit":limit,"cold_ms":cold,"warm_p50_ms":warm[10],"warm_p95_ms":warm[18],"wire_bytes":wire.len(),"sha256":digest(expected)?}))
}

/// All arguments must refer to owned corpus copies. Does not start the actual daemon.
pub fn run(root: PathBuf, owner: String, native: PathBuf) -> Result<Value> {
    ensure!(root.join("state/claims.sqlite3").is_file(), "copied store required");
    let open = Instant::now();
    let store = Arc::new(Store::open(&root.join("state/claims.sqlite3"), "dev5")?);
    let startup = ms(open);
    let state = AppState { store: store.clone(), notify: Arc::new(Notify::new()), event_notify: watch::channel(0_u64).0,
        node: "dev5".into(), state_dir: root.join("state"), pty_root: root.join("pty"), pty_binary: root.join("unused-pty"),
        fleet_id: None, configured_peers: Vec::new(), client_relay: None, native_session_home: Some(root.join("home")), planner_default: PlannerSpec::default() };
    // Relocate the selected binding with an ordinary accepted claim, never mutate signed input bytes.
    let binding = store.latest_claim(&owner, Some("harness.session-file"))?.context("selected binding absent")?;
    let mut fields: BTreeMap<String,Value> = serde_json::from_value(binding.body["fields"].clone())?;
    fields.insert("path".into(), json!(native));
    store.append_claim(&ClaimInput { subject: owner.clone(), kind: "harness.session-file".into(), actor: binding.actor,
        fields, evidence: vec![], expected_subject: None, idempotency_key: None })?;
    let session = conversation_session_id(&state, &owner).map_err(|e| anyhow::anyhow!(e.message))?;
    super::super::delivery_presence::assess("ivm-clock-initialization","omp");
    CLOCK.set((super::super::client_now_ms(),Instant::now())).map_err(|_|anyhow::anyhow!("one corpus run per process"))?;
    let before_rss = rss()?;
    let start = Instant::now();
    let initial = agents(&state, None, &[])?;
    let baseline_cold = ms(start);
    let mut baseline_warm = Vec::new();
    for _ in 0..3 { let start = Instant::now(); std::hint::black_box(agents(&state,None,&[])?); baseline_warm.push(ms(start)); }
    // Verify our uncached adapter against the public production projection, including overlays.
    let production = super::super::client_agent_resources(&store, false, &super::super::new_client_snapshot(&state).created_at, store.index()?)?;
    assert_parity(&root,"production agents",&json!(production),&json!(initial))?;
    let start = Instant::now(); let timeline = oracle(&state,&session,"timeline",50)?; let timeline_cold = ms(start);
    let start = Instant::now(); let conversation = oracle(&state,&session,"conversation",200)?; let conversation_cold = ms(start);
    let native_items = conversation["items"].as_array().context("items absent")?.iter().filter(|v| v["id"].as_str().is_some_and(|id| id.starts_with("timeline-entry/native-"))).count();
    ensure!(native_items > 0, "selected copied transcript produced no native content");
    let mut native_warm = Vec::new();
    for _ in 0..3 { let start=Instant::now(); std::hint::black_box(oracle(&state,&session,"conversation",200)?); native_warm.push(ms(start)); }
    let mut timeline_warm = Vec::new();
    for _ in 0..3 { let start=Instant::now(); std::hint::black_box(oracle(&state,&session,"timeline",50)?); timeline_warm.push(ms(start)); }
    let snapshot_path = root.join("views.sqlite3");
    ensure!(!snapshot_path.exists(), "fresh snapshot database required");
    let mut db = Connection::open(&snapshot_path)?;
    schema(&db)?;
    let start = Instant::now();
    let tx = db.transaction()?;
    for (ordinal,item) in initial.iter().enumerate() {
        tx.execute("INSERT INTO agent_views VALUES(?1,?2,?3,?4)",params![item["id"].as_str().unwrap(),ordinal as i64,store.index()?,serde_json::to_string(item)?])?;
        tx.execute("INSERT INTO dependency VALUES(?1,?1)",[item["id"].as_str().unwrap()])?;
        tx.execute("INSERT INTO generations VALUES(?1,0)",[item["id"].as_str().unwrap()])?;
    }
    for (kind,value) in [("timeline",&timeline),("conversation",&conversation)] {
        tx.execute("INSERT INTO subject_views VALUES(?1,?2,?3,?4)",params![session,kind,store.index()?,serde_json::to_string(value)?])?;
    }
    tx.execute("INSERT INTO dependency VALUES(?1,?2)",params![owner,session])?;
    tx.commit()?;
    let persist_ms = ms(start);
    let mut reads = Vec::new();
    for limit in [1,50,initial.len()] {
        reads.push(samples(&snapshot_path,"agents",limit,&session,&json!(initial.iter().take(limit).cloned().collect::<Vec<_>>()))?);
    }
    reads.push(samples(&snapshot_path,"timeline",50,&session,&timeline)?);
    reads.push(samples(&snapshot_path,"conversation",200,&session,&conversation)?);
    let after_rss = rss()?;
    // Replay actual runtime claim payloads through the real admission path, in log order.
    // New accepted times/IDs are intentional; this is load replay, not signed replication replay.
    let known: BTreeSet<String> = initial.iter().filter_map(|v| v["id"].as_str().map(str::to_owned)).collect();
    let mut slice = store.claims_for_kind_at("runtime.observed",None,true,256)?.claims;
    slice.retain(|c| known.contains(&c.subject)); slice.truncate(24); slice.reverse();
    ensure!(!slice.is_empty(), "runtime replay slice empty");
    let mut current = initial.clone();
    let mut admission = Vec::new(); let mut maintenance = Vec::new(); let mut affected = 0;
    let mut replay_hashes = Vec::new();
    let mut channels = known.iter().map(|id| (id.clone(),watch::channel(0_u64))).collect::<BTreeMap<_,_>>();
    let mut targeted_wakes = 0;
    for claim in &slice {
        let input = ClaimInput { subject:claim.subject.clone(),kind:claim.kind.clone(),actor:claim.actor.clone(),
            fields:serde_json::from_value(claim.body["fields"].clone())?,evidence:vec![],expected_subject:None,idempotency_key:None };
        let start=Instant::now(); let accepted=store.append_claim(&input)?; admission.push(ms(start));
        let start=Instant::now();
        let outputs = db.prepare_cached("SELECT output FROM dependency WHERE input=?1")?
            .query_map([&claim.subject], |r| r.get::<_,String>(0))?.collect::<rusqlite::Result<BTreeSet<_>>>()?;
        let dirty = outputs.iter().filter(|id| known.contains(*id)).cloned().collect::<BTreeSet<_>>();
        let updated = agents(&state,Some(&dirty),&current)?;
        ensure!(updated.len()==1, "selected runtime projection must retain one current agent");
        let tx=db.transaction()?;
        tx.execute("INSERT INTO replay_log VALUES(?1,?2,?3)",params![accepted.id,accepted.subject,serde_json::to_string(&accepted.body)?])?;
        for item in &updated {
            let body = serde_json::to_string(item)?;
            let changed = tx.execute("UPDATE agent_views SET version=?2,body=?3 WHERE id=?1 AND body<>?3",
                params![item["id"].as_str().unwrap(),store.index()?,body])?;
            affected += changed;
            if changed != 0 {
                tx.execute("UPDATE generations SET generation=generation+1 WHERE subject=?1",[item["id"].as_str().unwrap()])?;
            }
            let old=current.iter_mut().find(|v|v["id"]==item["id"]).context("unknown subject")?; *old=item.clone();
        }
        if outputs.contains(&session) {
            for kind in ["timeline","conversation"] {
                let value=oracle(&state,&session,kind,if kind=="timeline" {50} else {200})?;
                tx.execute("UPDATE subject_views SET version=?3,body=?4 WHERE subject=?1 AND kind=?2",params![session,kind,store.index()?,serde_json::to_string(&value)?])?;
            }
        }
        tx.commit()?; maintenance.push(ms(start));
        // Only committed durable generations drive subscription notifications.
        for id in &dirty {
            let generation: u64 = db.query_row("SELECT generation FROM generations WHERE subject=?1",[id],|r|r.get(0))?;
            let (sender,receiver)=&channels[id];
            if *receiver.borrow()!=generation {
                sender.send_replace(generation);
                targeted_wakes += 1;
            }
        }
        for (id,(_,receiver)) in &channels {
            ensure!(!receiver.has_changed()? || dirty.contains(id), "unrelated subscriber woken");
        }
        for (_,receiver) in channels.values_mut() { receiver.borrow_and_update(); }
        let all=agents(&state,None,&[])?;
        let materialized=read_view(&db,"agents",initial.len(),&session)?;
        assert_parity(&root,&format!("replay {}",accepted.id),&json!(all),&materialized)?;
        replay_hashes.push(digest(&materialized)?);
    }
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    let mut sorted=maintenance.clone();sorted.sort_by(f64::total_cmp);
    let mut admitted=admission.clone();admitted.sort_by(f64::total_cmp);
    Ok(json!({"origin_main":"00156543f","startup_ms":startup,"agents":initial.len(),"session":session,"native_items":native_items,
        "baseline":{"agents_uncached_cold_ms":baseline_cold,"agents_uncached_warm_ms":baseline_warm,"timeline_first_ms":timeline_cold,"timeline_warm_ms":timeline_warm,"conversation_first_ms":conversation_cold,"conversation_warm_ms":native_warm},
        "persist_ms":persist_ms,"reads":reads,"rss_before_kib":before_rss,"rss_after_kib":after_rss,"snapshot_bytes":std::fs::metadata(snapshot_path)?.len(),
        "replay":{"claims":slice.len(),"admission_ms":admission,"maintenance_ms":maintenance,"admission_p50_ms":admitted[admitted.len()/2],"maintenance_p50_ms":sorted[sorted.len()/2],"maintenance_p95_ms":sorted[(sorted.len()-1)*95/100],"affected_agent_rows":affected,"targeted_wakes":targeted_wakes,"global_wake_deliveries_for_same_subscribers":slice.len()*known.len(),"parity_sha256":replay_hashes,"source_claim_ids":slice.iter().map(|c|&c.id).collect::<Vec<_>>()},
        "limits":"Cold means new SQLite reader, not cold ZFS ARC. Native first read follows Store::open and binding relocation. Page envelopes persisted only for selected session. Volatile clock/delivery overlays are frozen at measurement; production needs expiry invalidations. Maintenance transaction is separate from real admission; no claim of atomic production integration or replication correctness."}))
}
