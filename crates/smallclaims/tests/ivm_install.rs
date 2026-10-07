//! Prepared installation controls. Source SQL/oracles stay outside operator maintenance.
//! No production Store hook, signature admission or notification coverage is implied here.
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use smallclaims::ivm::install::{
    Installer, Limits, Mutation, Namespace, Operator, Outcome, ScanPage,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Mail {
    recipient: String,
    unread: bool,
}

struct Mailbox(&'static str);
impl Operator for Mailbox {
    fn name(&self) -> &'static str {
        "mailbox"
    }
    fn fingerprint(&self) -> &'static str {
        self.0
    }
    fn source(&self) -> &'static str {
        "admitted-mail"
    }
    fn create_schema(&self, connection: &Connection) -> Result<()> {
        connection.execute_batch("CREATE TABLE IF NOT EXISTS mail_facts(namespace TEXT,key TEXT,recipient TEXT,unread INTEGER,PRIMARY KEY(namespace,key));
            CREATE TABLE IF NOT EXISTS unread_counts(namespace TEXT,recipient TEXT,count INTEGER CHECK(count>=0),PRIMARY KEY(namespace,recipient));")?;
        Ok(())
    }
    fn apply(&self, tx: &Transaction<'_>, ns: &Namespace, rows: &[Mutation]) -> Result<bool> {
        let mut changed = false;
        for row in rows {
            ensure!(row.key != "fail-after-first", "injected operator failure");
            let before = tx
                .query_row(
                    "SELECT recipient,unread FROM mail_facts WHERE namespace=?1 AND key=?2",
                    params![ns.as_str(), row.key],
                    |r| {
                        Ok(Mail {
                            recipient: r.get(0)?,
                            unread: r.get(1)?,
                        })
                    },
                )
                .optional()?;
            let after: Option<Mail> = row.new.clone().map(serde_json::from_value).transpose()?;
            if before == after {
                continue;
            }
            if let Some(before) = before
                && before.unread
            {
                tx.execute(
                    "UPDATE unread_counts SET count=count-1 WHERE namespace=?1 AND recipient=?2",
                    params![ns.as_str(), before.recipient],
                )?;
                tx.execute(
                    "DELETE FROM unread_counts WHERE namespace=?1 AND recipient=?2 AND count=0",
                    params![ns.as_str(), before.recipient],
                )?;
            }
            match after {
                Some(after) => {
                    tx.execute("INSERT INTO mail_facts VALUES(?1,?2,?3,?4) ON CONFLICT(namespace,key) DO UPDATE SET recipient=excluded.recipient,unread=excluded.unread",
                        params![ns.as_str(),row.key,after.recipient,after.unread])?;
                    if after.unread {
                        tx.execute("INSERT INTO unread_counts VALUES(?1,?2,1) ON CONFLICT(namespace,recipient) DO UPDATE SET count=count+1",params![ns.as_str(),after.recipient])?;
                    }
                }
                None => {
                    tx.execute(
                        "DELETE FROM mail_facts WHERE namespace=?1 AND key=?2",
                        params![ns.as_str(), row.key],
                    )?;
                }
            }
            changed = true;
        }
        Ok(changed)
    }
    fn validate_publication(&self, _tx: &Transaction<'_>, _ns: &Namespace) -> Result<()> {
        // Source completeness is established by extraction + the complete replacement journal.
        // Counts are transactional operator invariants, checked against raw source by tests.
        Ok(())
    }
    fn reclaim(&self, tx: &Transaction<'_>, ns: &Namespace, rows: usize) -> Result<bool> {
        let removed = tx.execute(
            "DELETE FROM mail_facts WHERE namespace=?1 AND key IN
            (SELECT key FROM mail_facts WHERE namespace=?1 ORDER BY key LIMIT ?2)",
            params![ns.as_str(), rows as u64],
        )?;
        if removed < rows {
            tx.execute("DELETE FROM unread_counts WHERE namespace=?1 AND recipient IN
                (SELECT recipient FROM unread_counts WHERE namespace=?1 ORDER BY recipient LIMIT ?2)",params![ns.as_str(),(rows-removed) as u64])?;
        }
        Ok(!tx.query_row("SELECT EXISTS(SELECT 1 FROM mail_facts WHERE namespace=?1) OR EXISTS(SELECT 1 FROM unread_counts WHERE namespace=?1)",[ns.as_str()],|r|r.get::<_,bool>(0))?)
    }
}
fn installer() -> Installer {
    Installer::new(vec![Box::new(Mailbox("mailbox.v1;admission.v1;keys.v1"))]).unwrap()
}
fn limits() -> Limits {
    Limits {
        page_rows: 2,
        page_bytes: 4096,
        pending_rows: 32,
        pending_bytes: 128 * 1024,
        total_rows: 128,
        callback_ms: 1000,
        lifetime_ms: 1000,
    }
}
fn schema(db: &mut Connection, install: &Installer) {
    install.create_schema(db).unwrap();
    db.execute_batch("CREATE TABLE source_mail(key TEXT PRIMARY KEY,payload TEXT NOT NULL);")
        .unwrap();
    let tx = db.transaction().unwrap();
    install
        .register_source(
            &tx,
            "admitted-mail",
            "mail-source.v1;complete-replace-hook.v1",
            1,
        )
        .unwrap();
    tx.commit().unwrap();
}
fn change(tx: &Transaction<'_>, install: &Installer, key: &str, after: Option<Mail>) {
    let old: Option<String> = tx
        .query_row("SELECT payload FROM source_mail WHERE key=?1", [key], |r| {
            r.get(0)
        })
        .optional()
        .unwrap();
    let new = after.map(|m| serde_json::to_value(m).unwrap());
    if let Some(new) = &new {
        tx.execute("INSERT INTO source_mail VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET payload=excluded.payload",params![key,serde_json::to_string(new).unwrap()]).unwrap();
    } else {
        tx.execute("DELETE FROM source_mail WHERE key=?1", [key])
            .unwrap();
    }
    install
        .record(
            tx,
            "admitted-mail",
            &Mutation {
                key: key.into(),
                old: old.map(|v| serde_json::from_str(&v).unwrap()),
                new,
            },
        )
        .unwrap();
}
fn mail(recipient: &str, unread: bool) -> Option<Mail> {
    Some(Mail {
        recipient: recipient.into(),
        unread,
    })
}
fn start(db: &mut Connection, install: &Installer, limits: Limits) -> String {
    let tx = db.transaction().unwrap();
    let id = install.start(&tx, "mailbox", limits, 10).unwrap();
    tx.commit().unwrap();
    id
}
fn page(db: &mut Connection, install: &Installer, id: &str) -> ScanPage {
    // This short read snapshot ends before caller applies the page with the writer.
    let tx = db.transaction().unwrap();
    let progress = install.progress(&tx, id).unwrap();
    let position = install.position(&tx, "admitted-mail").unwrap();
    let cursor = String::from_utf8(progress.cursor.clone()).unwrap();
    let rows = {
        let mut statement = tx
            .prepare("SELECT key,payload FROM source_mail WHERE key>?1 ORDER BY key LIMIT 2")
            .unwrap();
        statement
            .query_map([cursor], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .unwrap()
            .map(|r| {
                let (key, value) = r.unwrap();
                Mutation {
                    key,
                    old: None,
                    new: Some(serde_json::from_str(&value).unwrap()),
                }
            })
            .collect::<Vec<_>>()
    };
    let next_cursor = rows
        .last()
        .map(|r| r.key.as_bytes().to_vec())
        .unwrap_or(progress.cursor.clone());
    let finished = rows.len() < 2;
    tx.commit().unwrap();
    ScanPage {
        job: id.into(),
        expected_cursor: progress.cursor,
        next_cursor,
        position,
        rows,
        finished,
    }
}
fn apply_page(db: &mut Connection, install: &Installer, page: &ScanPage) -> Outcome {
    let tx = db.transaction().unwrap();
    let outcome = install.scan(&tx, page, 20).unwrap();
    tx.commit().unwrap();
    outcome
}
fn drain(db: &mut Connection, install: &Installer, id: &str) {
    for _ in 0..40 {
        let tx = db.transaction().unwrap();
        let outcome = install.catch_up(&tx, id, 30).unwrap();
        tx.commit().unwrap();
        if outcome == Outcome::Published {
            return;
        }
        assert_eq!(outcome, Outcome::Progress);
    }
    panic!("fixture exceeded bounded catch-up calls");
}
fn compare(db: &Connection, install: &Installer) {
    let root = install.root(db, "mailbox").unwrap();
    let actual = {
        let mut s = db
            .prepare(
                "SELECT recipient,count FROM unread_counts WHERE namespace=?1 ORDER BY recipient",
            )
            .unwrap();
        s.query_map([root.namespace.as_str()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    };
    // Independent full-source SQL is a TEST ORACLE, never an operator callback.
    let expected = {
        let mut s = db.prepare("SELECT json_extract(payload,'$.recipient'),COUNT(*) FROM source_mail WHERE json_extract(payload,'$.unread')=1 GROUP BY 1 ORDER BY 1").unwrap();
        s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(actual, expected);
    let actual_rows = {
        let mut s = db
            .prepare("SELECT key,recipient,unread FROM mail_facts WHERE namespace=?1 ORDER BY key")
            .unwrap();
        s.query_map([root.namespace.as_str()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    };
    let expected_rows = {
        let mut s = db.prepare("SELECT key,json_extract(payload,'$.recipient'),json_extract(payload,'$.unread') FROM source_mail ORDER BY key").unwrap();
        s.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    };
    assert_eq!(actual_rows, expected_rows);
}

#[test]
fn mailbox_install_interleaved_replace_delete_readd_and_before_cursor_insert() {
    let mut db = Connection::open_in_memory().unwrap();
    let install = installer();
    schema(&mut db, &install);
    let tx = db.transaction().unwrap();
    change(&tx, &install, "a", mail("ada", true));
    change(&tx, &install, "c", mail("ada", true));
    tx.commit().unwrap();
    let id = start(&mut db, &install, limits());
    assert!(install.root(&db, "mailbox").is_err());
    let first = page(&mut db, &install, &id);
    let tx = db.transaction().unwrap();
    change(&tx, &install, "a", mail("bob", true));
    change(&tx, &install, "0", mail("cy", true));
    change(&tx, &install, "c", None);
    assert_eq!(install.prune_journal(&tx, "admitted-mail", 2).unwrap(), 0);
    tx.commit().unwrap();
    assert_eq!(apply_page(&mut db, &install, &first), Outcome::Progress);
    let last = page(&mut db, &install, &id);
    assert!(last.finished);
    assert_eq!(apply_page(&mut db, &install, &last), Outcome::Progress);
    let tx = db.transaction().unwrap();
    change(&tx, &install, "c", mail("bob", false));
    change(&tx, &install, "a", None);
    change(&tx, &install, "a", mail("cy", true));
    tx.commit().unwrap();
    drain(&mut db, &install, &id);
    compare(&db, &install);
    let root = install.root(&db, "mailbox").unwrap();
    let tx = db.transaction().unwrap();
    change(&tx, &install, "a", mail("cy", true));
    tx.commit().unwrap();
    let duplicate = install.root(&db, "mailbox").unwrap();
    assert_eq!(root.generation, duplicate.generation);
    assert!(duplicate.revision > root.revision);
    compare(&db, &install);
    let tx = db.transaction().unwrap();
    change(&tx, &install, "c", mail("ada", true));
    tx.commit().unwrap();
    compare(&db, &install);
}

#[test]
fn rollback_keeps_source_journal_progress_and_namespace_atomic() {
    let mut db = Connection::open_in_memory().unwrap();
    let install = installer();
    schema(&mut db, &install);
    let id = start(&mut db, &install, limits());
    let p = page(&mut db, &install, &id);
    {
        let tx = db.transaction().unwrap();
        change(&tx, &install, "a", mail("ada", true));
        drop(tx);
    }
    assert_eq!(install.position(&db, "admitted-mail").unwrap().revision, 0);
    {
        let tx = db.transaction().unwrap();
        assert_eq!(install.scan(&tx, &p, 20).unwrap(), Outcome::Progress);
        drop(tx);
    }
    assert_eq!(install.progress(&db, &id).unwrap().phase, "scan");
    assert_eq!(apply_page(&mut db, &install, &p), Outcome::Progress);
    drain(&mut db, &install, &id);
    compare(&db, &install);
}

#[test]
fn operator_error_rolls_back_partial_page_and_stops_without_rejecting_source() {
    let mut db = Connection::open_in_memory().unwrap();
    let install = installer();
    schema(&mut db, &install);
    let tx = db.transaction().unwrap();
    change(&tx, &install, "a", mail("ada", true));
    change(&tx, &install, "fail-after-first", mail("bob", true));
    tx.commit().unwrap();
    let id = start(&mut db, &install, limits());
    let p = page(&mut db, &install, &id);
    assert!(matches!(
        apply_page(&mut db, &install, &p),
        Outcome::Stopped(_)
    ));
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM mail_facts WHERE namespace=?1",
            [&id],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM source_mail", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        2
    );
    let tx = db.transaction().unwrap();
    change(&tx, &install, "later", mail("cy", true));
    tx.commit().unwrap();
    assert!(install.root(&db, "mailbox").is_err());
    assert_eq!(install.progress(&db, &id).unwrap().phase, "stopped");
}

#[test]
fn journal_total_quota_stops_even_when_consumed_and_source_writes_continue() {
    let mut db = Connection::open_in_memory().unwrap();
    let install = installer();
    schema(&mut db, &install);
    let mut cap = limits();
    cap.pending_rows = 2;
    let id = start(&mut db, &install, cap);
    let p = page(&mut db, &install, &id);
    apply_page(&mut db, &install, &p);
    let tx = db.transaction().unwrap();
    change(&tx, &install, "a", mail("ada", true));
    change(&tx, &install, "b", mail("bob", true));
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    assert_eq!(install.catch_up(&tx, &id, 20).unwrap(), Outcome::Progress);
    tx.commit().unwrap();
    assert_eq!(install.progress(&db, &id).unwrap().queued_rows, 0);
    let tx = db.transaction().unwrap();
    change(&tx, &install, "c", mail("cy", true));
    tx.commit().unwrap();
    assert_eq!(install.progress(&db, &id).unwrap().phase, "stopped");
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM ivm_install_journal", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM source_mail", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        3
    );
    let tx = db.transaction().unwrap();
    assert!(install.start(&tx, "mailbox", limits(), 30).is_err());
    drop(tx);
    let tx = db.transaction().unwrap();
    assert!(!install.reclaim(&tx, &id, 2).unwrap());
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    assert!(install.reclaim(&tx, &id, 2).unwrap());
    assert_eq!(install.prune_journal(&tx, "admitted-mail", 1).unwrap(), 1);
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    assert_eq!(install.prune_journal(&tx, "admitted-mail", 1).unwrap(), 1);
    tx.commit().unwrap();
    let id = start(&mut db, &install, limits());
    loop {
        let p = page(&mut db, &install, &id);
        apply_page(&mut db, &install, &p);
        if p.finished {
            break;
        }
    }
    drain(&mut db, &install, &id);
    compare(&db, &install);
}

#[test]
fn reopen_resumes_cursor_without_open_time_replay_or_partial_readiness() {
    let temp = tempfile::NamedTempFile::new().unwrap();
    let install = installer();
    let id;
    {
        let mut db = Connection::open(temp.path()).unwrap();
        schema(&mut db, &install);
        let tx = db.transaction().unwrap();
        for key in ["a", "b", "c"] {
            change(&tx, &install, key, mail("ada", true));
        }
        tx.commit().unwrap();
        id = start(&mut db, &install, limits());
        let p = page(&mut db, &install, &id);
        apply_page(&mut db, &install, &p);
        assert_eq!(install.progress(&db, &id).unwrap().cursor, b"b".to_vec());
    }
    let mut db = Connection::open(temp.path()).unwrap();
    let install = installer();
    install.create_schema(&db).unwrap();
    assert!(install.root(&db, "mailbox").is_err());
    assert_eq!(install.progress(&db, &id).unwrap().cursor, b"b".to_vec());
    let p = page(&mut db, &install, &id);
    assert!(p.finished);
    apply_page(&mut db, &install, &p);
    drain(&mut db, &install, &id);
    compare(&db, &install);
}

#[test]
fn source_gap_preserves_admission_and_requires_explicit_coverage_and_installation() {
    let mut db = Connection::open_in_memory().unwrap();
    let install = installer();
    schema(&mut db, &install);
    let id = start(&mut db, &install, limits());
    let p = page(&mut db, &install, &id);
    apply_page(&mut db, &install, &p);
    drain(&mut db, &install, &id);
    let tx = db.transaction().unwrap();
    install
        .source_gap(
            &tx,
            "admitted-mail",
            "same-index signature evidence not captured",
        )
        .unwrap();
    change(&tx, &install, "a", mail("ada", true));
    tx.commit().unwrap();
    assert!(install.root(&db, "mailbox").is_err());
    assert!(install.position(&db, "admitted-mail").is_err());
    let expected = smallclaims::ivm::install::SourcePosition {
        source: "admitted-mail".into(),
        fingerprint: "mail-source.v1;complete-replace-hook.v1".into(),
        epoch: 1,
        revision: 1,
    };
    let tx = db.transaction().unwrap();
    install
        .restore_source(&tx, &expected, "mail-source.v2;signatures.v1", 2)
        .unwrap();
    tx.commit().unwrap();
    assert!(install.root(&db, "mailbox").is_err());
    let new = start(&mut db, &install, limits());
    let p = page(&mut db, &install, &new);
    apply_page(&mut db, &install, &p);
    drain(&mut db, &install, &new);
    compare(&db, &install);
    assert_ne!(install.root(&db, "mailbox").unwrap().namespace.as_str(), id);
}

#[test]
fn cursor_version_and_lifetime_errors_stop_with_no_automatic_restart() {
    let mut db = Connection::open_in_memory().unwrap();
    let install = installer();
    schema(&mut db, &install);
    let id = start(&mut db, &install, limits());
    let mut p = page(&mut db, &install, &id);
    p.expected_cursor = b"invented".to_vec();
    assert!(matches!(
        apply_page(&mut db, &install, &p),
        Outcome::Stopped(_)
    ));
    let tx = db.transaction().unwrap();
    assert!(install.reclaim(&tx, &id, 1).unwrap());
    tx.commit().unwrap();
    let id = start(&mut db, &install, limits());
    let p = page(&mut db, &install, &id);
    let successor = Installer::new(vec![Box::new(Mailbox("mailbox.v2"))]).unwrap();
    assert!(matches!(
        apply_page(&mut db, &successor, &p),
        Outcome::Stopped(_)
    ));
    let tx = db.transaction().unwrap();
    assert!(install.reclaim(&tx, &id, 1).unwrap());
    tx.commit().unwrap();
    let mut short = limits();
    short.lifetime_ms = 1;
    let id = start(&mut db, &install, short);
    let p = page(&mut db, &install, &id);
    assert!(matches!(
        apply_page(&mut db, &install, &p),
        Outcome::Stopped(_)
    ));
}

#[test]
fn journal_gap_and_catchup_failure_cannot_publish_partial_output() {
    let mut db = Connection::open_in_memory().unwrap();
    let install = installer();
    schema(&mut db, &install);
    let id = start(&mut db, &install, limits());
    let p = page(&mut db, &install, &id);
    apply_page(&mut db, &install, &p);
    let tx = db.transaction().unwrap();
    change(&tx, &install, "a", mail("ada", true));
    change(&tx, &install, "b", mail("bob", true));
    tx.commit().unwrap();
    db.execute("DELETE FROM ivm_install_journal WHERE revision=1", [])
        .unwrap(); // private corruption oracle
    let tx = db.transaction().unwrap();
    assert!(matches!(
        install.catch_up(&tx, &id, 30).unwrap(),
        Outcome::Stopped(_)
    ));
    tx.commit().unwrap();
    assert!(install.root(&db, "mailbox").is_err());
}

#[test]
fn live_operator_failure_fences_output_preserves_source_then_bounded_replacement_recovers() {
    let mut db = Connection::open_in_memory().unwrap();
    let install = installer();
    schema(&mut db, &install);
    let id = start(&mut db, &install, limits());
    let p = page(&mut db, &install, &id);
    apply_page(&mut db, &install, &p);
    drain(&mut db, &install, &id);
    let tx = db.transaction().unwrap();
    change(&tx, &install, "a", mail("ada", true));
    change(&tx, &install, "fail-after-first", mail("bob", true));
    tx.commit().unwrap();
    assert!(install.root(&db, "mailbox").is_err());
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM source_mail", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        2
    );
    let tx = db.transaction().unwrap();
    change(&tx, &install, "fail-after-first", None);
    tx.commit().unwrap();
    let replacement = start(&mut db, &install, limits());
    let p = page(&mut db, &install, &replacement);
    apply_page(&mut db, &install, &p);
    drain(&mut db, &install, &replacement);
    compare(&db, &install);
    // A third copy is refused until the detached old namespace is reclaimed in bounded pages.
    let tx = db.transaction().unwrap();
    assert!(install.start(&tx, "mailbox", limits(), 40).is_err());
    drop(tx);
    let tx = db.transaction().unwrap();
    assert!(!install.reclaim(&tx, &id, 1).unwrap());
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    assert!(install.reclaim(&tx, &id, 1).unwrap());
    tx.commit().unwrap();
    assert!(install.root(&db, "mailbox").is_ok());
}

#[test]
fn cancel_replacement_keeps_existing_root_live_and_exposes_stopped_job() {
    let mut db = Connection::open_in_memory().unwrap();
    let install = installer();
    schema(&mut db, &install);
    let id = start(&mut db, &install, limits());
    let p = page(&mut db, &install, &id);
    apply_page(&mut db, &install, &p);
    drain(&mut db, &install, &id);
    let old = install.root(&db, "mailbox").unwrap();
    let replacement = start(&mut db, &install, limits());
    let tx = db.transaction().unwrap();
    install.cancel(&tx, &replacement).unwrap();
    change(&tx, &install, "a", mail("ada", true));
    tx.commit().unwrap();
    assert_eq!(
        install.root(&db, "mailbox").unwrap().namespace,
        old.namespace
    );
    assert!(install.status(&db, "mailbox").unwrap().ready);
    assert_eq!(
        install.progress(&db, &replacement).unwrap().phase,
        "stopped"
    );
    compare(&db, &install);
}

#[test]
fn publication_rollback_keeps_pointer_and_job_state_atomic() {
    let mut db = Connection::open_in_memory().unwrap();
    let install = installer();
    schema(&mut db, &install);
    let id = start(&mut db, &install, limits());
    let p = page(&mut db, &install, &id);
    apply_page(&mut db, &install, &p);
    {
        let tx = db.transaction().unwrap();
        assert_eq!(install.catch_up(&tx, &id, 20).unwrap(), Outcome::Published);
        assert!(install.root(&tx, "mailbox").is_ok());
        drop(tx);
    }
    assert!(install.root(&db, "mailbox").is_err());
    assert_eq!(install.progress(&db, &id).unwrap().phase, "catchup");
    drain(&mut db, &install, &id);
    compare(&db, &install);
}

#[test]
fn total_work_and_byte_quota_stop_without_source_rollback_or_partial_output() {
    let mut db = Connection::open_in_memory().unwrap();
    let install = installer();
    schema(&mut db, &install);
    let tx = db.transaction().unwrap();
    change(&tx, &install, "a", mail("ada", true));
    tx.commit().unwrap();
    let mut cap = limits();
    cap.total_rows = 1;
    let id = start(&mut db, &install, cap);
    let p = page(&mut db, &install, &id);
    apply_page(&mut db, &install, &p);
    let tx = db.transaction().unwrap();
    change(&tx, &install, "b", mail("bob", true));
    tx.commit().unwrap();
    let tx = db.transaction().unwrap();
    assert!(matches!(
        install.catch_up(&tx, &id, 20).unwrap(),
        Outcome::Stopped(_)
    ));
    tx.commit().unwrap();
    let status = install.status(&db, "mailbox").unwrap();
    assert!(!status.ready);
    assert!(status.error.unwrap().contains("total work quota"));
    let tx = db.transaction().unwrap();
    assert!(install.reclaim(&tx, &id, 2).unwrap());
    install.prune_journal(&tx, "admitted-mail", 2).unwrap();
    tx.commit().unwrap();
    let mut cap = limits();
    cap.page_bytes = 128;
    let id = start(&mut db, &install, cap);
    let tx = db.transaction().unwrap();
    change(&tx, &install, "large", mail(&"x".repeat(256), true));
    tx.commit().unwrap();
    assert_eq!(install.progress(&db, &id).unwrap().phase, "stopped");
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM source_mail", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        3
    );
    assert!(install.root(&db, "mailbox").is_err());
}

#[test]
fn root_source_identity_cannot_alias_another_source_with_matching_cut_and_fingerprint() {
    let mut db = Connection::open_in_memory().unwrap();
    let install = installer();
    schema(&mut db, &install);
    let id = start(&mut db, &install, limits());
    let p = page(&mut db, &install, &id);
    apply_page(&mut db, &install, &p);
    drain(&mut db, &install, &id);
    assert!(install.root(&db, "mailbox").is_ok());
    let tx = db.transaction().unwrap();
    install
        .register_source(
            &tx,
            "different-admitted-mail",
            "mail-source.v1;complete-replace-hook.v1",
            1,
        )
        .unwrap();
    // Private corruption oracle: keep fingerprint/epoch/revision identical while changing
    // the persisted source identity. Reads must detect this without a repair or replay.
    tx.execute(
        "UPDATE ivm_install_roots SET source='different-admitted-mail' WHERE view='mailbox'",
        [],
    )
    .unwrap();
    tx.commit().unwrap();
    let generation: u64 = db
        .query_row(
            "SELECT generation FROM ivm_install_roots WHERE view='mailbox'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(install.root(&db, "mailbox").is_err());
    let status = install.status(&db, "mailbox").unwrap();
    assert!(!status.compatible);
    assert!(!status.ready);
    assert_eq!(status.generation, generation);
    let tx = db.transaction().unwrap();
    install
        .record(
            &tx,
            "different-admitted-mail",
            &Mutation {
                key: "a".into(),
                old: None,
                new: Some(
                    serde_json::to_value(Mail {
                        recipient: "ada".into(),
                        unread: true,
                    })
                    .unwrap(),
                ),
            },
        )
        .unwrap();
    tx.commit().unwrap();
    assert!(!install.status(&db, "mailbox").unwrap().ready);
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM mail_facts WHERE namespace=?1",
            [&id],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        0
    );
}
