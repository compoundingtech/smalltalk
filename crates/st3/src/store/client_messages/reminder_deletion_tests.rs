//! Real claim/projection fixtures; selector triggers record every row mutation.
use super::*;

const GROUP: &str = "bounded-desired-delete";
const OWNER: &str = "mission-run/bounded-desired-delete";
const RECIPIENTS: [&str; 2] = ["agent/bounded-a", "agent/bounded-b"];
const NATIVE_MEMBERS: usize = 4096;
const DECLARED_MEMBERS: usize = BACKFILL_SUBJECTS * 6 + 1;
const MAX_DELETE_SELECTOR_WRITES: usize = 9;

type PageRows = Vec<(crate::model::MessageView, bool)>;

fn native_subject(index: usize) -> String {
    format!("message/native-bounded-{index:05}")
}

fn declared_subject(index: usize) -> String {
    format!("message/declared-bounded-{index:05}")
}

fn append(transaction: &Transaction<'_>, subject: &str, kind: &str, body: &Value) -> smallclaims::ClaimRecord {
    smallclaims::store::append_claim_record_tx(
        transaction, "bounded-delete", subject, kind, Some("person/sender"), body, &[], None,
    ).unwrap()
}

fn seed_group(store: &Store, declarations: usize) {
    store.set_write_clock_at(1_800_000_000_000).unwrap();
    let mut source = String::from(
        "version 2\nagent \"bounded-a\" { workspace \"/tmp\"; command \"true\" }\nagent \"bounded-b\" { workspace \"/tmp\"; command \"true\" }\n",
    );
    for index in 0..declarations {
        let recipient = if index.is_multiple_of(2) { "bounded-a" } else { "bounded-b" };
        let version = u64::MAX - u64::try_from((declarations - 1 - index) / 2).unwrap();
        source.push_str(&format!(
            "message \"declared-bounded-{index:05}\" {{ from \"person/requester\"; to \"{recipient}\"; content \"declared {index}\"; tag \"reminder:{GROUP}\"; tag \"version:{version}\"; }}\n",
        ));
    }
    let intent = crate::graph::parse_test_intent(&source, "node").unwrap();
    {
        let mut writer = store.connection.write();
        let transaction = writer.transaction().unwrap();
        for index in 0..NATIVE_MEMBERS {
            append(&transaction, &native_subject(index), "message.sent", &json!({"fields": {
                "from": "person/sender", "to": RECIPIENTS[index % 2],
                "content": format!("native {index}"), "status": "sent",
                "tags": [format!("reminder:{GROUP}"), format!("version:{}", index / 2)],
            }}));
        }
        // The same normalized declarations and immutable claim bodies as apply,
        // without replaying unrelated graph projections in this selector fixture.
        for index in 0..declarations {
            let mut desired = intent.subjects[&declared_subject(index)].clone();
            desired.owner_run = Some(OWNER.into());
            let claim = append(&transaction, &desired.subject, "intent.desired", &serde_json::to_value(&desired).unwrap());
            transaction.execute(
                "INSERT INTO desired(subject,kind,revision,claim_id,body,owner_run) VALUES(?1,?2,?3,?4,?5,?6)",
                params![desired.subject,desired.kind,super::super::desired_revision(&desired),claim.id,
                    serde_json::to_string(&desired.desired).unwrap(),OWNER],
            ).unwrap();
        }
        // Deleting the top winner must leave its full fold durable across loans.
        for index in 0..FOLD_CLAIMS * 4 {
            append(&transaction, &declared_subject(declarations - 1), "custom.test.recorded",
                &json!({"fields": {"note": index}}));
        }
        transaction.commit().unwrap();
    }
    while store.maintain_client_message_selectors().unwrap() {}
    let reader = store.readers.get();
    let (open, winners): (usize, usize) = reader.query_row(
        "SELECT COUNT(*),SUM(global_current) FROM local_client_message_selectors_v1 WHERE born_index>=0 AND retired_index IS NULL AND closed=0 AND reminder=?1",
        [GROUP], |row| Ok((row.get(0)?,row.get(1)?)),
    ).unwrap();
    assert_eq!(open, NATIVE_MEMBERS + declarations);
    assert_eq!(winners, 1);
    assert!(open - winners >= 4000, "fixture must contain thousands of superseded open members");
}

