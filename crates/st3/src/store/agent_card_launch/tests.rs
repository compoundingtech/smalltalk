use super::*;
const AGENT: &str = "agent/amber.sample";
// Obtain an opaque context from a real Installer callback. This fixture operator cannot
// publish, and its test-only source registration supplies no production source coverage.
fn context(store: &Store) -> Namespace {
    use smallclaims::ivm::install::{Installer, Limits, Mutation, Operator, ScanPage};
    struct Capture {
        name: &'static str,
        context: Arc<std::sync::Mutex<Option<Namespace>>>,
    }
    impl Operator for Capture {
        fn name(&self) -> &'static str {
            self.name
        }
        fn fingerprint(&self) -> &'static str {
            FINGERPRINT
        }
        fn source(&self) -> &'static str {
            "launch-fixture-only"
        }
        fn create_schema(&self, c: &Connection) -> Result<()> {
            create_schema(c)
        }
        fn apply(&self, _: &Transaction<'_>, ns: &Namespace, _: &[Mutation]) -> Result<bool> {
            *self.context.lock().unwrap() = Some(ns.clone());
            Ok(false)
        }
        fn validate_publication(&self, _: &Transaction<'_>, _: &Namespace) -> Result<()> {
            anyhow::bail!("namespace fixture cannot certify a public card")
        }
        fn reclaim(&self, _: &Transaction<'_>, _: &Namespace, _: usize) -> Result<bool> {
            Ok(true)
        }
    }
    let slot = Arc::new(std::sync::Mutex::new(None));
    let name = Box::leak(format!("launch-fixture/{}", uuid::Uuid::now_v7()).into_boxed_str());
    let installer = Installer::new(vec![Box::new(Capture {
        name,
        context: slot.clone(),
    })])
    .unwrap();
    installer.create_schema(&store.connection.write()).unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    if installer.position(&tx, "launch-fixture-only").is_err() {
        installer
            .register_source(&tx, "launch-fixture-only", "fixture-only", 1)
            .unwrap();
    }
    let job = installer
        .start(
            &tx,
            name,
            Limits {
                page_rows: 2,
                page_bytes: 1024,
                pending_rows: 8,
                pending_bytes: 4096,
                total_rows: 16,
                callback_ms: 1000,
                lifetime_ms: 10000,
            },
            0,
        )
        .unwrap();
    installer
        .scan(
            &tx,
            &ScanPage {
                job: job.clone(),
                expected_cursor: vec![],
                next_cursor: vec![1],
                position: installer.position(&tx, "launch-fixture-only").unwrap(),
                rows: vec![Mutation {
                    key: "fixture".into(),
                    old: None,
                    new: Some(Value::Null),
                }],
                finished: false,
            },
            0,
        )
        .unwrap();
    installer.cancel(&tx, &job).unwrap();
    tx.commit().unwrap();
    slot.lock().unwrap().clone().unwrap()
}

