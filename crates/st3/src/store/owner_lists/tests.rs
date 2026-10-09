//! Published glasses and arrangements against their full reads: every person's published rows
//! equal `glasses_at`/`arrangements_at` (and a selected arrangement equals `arrangement_at`) at
//! the publication's cut, through edits, deletions, retirement, replication in either order,
//! reopen and forgotten views; a refresh reads again only the people whose rows may have changed.
use super::*;
use crate::model::ReplicationInventory;
use crate::store::tests::{exchange_from, receive_and_project};

const PEOPLE: &[&str] = &["person/ada", "person/bo", "person/nobody"];
const ADA_LAYOUT: &str = "arrangement/person/ada/019a0000-0000-7000-8000-000000000001";
const BO_LAYOUT: &str = "arrangement/person/bo/019a0000-0000-7000-8000-000000000002";

fn glass(person: &str, id: usize, name: &str) -> ClaimInput {
    ClaimInput {
        subject: format!("glass/{person}/019a0000-0000-7000-8000-{id:012x}"),
        kind: "glass.upserted".into(),
        actor: Some(person.into()),
        fields: serde_json::from_value(json!({
            "body":{"name":name,"layout":{"tabs":[{"pane":"opaque:anything"}]}},
            "base_revision":null,
        }))
        .unwrap(),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    }
}

fn arrangement(subject: &str, owner: &str, operations: Value) -> ClaimInput {
    ClaimInput {
        subject: subject.into(),
        kind: "arrangement.edited".into(),
        actor: Some(owner.into()),
        fields: serde_json::from_value(json!({"owner":owner,"operations":operations})).unwrap(),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    }
}

fn sync(source: &Store, target: &Store) {
    receive_and_project(
        target,
        &source.origin,
        &exchange_from(source, &ReplicationInventory::default()),
    );
}

fn newest(store: &Store, view: OwnerView) -> Arc<OwnerListPublication> {
    view.list(store)
        .published
        .lock()
        .unwrap()
        .clone()
        .expect("a publication")
}

/// Refresh `view`, then check every person's published rows against the full read at the
/// publication's cut, and each selected arrangement against `arrangement_at`.
fn refresh_and_check(store: &Store, view: OwnerView) -> Arc<OwnerListPublication> {
    store.refresh_owner_list(view, now_ms()).unwrap();
    let publication = newest(store, view);
    store
        .read_snapshot(|index| {
            assert_eq!(index, publication.cut, "published at the current cut");
            let connection = store.readers.get();
            for person in PEOPLE {
                let published = publication
                    .owners
                    .get(*person)
                    .map(|rows| (**rows).clone())
                    .unwrap_or_default();
                assert_eq!(published, view.rows(&connection, person, index)?, "{view:?} {person}");
                if view == OwnerView::Arrangements {
                    // A window following one arrangement keeps that row of its owner's.
                    for subject in [ADA_LAYOUT, BO_LAYOUT]
                        .into_iter()
                        .filter(|subject| st3_schema::arrangements::owner(subject).ok() == Some(*person))
                    {
                        let selected = published.iter().filter(|row| row["id"] == subject).cloned();
                        assert_eq!(
                            selected.collect::<Vec<_>>(),
                            arrangements::arrangement_at(&connection, subject, index)?
                                .into_iter()
                                .collect::<Vec<_>>(),
                        );
                    }
                }
            }
            Ok(())
        })
        .unwrap();
    publication
}