fn audit_deletions(connection: &Connection) {
    connection.execute_batch(r#"
CREATE TEMP TABLE reminder_delete_context(subject TEXT NOT NULL);
CREATE TEMP TABLE reminder_selector_writes(cause TEXT,subject TEXT NOT NULL,born_index INTEGER NOT NULL);
CREATE TEMP TRIGGER reminder_delete_context_set BEFORE DELETE ON desired BEGIN
 DELETE FROM reminder_delete_context;
 INSERT INTO reminder_delete_context VALUES(OLD.subject);
END;
CREATE TEMP TRIGGER reminder_selector_insert AFTER INSERT ON local_client_message_selectors_v1 BEGIN
 INSERT INTO reminder_selector_writes VALUES((SELECT subject FROM reminder_delete_context),NEW.subject,NEW.born_index);
END;
CREATE TEMP TRIGGER reminder_selector_update AFTER UPDATE ON local_client_message_selectors_v1 BEGIN
 INSERT INTO reminder_selector_writes VALUES((SELECT subject FROM reminder_delete_context),NEW.subject,NEW.born_index);
END;
CREATE TEMP TRIGGER reminder_selector_delete AFTER DELETE ON local_client_message_selectors_v1 BEGIN
 INSERT INTO reminder_selector_writes VALUES((SELECT subject FROM reminder_delete_context),OLD.subject,OLD.born_index);
END;
"#).unwrap();
}

fn writes(connection: &Connection) -> Vec<(String, String, i64)> {
    connection.prepare("SELECT COALESCE(cause,''),subject,born_index FROM reminder_selector_writes").unwrap()
        .query_map([], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap()
        .collect::<rusqlite::Result<Vec<_>>>().unwrap()
}

fn assert_single_delete_writes(connection: &Connection, affected: &str, allowed_live: &[String]) {
    let changes = writes(connection);
    assert!(changes.len() <= MAX_DELETE_SELECTOR_WRITES,
        "desired deletion changed {} selector rows for a {NATIVE_MEMBERS}-member group: {changes:?}", changes.len());
    assert!(changes.iter().any(|(_,subject,born)|subject==affected && *born==-1),
        "unfinished reconciliation must stay in the subject-keyed pending queue");
    for (cause,subject,born) in changes {
        assert_eq!(cause, affected);
        if born < 0 {
            assert_eq!(born, -1);
            assert_eq!(subject, affected, "cannot enqueue a whole reminder group");
        } else {
            assert!(allowed_live.contains(&subject), "only the affected subject and winners may be touched: {subject}");
        }
    }
}

fn assert_bounded_deletion_predicate(connection: &Connection, affected: &str, allowed: &[String]) {
    let trigger: String = connection.query_row(
        "SELECT sql FROM sqlite_master WHERE type='trigger' AND name='client_selector_desired_delete'",
        [], |row| row.get(0),
    ).unwrap();
    let predicate = trigger.split_once(" WHERE born_index>=0 AND retired_index IS NULL AND").unwrap().1
        .trim().strip_suffix("END").unwrap().trim().trim_end_matches(';').replace("OLD.subject", "?1");
    let sql = format!("SELECT subject FROM local_client_message_selectors_v1 WHERE born_index>=0 AND retired_index IS NULL AND {predicate}");
    let selected = connection.prepare(&sql).unwrap().query_map([affected], |row| row.get::<_,String>(0)).unwrap()
        .collect::<rusqlite::Result<Vec<_>>>().unwrap();
    assert!(selected.len() <= 7, "the actual deletion predicate cannot inventory a reminder group");
    assert!(selected.iter().all(|subject|allowed.contains(subject)), "unexpected deletion candidates: {selected:?}");
    let plan = connection.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap()
        .query_map([affected], |row|row.get::<_,String>(3)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    for index in ["client_selector_reminder_candidates", "client_selector_recipient_reminder_candidates",
        "client_selector_reminder_winner", "client_selector_recipient_reminder_winner"] {
        assert!(plan.iter().any(|step|step.contains("SEARCH") && step.contains(index)),
            "deletion must seek {index}: {plan:?}");
    }
    assert!(!plan.iter().any(|step|step.contains("SCAN local_client_message_selectors_v1") ||
        step.contains("USE TEMP B-TREE FOR ORDER BY")), "group-size-dependent deletion plan: {plan:?}");
}

fn pages(store: &Store, person: Option<&str>, history: bool, limit: usize) -> PageRows {
    store.read_snapshot(|through| {
        let cut = store.client_messages_page_cut(through)?;
        let mut result = Vec::new();
        let mut after = None;
        loop {
            let mut page = store.client_messages_page(person,None,history,cut,after.as_ref(),limit)?;
            let more = page.len() > limit;
            page.truncate(limit);
            for (message,metadata,current) in page {
                after = Some((metadata["sent_at"].as_str().unwrap().parse::<u128>()?,message.subject.clone()));
                result.push((message,current));
            }
            if !more { return Ok(result); }
        }
    }).unwrap()
}

fn assert_fresh_and_continued_parity(store: &Store) {
    for person in [None,Some(RECIPIENTS[0]),Some(RECIPIENTS[1])] {
        let actual = pages(store,person,false,1);
        assert_eq!(serde_json::to_value(&actual).unwrap(),serde_json::to_value(pages(store,person,false,100)).unwrap(),
            "continued pages must match fresh pages for {person:?}");
        let expected = store.operational_messages(person,false).unwrap();
        let expected = expected.into_iter().map(|message|(message.subject.clone(),serde_json::to_value(message).unwrap()))
            .collect::<BTreeMap<_,_>>();
        let subjects = actual.into_iter().map(|(message,current)| {
            assert!(current);
            (message.subject.clone(),serde_json::to_value(message).unwrap())
        }).collect::<BTreeMap<_,_>>();
        assert_eq!(subjects,expected,"fresh page/canonical parity for {person:?}");
    }
}

#[test]
fn desired_delete_large_reminder_group_changes_only_affected_subject_and_winners() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("bounded-single-delete.sqlite"),"bounded-delete").unwrap();
    seed_group(&store,DECLARED_MEMBERS);
    let affected = declared_subject(DECLARED_MEMBERS - 1);
    let successor = declared_subject(DECLARED_MEMBERS - 2);
    let recipient_successor = declared_subject(DECLARED_MEMBERS - 3);
    let allowed = vec![affected.clone(),successor.clone(),recipient_successor.clone()];
    let epoch = store.client_messages_cut_epoch().unwrap();
    let frontier = store.client_messages_page_cut(store.index().unwrap()).unwrap();
    assert_fresh_and_continued_parity(&store);
    {
        let mut writer = store.connection.write();
        audit_deletions(&writer);
        let transaction = writer.transaction().unwrap();
        // A pre-existing pending row must also capture the deleted header's old
        // recipient, rather than leave its default empty recipient behind.
        transaction.execute("UPDATE desired SET revision=revision WHERE subject=?1", [&affected]).unwrap();
        transaction.execute("DELETE FROM reminder_selector_writes", []).unwrap();
        assert_eq!(transaction.execute("DELETE FROM desired WHERE subject=?1", [&affected]).unwrap(),1);
        assert_single_delete_writes(&transaction,&affected,&allowed);
        assert_bounded_deletion_predicate(&transaction,&affected,&allowed);
        let pending: (String,String) = transaction.query_row(
            "SELECT reminder,recipient FROM local_client_message_selectors_v1 WHERE subject=?1 AND born_index=-1",
            [&affected], |row| Ok((row.get(0)?,row.get(1)?)),
        ).unwrap();
        assert_eq!(pending,(GROUP.into(),RECIPIENTS[0].into()));
        transaction.commit().unwrap();
    }
    assert_eq!(store.client_messages_cut_epoch().unwrap(),epoch+1,
        "the existing HTTP desired-delete cursor regression must still expire old traversals");
    assert_eq!(store.client_messages_page_cut(store.index().unwrap()).unwrap(),frontier);
    assert!(store.client_message_selectors_pending().unwrap());
    let immediate = pages(&store,None,false,1);
    assert_eq!(immediate.iter().map(|(message,_)|message.subject.clone()).collect::<BTreeSet<_>>(),
        BTreeSet::from([affected.clone(),successor]));
    let deleted = immediate.iter().find(|(message,_)|message.subject==affected).unwrap();
    assert_eq!(deleted.0.from,"requester");
    assert!(deleted.0.to.is_empty());
    assert!(deleted.0.tags.is_empty());
    assert_fresh_and_continued_parity(&store);
    assert!(store.maintain_client_message_selectors().unwrap(),"long affected history must remain durable after one bounded loan");
    assert!(store.client_message_selectors_pending().unwrap());
    assert_fresh_and_continued_parity(&store);
    while store.maintain_client_message_selectors().unwrap() {}
    assert_eq!(serde_json::to_value(pages(&store,None,false,1)).unwrap(),serde_json::to_value(immediate).unwrap());
    assert_fresh_and_continued_parity(&store);
}

#[test]
fn desired_delete_preserves_native_reminder_and_promotes_only_old_recipient_winner() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("bounded-native-tags.sqlite"),"bounded-delete").unwrap();
    seed_group(&store,2);
    let affected = declared_subject(1);
    // A prior native claim can arrive through replication after its declaration.
    // Do not resend an already-sent message through the local transition API.
    {
        let mut writer = store.connection.write();
        let transaction = writer.transaction().unwrap();
        smallclaims::store::append_claim_record_tx(
            &transaction,"peer",&affected,"message.sent",Some("person/sender"),
            &json!({"fields":{"content":"native body","status":"sent",
                "tags":[format!("reminder:{GROUP}"),format!("version:{}",u64::MAX)]}}),
            &[],None,
        ).unwrap();
        transaction.commit().unwrap();
    }
    while store.maintain_client_message_selectors().unwrap() {}
    {
        let mut writer = store.connection.write();
        audit_deletions(&writer);
        let transaction = writer.transaction().unwrap();
        transaction.execute("UPDATE desired SET revision=revision WHERE subject=?1", [&affected]).unwrap();
        transaction.execute("DELETE FROM reminder_selector_writes", []).unwrap();
        assert_eq!(transaction.execute("DELETE FROM desired WHERE subject=?1", [&affected]).unwrap(),1);
        assert_single_delete_writes(&transaction,&affected,&[affected.clone(),native_subject(NATIVE_MEMBERS-1)]);
        transaction.commit().unwrap();
    }
    let rows = pages(&store,None,false,1);
    assert_eq!(rows.len(),1);
    assert_eq!(rows[0].0.subject,affected);
    assert_eq!(rows[0].0.tags,vec![format!("reminder:{GROUP}"),format!("version:{}",u64::MAX)]);
    assert_eq!(pages(&store,Some(RECIPIENTS[1]),false,1)[0].0.subject,native_subject(NATIVE_MEMBERS-1));
    assert_fresh_and_continued_parity(&store);
    while store.maintain_client_message_selectors().unwrap() {}
    assert_fresh_and_continued_parity(&store);
}