fn declare(store: &Store, workspace: &str) -> ClaimRecord {
    let mut intent=crate::parse_intent("version 2\nagent \"sample\" { host \"amber\"; workspace \"/sample\"; harness \"omp\" { model \"canary\" } }","amber").unwrap();
    intent
        .subjects
        .get_mut(AGENT)
        .unwrap()
        .member
        .as_mut()
        .unwrap()
        .workspace = workspace.into();
    store
        .apply_internal(&intent, "launch-fixture-declare")
        .unwrap();
    store
        .claim_by_id(&store.selected_desired_token(AGENT).unwrap().unwrap())
        .unwrap()
        .unwrap()
}
fn fixture() -> Store {
    let store = Store::open_memory("amber").unwrap();
    store.set_write_clock_at(1_800_000_000_000).unwrap();
    declare(&store, "/sample");
    store
}
fn receipt(store: &Store, token: &str, inc: &str) -> ClaimRecord {
    store
        .append_claim(&ClaimInput {
            subject: AGENT.into(),
            kind: "runtime.action.succeeded".into(),
            actor: Some("person/test".into()),
            fields: serde_json::from_value(
                json!({"action":"start","desired_token":token,"incarnation_id":inc}),
            )
            .unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
}
fn observation(store: &Store, c: &ClaimRecord) -> Observation {
    let order = if c.id.starts_with(LOCAL_OBSERVATION_ID_PREFIX) {
        Order::Local {
            physical_id: c.id.rsplit_once('/').unwrap().1.parse().unwrap(),
        }
    } else {
        Order::Claim(canonical::claim_key(&store.readers.get(), &c.id).unwrap())
    };
    Observation {
        record: c.clone(),
        order,
    }
}
fn local(store: &Store, id: i64, index: u64, token: &str, inc: &str) -> ClaimRecord {
    // Isolated legacy local-source fixture: observations_for accepts physical rows for
    // this kind even though new runtime.action.succeeded claims are normally durable.
    let c = store.connection.write();
    c.execute("INSERT INTO local_observations(id,after_store_index,subject,kind,actor,body,observed_at_unix_ms) VALUES(?1,?2,?3,'runtime.action.succeeded','person/test',?4,1800000000000)",params![id,index,AGENT,serde_json::to_string(&json!({"fields":{"action":"start","desired_token":token,"incarnation_id":inc}})).unwrap()]).unwrap();
    c.query_row(
        &format!("{LOCAL_OBSERVATION_COLUMNS} WHERE id=?1"),
        [id],
        |r| local_observation_from_row(&store.origin, r),
    )
    .unwrap()
}
fn key(inc: &str) -> Key {
    Key {
        agent: AGENT.into(),
        incarnation: inc.into(),
    }
}
fn finish(tx: &Transaction<'_>, ns: &Namespace) {
    let mut after = None;
    for _ in 0..256 {
        let page = flush_page(tx, ns, after.as_ref(), 2).unwrap();
        assert!(
            page.missing.is_empty(),
            "fixture token input not captured: {:?}",
            page.missing
        );
        if page.complete {
            return;
        }
        after = page.after;
    }
    panic!("launch fixture did not close");
}
fn install(store: &Store) -> Namespace {
    let ns = context(store);
    let (observations, tokens) = store
        .read_snapshot(|_| {
            let records = store.observations_for(AGENT, "runtime.action.succeeded")?;
            let mut tokens = BTreeMap::new();
            let mut obs = Vec::new();
            for c in records {
                let fields = c.body.get("fields").unwrap_or(&c.body);
                if let Some(id) = fields["desired_token"].as_str() {
                    tokens.insert(id.to_owned(), store.claim_by_id(id)?);
                }
                obs.push(observation(store, &c));
            }
            Ok((obs, tokens))
        })
        .unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    for (id, c) in tokens {
        apply_token(&tx, &ns, &id, c.as_ref()).unwrap();
    }
    for obs in observations {
        apply_observation(&tx, &ns, None, Some(&obs)).unwrap();
    }
    finish(&tx, &ns);
    tx.commit().unwrap();
    ns
}
fn project(store: &Store, ns: &Namespace, old: Option<&Observation>, new: Option<&Observation>) {
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    apply_observation(&tx, ns, old, new).unwrap();
    finish(&tx, ns);
    tx.commit().unwrap();
}
fn parity(store: &Store, ns: &Namespace, inc: &str) -> Option<(String, MemberSpec)> {
    let got = read_selected(&store.readers.get(), ns, &key(inc)).unwrap();
    assert_eq!(
        got,
        crate::rollout::launched_member(store, AGENT, inc).unwrap()
    );
    got
}
#[test]
fn store_receipt_position_dominates_canonical_time_and_incarnation_isolation() {
    let store = fixture();
    let first = declare(&store, "/first");
    receipt(&store, &first.id, "one");
    let second = declare(&store, "/second");
    let mut later = receipt(&store, &second.id, "one");
    // Isolated legacy source changes canonical time without changing receipt order.
    later.accepted_at_unix_ms = 1;
    store
        .connection
        .write()
        .execute(
            "UPDATE claims SET accepted_at_unix_ms=1 WHERE id=?1",
            [&later.id],
        )
        .unwrap();
    receipt(&store, &first.id, "two");
    let ns = install(&store);
    assert_eq!(parity(&store, &ns, "one").unwrap().0, second.id);
    assert_eq!(parity(&store, &ns, "two").unwrap().0, first.id);
    assert!(parity(&store, &ns, "unknown").is_none());
}
#[test]
fn local_numeric_position_and_stable_zero_negative_ties_match_observations_for() {
    let store = fixture();
    let a = declare(&store, "/first");
    let b = declare(&store, "/second");
    let c = receipt(&store, &a.id, "one");
    local(&store, -1, c.store_index, &a.id, "one");
    local(&store, 0, c.store_index, &b.id, "one");
    let ns = install(&store);
    assert_eq!(parity(&store, &ns, "one").unwrap().0, b.id);
    let c = local(&store, 2, c.store_index, &a.id, "one");
    let obs = observation(&store, &c);
    project(&store, &ns, None, Some(&obs));
    assert_eq!(parity(&store, &ns, "one").unwrap().0, a.id);
    let c = local(&store, 10, c.store_index, &b.id, "one");
    let obs = observation(&store, &c);
    project(&store, &ns, None, Some(&obs));
    assert_eq!(parity(&store, &ns, "one").unwrap().0, b.id);
    let later = receipt(&store, &a.id, "one");
    let obs = observation(&store, &later);
    project(&store, &ns, None, Some(&obs));
    assert_eq!(parity(&store, &ns, "one").unwrap().0, a.id);
}
#[test]
fn malformed_nonmember_and_known_missing_tokens_skip_to_older_valid_candidate() {
    let store = fixture();
    let valid = declare(&store, "/valid");
    receipt(&store, &valid.id, "one");
    let malformed = declare(&store, "/malformed");
    receipt(&store, &malformed.id, "one");
    let empty = declare(&store, "/empty");
    receipt(&store, &empty.id, "one");
    receipt(&store, "claim/known-missing", "one");
    // Finish normal writes before introducing legacy malformed source bodies.
    store
        .connection
        .write()
        .execute("UPDATE claims SET body='{}' WHERE id=?1", [&malformed.id])
        .unwrap();
    let mut body = empty.body.clone();
    body["member"] = Value::Null;
    store
        .connection
        .write()
        .execute(
            "UPDATE claims SET body=?2 WHERE id=?1",
            params![empty.id, serde_json::to_string(&body).unwrap()],
        )
        .unwrap();
    let ns = install(&store);
    assert_eq!(parity(&store, &ns, "one").unwrap().0, valid.id);
}
#[test]
fn uncaptured_token_waits_and_later_global_body_cannot_repair_without_namespace_capture() {
    let store = fixture();
    let old = declare(&store, "/old");
    receipt(&store, &old.id, "one");
    let ns = install(&store);
    let source = Store::open_memory("birch").unwrap();
    let new = declare(&source, "/new");
    let c = receipt(&store, &new.id, "one");
    let obs = observation(&store, &c);
    {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        apply_observation(&tx, &ns, None, Some(&obs)).unwrap();
        let page = flush_page(&tx, &ns, None, 1).unwrap();
        assert_eq!(page.missing, BTreeSet::from([new.id.clone()]));
        assert!(!page.complete);
        assert!(read_selected(&tx, &ns, &key("one")).is_err());
        tx.commit().unwrap();
    }
    store
        .import_replication("birch", &source.export_replication(0).unwrap())
        .unwrap();
    {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        assert!(!flush_page(&tx, &ns, None, 1).unwrap().complete);
        // Known live absence at this namespace cut skips the newer candidate, even with
        // a body visible in a later global snapshot. Only explicit capture promotes it.
        apply_token(&tx, &ns, &new.id, None).unwrap();
        finish(&tx, &ns);
        assert_eq!(
            read_selected(&tx, &ns, &key("one")).unwrap().unwrap().0,
            old.id
        );
        let changed = apply_token(&tx, &ns, &new.id, Some(&new)).unwrap();
        assert_eq!(changed, BTreeSet::from([key("one")]));
        finish(&tx, &ns);
        tx.commit().unwrap();
    }
    assert_eq!(parity(&store, &ns, "one").unwrap().0, new.id);
}
#[test]
fn live_token_body_has_no_extra_subject_kind_filter_and_checkpoint_only_is_not_live() {
    let store = fixture();
    let old = declare(&store, "/old");
    receipt(&store, &old.id, "one");
    let mut new = declare(&store, "/new");
    receipt(&store, &new.id, "one");
    // Legacy live token bodies can be used even when their raw metadata differs.
    store.connection.write().execute("UPDATE claims SET subject='agent/garden/foreign-token',kind='harness.observed' WHERE id=?1",[&new.id]).unwrap();
    new.subject = "agent/garden/foreign-token".into();
    new.kind = "harness.observed".into();
    let ns = install(&store);
    assert_eq!(parity(&store, &ns, "one").unwrap().0, new.id);
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    tx.execute("INSERT INTO checkpoint_claims(id,writer,sequence,envelope_hash,subject,kind,actor,predecessors,accepted_at_unix_ms,checkpoint) VALUES(?1,'amber',1,'fixture',?2,?3,'person/test',?4,?5,'fixture')",params![new.id,new.subject,new.kind,serde_json::to_string(&new.predecessors).unwrap(),new.accepted_at_unix_ms as i64]).unwrap();
    // This source fixture's desired row references the token, so simulate removal as
    // the physical hook does and update the global source after dropping that reference.
    tx.execute("DELETE FROM desired WHERE claim_id=?1", [&new.id])
        .unwrap();
    tx.execute(
        "DELETE FROM operations WHERE canonical_claim_id=?1",
        [&new.id],
    )
    .unwrap();
    tx.execute("DELETE FROM claims WHERE id=?1", [&new.id])
        .unwrap();
    apply_token(&tx, &ns, &new.id, None).unwrap();
    finish(&tx, &ns);
    tx.commit().unwrap();
    drop(writer);
    assert_eq!(parity(&store, &ns, "one").unwrap().0, old.id);
}
#[test]
fn old_new_receipt_metadata_correction_and_local_deletion_repair_affected_pairs() {
    let store = fixture();
    let a = declare(&store, "/a");
    let b = declare(&store, "/b");
    receipt(&store, &a.id, "one");
    let c = receipt(&store, &b.id, "one");
    let old = observation(&store, &c);
    let ns = install(&store);
    let mut corrected = c.clone();
    corrected.body["fields"]["incarnation_id"] = json!("two");
    store
        .connection
        .write()
        .execute(
            "UPDATE claims SET body=?2 WHERE id=?1",
            params![c.id, serde_json::to_string(&corrected.body).unwrap()],
        )
        .unwrap();
    let new = observation(&store, &corrected);
    project(&store, &ns, Some(&old), Some(&new));
    assert_eq!(parity(&store, &ns, "one").unwrap().0, a.id);
    assert_eq!(parity(&store, &ns, "two").unwrap().0, b.id);
    let c = local(&store, 20, store.index().unwrap(), &b.id, "one");
    let obs = observation(&store, &c);
    project(&store, &ns, None, Some(&obs));
    assert_eq!(parity(&store, &ns, "one").unwrap().0, b.id);
    store
        .connection
        .write()
        .execute("DELETE FROM local_observations WHERE id=20", [])
        .unwrap();
    project(&store, &ns, Some(&obs), None);
    assert_eq!(parity(&store, &ns, "one").unwrap().0, a.id);
}
#[test]
fn launch_changes_comparison_preserves_label_environment_and_restart_only_changes() {
    let store = fixture();
    let token = declare(&store, "/same");
    receipt(&store, &token.id, "one");
    let ns = install(&store);
    let old = parity(&store, &ns, "one").unwrap().1;
    let mut member = old.clone();
    member.display_name = Some("A different label".into());
    member
        .environment
        .insert("FIXTURE_VARIABLE".into(), "different".into());
    member.tags.insert("fixture".into(), "different".into());
    member.restart = crate::model::RestartType::Never;
    member.restart_intensity.attempts += 1;
    assert!(member.launch_changes(&old).is_empty());
    member.workspace = "/changed".into();
    assert_eq!(member.launch_changes(&old), vec!["workspace"]);
}
#[test]
fn namespace_isolation_rollback_and_total_bounded_reclamation() {
    let store = fixture();
    let a = declare(&store, "/a");
    receipt(&store, &a.id, "one");
    let old = install(&store);
    let other = install(&store);
    let before = parity(&store, &old, "one");
    {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        apply_token(&tx, &old, &a.id, None).unwrap();
        finish(&tx, &old);
        assert!(read_selected(&tx, &old, &key("one")).unwrap().is_none());
        assert_eq!(read_selected(&tx, &other, &key("one")).unwrap(), before);
        tx.rollback().unwrap();
    }
    assert_eq!(parity(&store, &old, "one"), before);
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    for _ in 0..256 {
        if reclaim_namespace(&tx, &old, 1).unwrap() {
            break;
        }
    }
    assert_eq!(read_selected(&tx, &other, &key("one")).unwrap(), before);
    assert!(read_selected(&tx, &old, &key("one")).unwrap().is_none());
    tx.commit().unwrap();
}
#[test]
fn unresolved_token_budget_is_shared_across_all_pairs_in_a_repair_page() {
    let store = fixture();
    let ns = context(&store);
    let mut observations = Vec::new();
    for inc in ["aaa", "bbb"] {
        for n in 0..100 {
            let c = receipt(&store, &format!("claim/missing-{inc}-{n:03}"), inc);
            observations.push(observation(&store, &c));
        }
    }
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    for obs in &observations {
        apply_observation(&tx, &ns, None, Some(obs)).unwrap();
    }
    let first = flush_page(&tx, &ns, None, BOUND).unwrap();
    assert_eq!(first.missing.len(), BOUND);
    assert_eq!(first.after, Some(key("bbb")));
    assert!(!first.complete);
    for id in first.missing {
        apply_token(&tx, &ns, &id, None).unwrap();
    }
    let second = flush_page(&tx, &ns, None, BOUND).unwrap();
    assert_eq!(second.missing.len(), 72);
    assert_eq!(second.changed, BTreeSet::from([key("aaa")]));
    for id in second.missing {
        apply_token(&tx, &ns, &id, None).unwrap();
    }
    finish(&tx, &ns);
    ensure_closed(&tx, &ns).unwrap();
    tx.commit().unwrap();
    drop(writer);
    assert!(parity(&store, &ns, "aaa").is_none());
    assert!(parity(&store, &ns, "bbb").is_none());
}
#[test]
fn reverse_token_fanout_fences_all_pairs_and_deferred_cursor_advances() {
    let store = fixture();
    let token = declare(&store, "/shared");
    for n in 0..=BOUND {
        receipt(&store, &token.id, &format!("inc-{n:03}"));
    }
    let ns = install(&store);
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    assert_eq!(apply_token(&tx, &ns, &token.id, None).unwrap().len(), BOUND);
    assert!(ensure_closed(&tx, &ns).is_err());
    assert!(read_selected(&tx, &ns, &key("inc-128")).is_err());
    assert!(flush_page(&tx, &ns, None, 129).is_err());
    tx.rollback().unwrap();
    drop(writer);
    let ns = context(&store);
    let a = receipt(&store, "claim/uncaptured-token", "aaa");
    let b = receipt(&store, &token.id, "zzz");
    let ao = observation(&store, &a);
    let bo = observation(&store, &b);
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    apply_token(&tx, &ns, &token.id, Some(&token)).unwrap();
    apply_observation(&tx, &ns, None, Some(&ao)).unwrap();
    apply_observation(&tx, &ns, None, Some(&bo)).unwrap();
    let first = flush_page(&tx, &ns, None, 1).unwrap();
    assert_eq!(first.after, Some(key("aaa")));
    assert!(!first.complete);
    let second = flush_page(&tx, &ns, first.after.as_ref(), 1).unwrap();
    assert_eq!(second.changed, BTreeSet::from([key("zzz")]));
    assert!(!second.complete);
    apply_token(&tx, &ns, "claim/uncaptured-token", None).unwrap();
    finish(&tx, &ns);
    tx.commit().unwrap();
}