#[test]
fn glasses_read_again_only_the_people_whose_glasses_changed() {
    let store = Store::open_memory("alder").unwrap();
    let ada = store.append_claim(&glass("person/ada", 1, "Desk")).unwrap();
    store.append_claim(&glass("person/ada", 2, "Phone")).unwrap();
    let bo = store.append_claim(&glass("person/bo", 3, "Wall")).unwrap();
    let first = refresh_and_check(&store, OwnerView::Glasses);
    assert_eq!(first.owners["person/ada"].len(), 2);
    assert_eq!(store.owner_list_reads(OwnerView::Glasses), (2, 1), "everyone, once");
    // An edit of one of Ada's glasses reads Ada again, nobody else.
    let mut edit = glass("person/ada", 1, "Desk, renamed");
    edit.fields.insert("base_revision".into(), json!(ada.id));
    store.append_claim(&edit).unwrap();
    let edited = refresh_and_check(&store, OwnerView::Glasses);
    assert!(Arc::ptr_eq(&edited.owners["person/bo"], &first.owners["person/bo"]));
    assert_eq!(store.owner_list_reads(OwnerView::Glasses), (3, 1));
    // A deletion removes Bo's entry.
    let mut delete = glass("person/bo", 3, "Wall");
    delete.kind = "glass.deleted".into();
    delete.fields = serde_json::from_value(json!({"base_revision":bo.id})).unwrap();
    store.append_claim(&delete).unwrap();
    let deleted = refresh_and_check(&store, OwnerView::Glasses);
    assert!(!deleted.owners.contains_key("person/bo"));
    // A claim about something else reads nobody and keeps the same rows.
    store
        .append_claim(&ClaimInput {
            subject: "host/one".into(),
            kind: "transport.observed".into(),
            actor: None,
            fields: BTreeMap::from([("status".into(), json!("up"))]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    assert!(!store.refresh_owner_list(OwnerView::Glasses, now_ms()).unwrap());
    let unrelated = refresh_and_check(&store, OwnerView::Glasses);
    assert!(unrelated.cut > deleted.cut);
    assert!(Arc::ptr_eq(&unrelated.owners, &deleted.owners));
    assert_eq!(store.owner_list_reads(OwnerView::Glasses), (5, 1));
}

#[test]
fn glasses_converge_whichever_order_replication_delivers_them() {
    let alder = Store::open_memory("alder").unwrap();
    let birch = Store::open_memory("birch").unwrap();
    alder.set_write_clock_at(1_800_000_000_000).unwrap();
    birch.set_write_clock_at(1_800_000_000_000).unwrap();
    alder.append_claim(&glass("person/ada", 1, "On alder")).unwrap();
    birch.append_claim(&glass("person/ada", 1, "On birch")).unwrap();
    birch.append_claim(&glass("person/bo", 2, "Only birch")).unwrap();
    let forward = Store::open_memory("cedar").unwrap();
    let reverse = Store::open_memory("elm").unwrap();
    for (target, sources) in [(&forward, [&alder, &birch]), (&reverse, [&birch, &alder])] {
        refresh_and_check(target, OwnerView::Glasses);
        for source in sources {
            sync(source, target);
            refresh_and_check(target, OwnerView::Glasses);
        }
    }
    assert_eq!(
        newest(&forward, OwnerView::Glasses).owners,
        newest(&reverse, OwnerView::Glasses).owners
    );
}

#[test]
fn arrangements_follow_edits_retirement_replication_reopen_and_forgotten_views() {
    let a = Store::open_memory("alder").unwrap();
    a.append_client_claim(&arrangement(ADA_LAYOUT, "person/ada", json!([{"op":"create","name":"Work"}])))
        .unwrap();
    a.append_client_claim(&arrangement(BO_LAYOUT, "person/bo", json!([{"op":"create","name":"Home"}])))
        .unwrap();
    let first = refresh_and_check(&a, OwnerView::Arrangements);
    assert_eq!(first.owners.len(), 2);
    a.append_client_claim(&arrangement(ADA_LAYOUT, "person/ada", json!([{"op":"rename","name":"Desk"}])))
        .unwrap();
    let renamed = refresh_and_check(&a, OwnerView::Arrangements);
    assert!(Arc::ptr_eq(&renamed.owners["person/bo"], &first.owners["person/bo"]));
    assert_eq!(store_reads(&a), (3, 1), "Ada again, not Bo");
    // Replication in either order converges, each receiver publishing after each exchange.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("arrangements.sqlite3");
    let b = Store::open(&path, "birch").unwrap();
    refresh_and_check(&b, OwnerView::Arrangements);
    b.append_client_claim(&arrangement(ADA_LAYOUT, "person/ada", json!([{"op":"create","name":"Offline"}])))
        .ok();
    sync(&a, &b);
    refresh_and_check(&b, OwnerView::Arrangements);
    sync(&b, &a);
    refresh_and_check(&a, OwnerView::Arrangements);
    let mut retire = arrangement(BO_LAYOUT, "person/bo", json!([{"op":"retire"}]));
    retire.actor = Some("person/bo".into());
    a.append_client_claim(&retire).unwrap();
    let retired = refresh_and_check(&a, OwnerView::Arrangements);
    assert!(!retired.owners.contains_key("person/bo"));
    sync(&a, &b);
    let before = refresh_and_check(&b, OwnerView::Arrangements);
    assert_eq!(before.owners, retired.owners);
    // A reopened store publishes the same rows from nothing.
    drop(b);
    let b = Store::open(&path, "birch").unwrap();
    assert!(b.smalltalk.arrangement_list.published.lock().unwrap().is_none());
    assert_eq!(refresh_and_check(&b, OwnerView::Arrangements).owners, before.owners);
    // Forgotten views (a checkpoint trim, a replay) read everyone again; a refresh that began
    // before the forget publishes nothing.
    let (reads, full) = store_reads(&b);
    b.forget_current_views();
    assert!(b.smalltalk.arrangement_list.published.lock().unwrap().is_none());
    assert_eq!(refresh_and_check(&b, OwnerView::Arrangements).owners, before.owners);
    assert_eq!(store_reads(&b).1, full + 1);
    assert!(store_reads(&b).0 > reads);
}

fn store_reads(store: &Store) -> (u64, u64) {
    store.owner_list_reads(OwnerView::Arrangements)
}