#[test]
fn bulk_owned_desired_delete_does_not_rewrite_superseded_reminder_members() {
    for managed in [false,true] {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("bounded-bulk-delete.sqlite"),"bounded-delete").unwrap();
        seed_group(&store,DECLARED_MEMBERS);
        let epoch = store.client_messages_cut_epoch().unwrap();
        {
            let writer = store.connection.write();
            audit_deletions(&writer);
        }
        if managed {
            assert_eq!(store.discard_desired_owned_by(OWNER).unwrap(),DECLARED_MEMBERS);
        } else {
            let mut writer = store.connection.write();
            let transaction = writer.transaction().unwrap();
            assert_eq!(transaction.execute("DELETE FROM desired WHERE owner_run=?1",[OWNER]).unwrap(),DECLARED_MEMBERS);
            let changes = writes(&transaction);
            let mut per_subject = BTreeMap::<String,usize>::new();
            for (cause,_,_) in &changes { *per_subject.entry(cause.clone()).or_default() += 1; }
            assert_eq!(per_subject.len(),DECLARED_MEMBERS);
            assert!(per_subject.values().all(|count|*count<=MAX_DELETE_SELECTOR_WRITES),
                "each deletion's source work must be constant, even inside one bulk transaction: {per_subject:?}");
            assert_eq!(transaction.query_row(
                "SELECT COUNT(*) FROM local_client_message_selectors_v1 WHERE born_index=-1",
                [], |row|row.get::<_,usize>(0),
            ).unwrap(),DECLARED_MEMBERS,"only deleted subjects, not reminder group members, may be queued");
            transaction.commit().unwrap();
        }
        {
            let reader = store.readers.get();
            let pending: usize = reader.query_row(
                "SELECT COUNT(*) FROM local_client_message_selectors_v1 WHERE born_index=-1",
                [], |row|row.get(0),
            ).unwrap();
            assert!(pending>=DECLARED_MEMBERS-BACKFILL_SUBJECTS,
                "bulk desired discard must leave unfinished subjects durable after its one transaction allowance");
        }
        {
            let writer = store.connection.write();
            let changes = writes(&writer);
            // The managed API can additionally perform one bounded fold/cleanup
            // allowance, never a second inventory of the 4,096-member group.
            let fold_allowance = if managed { BACKFILL_SUBJECTS*32+PRUNE_ROWS } else { 0 };
            assert!(changes.len()<=DECLARED_MEMBERS*MAX_DELETE_SELECTOR_WRITES+fold_allowance,
                "bulk source/fold selector mutations exceeded the bounded allowance: {}",changes.len());
            let native_winners = [native_subject(NATIVE_MEMBERS-2),native_subject(NATIVE_MEMBERS-1)];
            for (_,subject,born) in changes {
                assert!(subject.starts_with("message/declared-bounded-") ||
                    (born>=0 && native_winners.contains(&subject)),
                    "bulk deletion touched a superseded native reminder member: {subject} at {born}");
            }
        }
        assert_eq!(store.client_messages_cut_epoch().unwrap(),epoch+u64::try_from(DECLARED_MEMBERS).unwrap());
        assert_fresh_and_continued_parity(&store);
        assert_eq!(pages(&store,None,false,5).len(),DECLARED_MEMBERS+1);
        for (index,recipient) in RECIPIENTS.into_iter().enumerate() {
            let expected = native_subject(NATIVE_MEMBERS-2+index);
            assert_eq!(pages(&store,Some(recipient),false,1)[0].0.subject,expected);
        }
        while store.maintain_client_message_selectors().unwrap() {}
        assert!(!store.client_message_selectors_pending().unwrap());
        assert_fresh_and_continued_parity(&store);
        assert_eq!(pages(&store,None,false,5).len(),DECLARED_MEMBERS+1);
    }
}
