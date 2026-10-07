//! Real Store admitted-claim -> namespaced mailbox installation controls.
//! Local targeted execution uses the standard Cargo wrapper and target runner.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use smallclaims::{
    ClaimInput, ClaimRecord, Store,
    fleet::MemberKey,
    ivm::{
        Views,
        claim_source::{ClaimFact, ClaimSource},
        install::{Installer, Limits, Mutation, Namespace, Operator, Outcome},
        runtime::ViewRuntime,
    },
    replication::ReplicationInventory,
    store::{Runtime, canonical, claim_from_row},
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const SOURCE: &str = "admitted-mail";
const KINDS: &[&str] = &["message.sent", "message.read", "message.closed"];
struct Mailbox {
    fail: Arc<AtomicBool>,
}
impl Operator for Mailbox {
    fn name(&self) -> &'static str {
        "mailbox"
    }
    fn fingerprint(&self) -> &'static str {
        "retained-mailbox.v1;canonical-winner;read-closed-monotone;plain-no-authority"
    }
    fn source(&self) -> &'static str {
        SOURCE
    }
    fn create_schema(&self, c: &Connection) -> Result<()> {
        c.execute_batch("CREATE TABLE IF NOT EXISTS cm_facts(namespace TEXT,id TEXT,subject TEXT,kind TEXT,rank BLOB,value TEXT,PRIMARY KEY(namespace,id));
            CREATE INDEX IF NOT EXISTS cm_subject_kind ON cm_facts(namespace,subject,kind,rank DESC,id DESC);
            CREATE TABLE IF NOT EXISTS cm_rows(namespace TEXT,subject TEXT,recipient TEXT,body TEXT,unread INTEGER,PRIMARY KEY(namespace,subject));
            CREATE TABLE IF NOT EXISTS cm_counts(namespace TEXT,recipient TEXT,n INTEGER CHECK(n>0),PRIMARY KEY(namespace,recipient));")?;
        Ok(())
    }
    fn apply(&self, tx: &Transaction<'_>, ns: &Namespace, rows: &[Mutation]) -> Result<bool> {
        let mut changed = false;
        for row in rows {
            let fact: ClaimFact = serde_json::from_value(
                row.new
                    .clone()
                    .context("retained claim source requires present fact")?,
            )?;
            ensure!(row.key == fact.id, "fact identity mismatch");
            tx.execute("INSERT INTO cm_facts VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(namespace,id) DO UPDATE SET subject=excluded.subject,kind=excluded.kind,rank=excluded.rank,value=excluded.value",
                params![ns.as_str(),fact.id,fact.subject,fact.kind,fact.rank,serde_json::to_string(&fact.body)?])?;
            ensure!(
                !self.fail.load(Ordering::SeqCst),
                "injected after-fact failure"
            );
            let old: Option<(String, String, bool)> = tx
                .query_row(
                    "SELECT recipient,body,unread FROM cm_rows WHERE namespace=?1 AND subject=?2",
                    params![ns.as_str(), fact.subject],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let sent:Option<String>=tx.query_row("SELECT value FROM cm_facts WHERE namespace=?1 AND subject=?2 AND kind='message.sent' ORDER BY rank DESC,id DESC LIMIT 1",params![ns.as_str(),fact.subject],|r|r.get(0)).optional()?;
            let new = if let Some(sent) = sent {
                let body: Value = serde_json::from_str(&sent)?;
                let unread=!tx.query_row("SELECT EXISTS(SELECT 1 FROM cm_facts WHERE namespace=?1 AND subject=?2 AND kind='message.read') OR EXISTS(SELECT 1 FROM cm_facts WHERE namespace=?1 AND subject=?2 AND kind='message.closed')",params![ns.as_str(),fact.subject],|r|r.get::<_,bool>(0))?;
                Some((
                    body["fields"]["to"]
                        .as_str()
                        .context("mail recipient unsupported")?
                        .to_owned(),
                    body["fields"]["body"]
                        .as_str()
                        .context("mail body unsupported")?
                        .to_owned(),
                    unread,
                ))
            } else {
                None
            };
            if old == new {
                continue;
            }
            if let Some((recipient, _, true)) = old {
                // Delete n=1 first to preserve the positive-count invariant.
                let removed = tx.execute(
                    "DELETE FROM cm_counts WHERE namespace=?1 AND recipient=?2 AND n=1",
                    params![ns.as_str(), recipient],
                )?;
                if removed == 0 {
                    tx.execute(
                        "UPDATE cm_counts SET n=n-1 WHERE namespace=?1 AND recipient=?2",
                        params![ns.as_str(), recipient],
                    )?;
                }
            }
            if let Some((recipient, body, unread)) = new {
                tx.execute("INSERT INTO cm_rows VALUES(?1,?2,?3,?4,?5) ON CONFLICT(namespace,subject) DO UPDATE SET recipient=excluded.recipient,body=excluded.body,unread=excluded.unread",params![ns.as_str(),fact.subject,recipient,body,unread])?;
                if unread {
                    tx.execute("INSERT INTO cm_counts VALUES(?1,?2,1) ON CONFLICT(namespace,recipient) DO UPDATE SET n=n+1",params![ns.as_str(),recipient])?;
                }
            }
            changed = true;
        }
        Ok(changed)
    }
    fn validate_publication(&self, _: &Transaction<'_>, _: &Namespace) -> Result<()> {
        Ok(())
    }
    fn reclaim(&self, tx: &Transaction<'_>, ns: &Namespace, rows: usize) -> Result<bool> {
        let mut remaining = rows;
        for (table, key) in [
            ("cm_facts", "id"),
            ("cm_rows", "subject"),
            ("cm_counts", "recipient"),
        ] {
            let removed=tx.execute(&format!("DELETE FROM {table} WHERE namespace=?1 AND {key} IN (SELECT {key} FROM {table} WHERE namespace=?1 ORDER BY {key} LIMIT ?2)"),params![ns.as_str(),remaining as u64])?;
            remaining -= removed;
        }
        Ok(!tx.query_row("SELECT EXISTS(SELECT 1 FROM cm_facts WHERE namespace=?1) OR EXISTS(SELECT 1 FROM cm_rows WHERE namespace=?1) OR EXISTS(SELECT 1 FROM cm_counts WHERE namespace=?1)",[ns.as_str()],|r|r.get::<_,bool>(0))?)
    }
}
struct Node {
    store: Store,
    runtime: Arc<ViewRuntime>,
    fail: Arc<AtomicBool>,
}
impl Node {
    fn source(&self) -> &ClaimSource {
        self.runtime.claim_source.as_ref().unwrap()
    }
}
fn runtime(fail: Arc<AtomicBool>) -> Arc<ViewRuntime> {
    let installer = Arc::new(Installer::new(vec![Box::new(Mailbox { fail })]).unwrap());
    let source = ClaimSource::new(installer, SOURCE, KINDS).unwrap();
    Arc::new(ViewRuntime::with_claim_source(Views::new(vec![]).unwrap(), source).unwrap())
}
fn node(origin: &str) -> Node {
    let fail = Arc::new(AtomicBool::new(false));
    let runtime = runtime(fail.clone());
    let store = Store::open_memory(origin, runtime.clone()).unwrap();
    Node {
        store,
        runtime,
        fail,
    }
}
fn append(n: &Node, subject: &str, kind: &str, fields: Value) -> ClaimRecord {
    n.store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: None,
            fields: serde_json::from_value(fields).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
}
fn register(n: &Node) {
    let mut writer = n.store.connection.write();
    let tx = writer.transaction().unwrap();
    n.source().register(&tx).unwrap();
    tx.commit().unwrap();
}
fn start(n: &Node) -> String {
    let mut writer = n.store.connection.write();
    let tx = writer.transaction().unwrap();
    let id = n
        .source()
        .installer
        .start(
            &tx,
            "mailbox",
            Limits {
                page_rows: 2,
                page_bytes: 8192,
                pending_rows: 128,
                pending_bytes: 1024 * 1024,
                total_rows: 1024,
                callback_ms: 1000,
                lifetime_ms: 10000,
            },
            10,
        )
        .unwrap();
    tx.commit().unwrap();
    id
}
fn extract_page(n: &Node, id: &str, bytes: usize) -> Result<smallclaims::ivm::install::ScanPage> {
    let reader = n.store.readers.get();
    let tx = reader.unchecked_transaction()?;
    let page = n.source().extract(&tx, id, 2, bytes)?;
    tx.commit()?;
    Ok(page)
}
fn scan(n: &Node, id: &str) {
    for _ in 0..40 {
        let page = {
            let reader = n.store.readers.get();
            let tx = reader.unchecked_transaction().unwrap();
            let page = n.source().extract(&tx, id, 2, 8192).unwrap();
            tx.commit().unwrap();
            page
        };
        let mut writer = n.store.connection.write();
        let tx = writer.transaction().unwrap();
        assert_eq!(
            n.source().installer.scan(&tx, &page, 20).unwrap(),
            Outcome::Progress
        );
        tx.commit().unwrap();
        if page.finished {
            return;
        }
    }
    panic!("scan exceeded fixture bound");
}
fn publish(n: &Node, id: &str) {
    for _ in 0..40 {
        let mut writer = n.store.connection.write();
        let tx = writer.transaction().unwrap();
        let outcome = n.source().installer.catch_up(&tx, id, 30).unwrap();
        tx.commit().unwrap();
        if outcome == Outcome::Published {
            return;
        }
        assert_eq!(outcome, Outcome::Progress);
    }
    panic!("catchup exceeded fixture bound");
}
fn install(n: &Node) -> String {
    let id = start(n);
    scan(n, &id);
    publish(n, &id);
    id
}
type MailboxAnswer = (
    BTreeMap<String, (String, String, bool)>,
    BTreeMap<String, u64>,
);
fn output(n: &Node) -> MailboxAnswer {
    let reader = n.store.readers.get();
    let tx = reader.unchecked_transaction().unwrap();
    let root = n.source().root(&tx, "mailbox").unwrap();
    let rows = tx
        .prepare(
            "SELECT subject,recipient,body,unread FROM cm_rows WHERE namespace=?1 ORDER BY subject",
        )
        .unwrap()
        .query_map([root.namespace.as_str()], |r| {
            Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?)))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let counts = tx
        .prepare("SELECT recipient,n FROM cm_counts WHERE namespace=?1 ORDER BY recipient")
        .unwrap()
        .query_map([root.namespace.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    tx.commit().unwrap();
    (rows, counts)
}
fn oracle(n: &Node) -> MailboxAnswer {
    let c = n.store.readers.get();
    let sql = canonical::canonical_sql(
        "SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM claims ORDER BY CANONICAL_ASC(claims)",
    );
    let mut sent = BTreeMap::<String, (String, String, bool)>::new();
    let mut read = std::collections::BTreeSet::new();
    for claim in c
        .prepare(&sql)
        .unwrap()
        .query_map([], claim_from_row)
        .unwrap()
    {
        let claim = claim.unwrap();
        let f = &claim.body["fields"];
        match claim.kind.as_str() {
            "message.sent" => {
                sent.insert(
                    claim.subject,
                    (
                        f["to"].as_str().unwrap().into(),
                        f["body"].as_str().unwrap().into(),
                        true,
                    ),
                );
            }
            "message.read" | "message.closed" => {
                read.insert(claim.subject);
            }
            _ => {}
        }
    }
    let mut counts = BTreeMap::new();
    for (subject, (recipient, _, unread)) in &mut sent {
        *unread = !read.contains(subject);
        if *unread {
            *counts.entry(recipient.clone()).or_insert(0) += 1;
        }
    }
    (sent, counts)
}

#[test]
fn populated_source_is_installed_only_by_explicit_indexed_pages_with_interleaved_claims() {
    let n = node("alder");
    append(
        &n,
        "message/a",
        "message.sent",
        json!({"to":"person/lichen","body":"first"}),
    );
    append(
        &n,
        "message/b",
        "message.sent",
        json!({"to":"person/blair","body":"second"}),
    );
    assert_eq!(
        n.store
            .readers
            .get()
            .query_row("SELECT COUNT(*) FROM ivm_install_sources", [], |r| r
                .get::<_, u64>(0))
            .unwrap(),
        0
    );
    register(&n);
    let id = start(&n);
    let page = extract_page(&n, &id, 8192).unwrap();
    append(&n, "message/a", "message.read", json!({}));
    append(
        &n,
        "message/c",
        "message.sent",
        json!({"to":"person/lichen","body":"third"}),
    );
    {
        let mut w = n.store.connection.write();
        let tx = w.transaction().unwrap();
        assert_eq!(
            n.source().installer.scan(&tx, &page, 20).unwrap(),
            Outcome::Progress
        );
        tx.commit().unwrap();
    }
    if !page.finished {
        scan(&n, &id);
    }
    publish(&n, &id);
    assert_eq!(output(&n), oracle(&n));
    let before = n.source().root(&n.store.readers.get(), "mailbox").unwrap();
    append(
        &n,
        "message/a",
        "custom.future.unknown",
        json!({"opaque":true}),
    );
    let after = n.source().root(&n.store.readers.get(), "mailbox").unwrap();
    assert_eq!(before, after);
}

#[test]
fn signed_peer_claims_shuffled_and_duplicated_match_raw_mail_history_after_each_projection() {
    let emitter = node("alder");
    let (key, _) = MemberKey::generate().unwrap();
    let public = key.public().to_owned();
    emitter.store.bind_fleet("fixture-fleet").unwrap();
    emitter.store.pin_fleet_anchor(&public).unwrap();
    emitter.store.set_member_key(Some(Arc::new(key))).unwrap();
    emitter
        .store
        .admit_fleet_anchor("fixture-fleet", &public, "listening")
        .unwrap();
    append(
        &emitter,
        "message/a",
        "message.sent",
        json!({"to":"person/lichen","body":"old"}),
    );
    append(&emitter, "message/a", "message.read", json!({}));
    append(
        &emitter,
        "message/b",
        "message.sent",
        json!({"to":"person/blair","body":"other"}),
    );
    append(
        &emitter,
        "message/a",
        "message.sent",
        json!({"to":"person/blair","body":"new"}),
    );
    append(
        &emitter,
        "message/a",
        "custom.future.unknown",
        json!({"opaque":1}),
    );
    let exchange = emitter
        .store
        .export_replication_exchange("fixture-fleet", &ReplicationInventory::default())
        .unwrap();
    for reverse in [false, true] {
        let receiver = node(if reverse { "cedar" } else { "birch" });
        receiver.store.bind_fleet("fixture-fleet").unwrap();
        receiver.store.pin_fleet_anchor(&public).unwrap();
        register(&receiver);
        install(&receiver);
        let mut envelopes = exchange.envelopes.clone();
        envelopes.sort_by_key(|e| e.sequence);
        // Anchor creation is a prerequisite; remaining message envelopes arrive in both orders.
        let anchor = envelopes.remove(0);
        if reverse {
            envelopes.reverse();
        }
        envelopes.insert(0, anchor);
        for envelope in envelopes {
            let mut delivery = exchange.clone();
            delivery.envelopes = vec![envelope];
            receiver
                .store
                .receive_replication_exchange("alder", "fixture-fleet", &delivery)
                .unwrap();
            receiver.store.validate_replication_backlog().unwrap();
            let pending = receiver
                .source()
                .availability(&receiver.store.readers.get(), "mailbox")
                .unwrap();
            let current = smallclaims::store::current_index(&receiver.store.readers.get()).unwrap();
            if pending.cut.unwrap().projected < current {
                assert!(!pending.ready);
                assert!(pending.installation.source_available);
                assert!(
                    receiver
                        .source()
                        .root(&receiver.store.readers.get(), "mailbox")
                        .is_err()
                );
            }
            receiver.store.project_replication_backlog().unwrap();
            assert_eq!(output(&receiver), oracle(&receiver));
            let before = receiver
                .source()
                .root(&receiver.store.readers.get(), "mailbox")
                .unwrap();
            receiver
                .store
                .receive_replication_exchange("alder", "fixture-fleet", &delivery)
                .unwrap();
            receiver.store.validate_replication_backlog().unwrap();
            receiver.store.project_replication_backlog().unwrap();
            assert_eq!(
                before,
                receiver
                    .source()
                    .root(&receiver.store.readers.get(), "mailbox")
                    .unwrap()
            );
        }
        assert_eq!(output(&receiver), oracle(&emitter));
    }
}

#[test]
fn live_callback_failure_preserves_peer_admission_and_explicit_paged_recovery() {
    let emitter = node("alder");
    let (key, _) = MemberKey::generate().unwrap();
    let public = key.public().to_owned();
    emitter.store.bind_fleet("fixture-fleet").unwrap();
    emitter.store.pin_fleet_anchor(&public).unwrap();
    emitter.store.set_member_key(Some(Arc::new(key))).unwrap();
    emitter
        .store
        .admit_fleet_anchor("fixture-fleet", &public, "listening")
        .unwrap();
    let claim = append(
        &emitter,
        "message/a",
        "message.sent",
        json!({"to":"person/lichen","body":"admitted"}),
    );
    let exchange = emitter
        .store
        .export_replication_exchange("fixture-fleet", &ReplicationInventory::default())
        .unwrap();
    let n = node("birch");
    n.store.bind_fleet("fixture-fleet").unwrap();
    n.store.pin_fleet_anchor(&public).unwrap();
    register(&n);
    install(&n);
    n.fail.store(true, Ordering::SeqCst);
    n.store
        .receive_replication_exchange("alder", "fixture-fleet", &exchange)
        .unwrap();
    n.store.validate_replication_backlog().unwrap();
    n.store.project_replication_backlog().unwrap();
    assert!(n.store.claim_by_id(&claim.id).unwrap().is_some());
    assert!(n.source().root(&n.store.readers.get(), "mailbox").is_err());
    assert_eq!(
        n.store
            .readers
            .get()
            .query_row("SELECT COUNT(*) FROM cm_facts", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        0
    );
    n.fail.store(false, Ordering::SeqCst);
    install(&n);
    assert_eq!(output(&n), oracle(&n));
}

#[test]
fn rank_and_deletion_fences_preserve_appends_then_explicit_recovery_recomputes_retained_input() {
    let n = node("alder");
    register(&n);
    install(&n);
    let claim = append(
        &n,
        "message/a",
        "message.sent",
        json!({"to":"person/lichen","body":"kept"}),
    );
    let index = smallclaims::store::current_index(&n.store.readers.get()).unwrap();
    let original: String = n
        .store
        .readers
        .get()
        .query_row("SELECT body FROM claims WHERE id=?1", [&claim.id], |r| {
            r.get(0)
        })
        .unwrap();
    {
        let w = n.store.connection.write();
        w.execute(
            "UPDATE claims SET body=json_set(body,'$.fields.body','changed') WHERE id=?1",
            [&claim.id],
        )
        .unwrap();
    }
    assert!(n.source().root(&n.store.readers.get(), "mailbox").is_err());
    append(
        &n,
        "message/b",
        "message.sent",
        json!({"to":"person/blair","body":"admitted while fenced"}),
    );
    assert!(smallclaims::store::current_index(&n.store.readers.get()).unwrap() > index);
    // Restore the original admitted bytes before attesting coverage. This is private
    // corruption repair, not accepted replicated repair or a new claim-validation policy.
    {
        let w = n.store.connection.write();
        w.execute(
            "UPDATE claims SET body=?2 WHERE id=?1",
            params![claim.id, original],
        )
        .unwrap();
    }
    let expected = n
        .source()
        .installer
        .status(&n.store.readers.get(), "mailbox")
        .unwrap()
        .source;
    {
        let mut w = n.store.connection.write();
        let tx = w.transaction().unwrap();
        n.source().restore(&tx, &expected).unwrap();
        tx.commit().unwrap();
    }
    install(&n);
    assert_eq!(output(&n), oracle(&n));
    {
        let w = n.store.connection.write();
        w.execute("DELETE FROM claims WHERE id=?1", [&claim.id])
            .unwrap();
    }
    assert!(n.source().root(&n.store.readers.get(), "mailbox").is_err());
    let expected = n
        .source()
        .installer
        .status(&n.store.readers.get(), "mailbox")
        .unwrap()
        .source;
    {
        let mut w = n.store.connection.write();
        let tx = w.transaction().unwrap();
        n.source().restore(&tx, &expected).unwrap();
        tx.commit().unwrap();
    }
    // Reclaim the first replaced namespace before starting a third one.
    let detached:String=n.store.readers.get().query_row("SELECT j.id FROM ivm_install_jobs j WHERE NOT EXISTS(SELECT 1 FROM ivm_install_roots r WHERE r.namespace=j.id)",[],|r|r.get(0)).unwrap();
    for _ in 0..20 {
        let mut w = n.store.connection.write();
        let tx = w.transaction().unwrap();
        let done = n.source().installer.reclaim(&tx, &detached, 2).unwrap();
        tx.commit().unwrap();
        if done {
            break;
        }
    }
    install(&n);
    assert_eq!(output(&n), oracle(&n));
}

#[test]
fn rollback_captures_neither_claim_revision_journal_nor_live_namespace() {
    let n = node("alder");
    register(&n);
    install(&n);
    let before = n.source().root(&n.store.readers.get(), "mailbox").unwrap();
    {
        let mut w = n.store.connection.write();
        let tx = w.transaction().unwrap();
        n.runtime
            .append_claim_tx(
                &tx,
                "alder",
                "message/a",
                "message.sent",
                None,
                &json!({"fields":{"to":"person/lichen","body":"rolled back"},"evidence":[]}),
                &[],
                None,
            )
            .unwrap();
        tx.rollback().unwrap();
    }
    assert_eq!(
        before,
        n.source().root(&n.store.readers.get(), "mailbox").unwrap()
    );
    assert_eq!(output(&n), oracle(&n));
}

#[test]
fn reopening_preserves_namespace_and_cursor_without_registration_or_history_replay() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source.sqlite3");
    let fail = Arc::new(AtomicBool::new(false));
    let rt = runtime(fail.clone());
    let n = Node {
        store: Store::open(&path, "alder", rt.clone()).unwrap(),
        runtime: rt,
        fail,
    };
    register(&n);
    append(
        &n,
        "message/a",
        "message.sent",
        json!({"to":"person/lichen","body":"persisted"}),
    );
    let id = start(&n);
    scan(&n, &id);
    let before = n
        .source()
        .installer
        .progress(&n.store.readers.get(), &id)
        .unwrap();
    drop(n);
    let fail = Arc::new(AtomicBool::new(false));
    let rt = runtime(fail.clone());
    let n = Node {
        store: Store::open(&path, "alder", rt.clone()).unwrap(),
        runtime: rt,
        fail,
    };
    assert_eq!(
        before.cursor,
        n.source()
            .installer
            .progress(&n.store.readers.get(), &id)
            .unwrap()
            .cursor
    );
    assert!(n.source().root(&n.store.readers.get(), "mailbox").is_err());
    publish(&n, &id);
    assert_eq!(output(&n), oracle(&n));
}

#[test]
fn extraction_byte_bound_is_checked_before_publication_and_remap_cannot_reuse_cursor() {
    let n = node("alder");
    register(&n);
    let claim = append(
        &n,
        "message/a",
        "message.sent",
        json!({"to":"person/lichen","body":"large".repeat(200)}),
    );
    let id = start(&n);
    assert!(extract_page(&n, &id, 64).is_err());
    {
        let w = n.store.connection.write();
        w.execute(
            "UPDATE claims SET store_index=store_index+10 WHERE id=?1",
            [&claim.id],
        )
        .unwrap();
    }
    assert!(extract_page(&n, &id, 8192).is_err());
    assert_eq!(
        n.source()
            .installer
            .progress(&n.store.readers.get(), &id)
            .unwrap()
            .phase,
        "stopped"
    );
    assert!(n.source().root(&n.store.readers.get(), "mailbox").is_err());
}

#[test]
fn captured_after_registration_local_sealing_preserves_rank_then_position_edit_fences() {
    let n = node("alder");
    register(&n);
    install(&n);
    let claim = append(
        &n,
        "message/a",
        "message.sent",
        json!({"to":"person/lichen","body":"sealed"}),
    );
    let before = n.source().root(&n.store.readers.get(), "mailbox").unwrap();
    n.store.seal_local_batches().unwrap();
    assert_eq!(
        before,
        n.source().root(&n.store.readers.get(), "mailbox").unwrap()
    );
    let index = smallclaims::store::current_index(&n.store.readers.get()).unwrap();
    {
        let w = n.store.connection.write();
        assert_eq!(
            w.execute(
                "UPDATE replica_records SET position=position+10 WHERE claim_id=?1",
                [&claim.id]
            )
            .unwrap(),
            1
        );
    }
    assert_eq!(
        index,
        smallclaims::store::current_index(&n.store.readers.get()).unwrap()
    );
    assert!(n.source().root(&n.store.readers.get(), "mailbox").is_err());
    assert!(
        !n.source()
            .availability(&n.store.readers.get(), "mailbox")
            .unwrap()
            .ready
    );
}

#[test]
fn populated_before_registration_install_then_seal_fences_missing_historical_rank_provenance() {
    let n = node("alder");
    let claim = append(
        &n,
        "message/historical",
        "message.sent",
        json!({"to":"person/lichen","body":"written before registration"}),
    );
    assert_eq!(
        n.store
            .readers
            .get()
            .query_row(
                "SELECT COUNT(*) FROM replica_records WHERE claim_id=?1",
                [&claim.id],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
        0
    );
    register(&n);
    install(&n);
    assert_eq!(output(&n), oracle(&n));
    let before = n.source().root(&n.store.readers.get(), "mailbox").unwrap();
    let before_status = n
        .source()
        .availability(&n.store.readers.get(), "mailbox")
        .unwrap();
    assert!(before_status.ready);
    let before_rank = canonical::claim_key(&n.store.readers.get(), &claim.id).unwrap();
    let before_oracle = oracle(&n);
    let before_index = smallclaims::store::current_index(&n.store.readers.get()).unwrap();
    let facts: Vec<(String, String)> = n
        .store
        .readers
        .get()
        .prepare("SELECT id,value FROM cm_facts WHERE namespace=?1 ORDER BY id")
        .unwrap()
        .query_map([before.namespace.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    // Extraction and namespace publication did not seed the adapter's capture cache.
    assert_eq!(
        n.store
            .readers
            .get()
            .query_row(
                "SELECT COUNT(*) FROM ivm_install_claim_ranks WHERE source=?1 AND claim_id=?2",
                params![SOURCE, claim.id],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
        0
    );
    n.store.seal_local_batches().unwrap();
    assert_eq!(
        n.store
            .readers
            .get()
            .query_row(
                "SELECT COUNT(*) FROM replica_records WHERE claim_id=?1",
                [&claim.id],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
        1
    );
    // The answer and actual canonical tuple are unchanged; absence of historical
    // provenance still requires the conservative source fence in this prototype.
    assert_eq!(
        before_rank,
        canonical::claim_key(&n.store.readers.get(), &claim.id).unwrap()
    );
    assert_eq!(before_oracle, oracle(&n));
    assert_eq!(
        before_index,
        smallclaims::store::current_index(&n.store.readers.get()).unwrap()
    );
    assert!(n.source().root(&n.store.readers.get(), "mailbox").is_err());
    let after = n
        .source()
        .availability(&n.store.readers.get(), "mailbox")
        .unwrap();
    assert!(!after.ready);
    assert!(!after.installation.source_available);
    assert_eq!(before.generation, after.installation.generation);
    assert!(after.installation.status_revision > before.status_revision);
    assert!(
        after
            .installation
            .error
            .as_deref()
            .unwrap()
            .contains("outside bounded capture")
    );
    assert_eq!(
        before_status.installation.source.revision,
        after.installation.source.revision
    );
    let after_facts: Vec<(String, String)> = n
        .store
        .readers
        .get()
        .prepare("SELECT id,value FROM cm_facts WHERE namespace=?1 ORDER BY id")
        .unwrap()
        .query_map([before.namespace.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(facts, after_facts);
    // Readiness loss never authorizes a read-triggered seed or rejects later source writes.
    let later = append(
        &n,
        "message/later",
        "message.sent",
        json!({"to":"person/blair","body":"admitted while historical cache is missing"}),
    );
    assert!(n.store.claim_by_id(&later.id).unwrap().is_some());
    assert!(
        !n.source()
            .availability(&n.store.readers.get(), "mailbox")
            .unwrap()
            .ready
    );
    assert_eq!(
        n.store
            .readers
            .get()
            .query_row(
                "SELECT COUNT(*) FROM ivm_install_claim_ranks WHERE source=?1 AND claim_id=?2",
                params![SOURCE, claim.id],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
        0
    );
}
