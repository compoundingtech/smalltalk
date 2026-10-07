//! Application-shaped Store fixtures, with independent raw-history oracles.
//! Restricted relations; each production consumer must preserve its complete input policy.
#[path = "support/desired_wakes.rs"]
mod desired_wakes;
#[path = "support/fleet_view.rs"]
mod fleet_view;
#[path = "support/history.rs"]
mod history;
#[path = "support/limits_view.rs"]
mod limits_view;
#[path = "support/real_views.rs"]
mod real_views;
use anyhow::Result;
use real_views::{Family, collection, desired_candidates, row};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use smallclaims::{
    ClaimInput, ClaimRecord, Store,
    fleet::MemberKey,
    ivm::{LocalChange, Readiness, Views, runtime::ViewRuntime, source_cut},
    replication::ReplicationInventory,
    store::Runtime,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

const RUN: &str = "mission-run/fixture";
const CHILD_RUN: &str = "mission-run/fixture/child";
const STEPS: &[history::Step] = &[
    ("step-run/fixture/build", RUN, "build", Some(RUN)),
    ("step-run/fixture/review", RUN, "review", Some(RUN)),
    (
        "step-run/fixture/child/check",
        CHILD_RUN,
        "check",
        Some(CHILD_RUN),
    ),
];
const NAMES: &[&str] = &[
    "agent-card",
    "account-limits",
    "fleet-membership",
    "mailbox",
    "mission-tree",
    "desired-by-host",
];
struct Node {
    store: Store,
    runtime: Arc<ViewRuntime>,
    anchor: String,
    local_signer: Option<String>,
}
fn definitions(origin: &str, anchor: &str, local_signer: Option<&str>) -> Views {
    // A test-local static fingerprint includes pinned authority configuration, not just code.
    let fingerprint = Box::leak(
        format!("anchor-direct.v3;anchor={anchor};root=alder;signer-source.v2").into_boxed_str(),
    );
    Views::new(vec![
        Box::new(Family::Card),
        Box::new(limits_view::AccountLimits),
        Box::new(fleet_view::AnchorFleet {
            root: "alder".into(),
            anchor: anchor.into(),
            local_origin: origin.into(),
            local_signer: local_signer.map(str::to_owned),
            fingerprint,
        }),
        Box::new(Family::Mailbox),
        Box::new(Family::Tree),
        Box::new(Family::Desired),
    ])
    .unwrap()
}
fn node(origin: &str, anchor: &str, signer: Option<&str>) -> Node {
    let runtime = Arc::new(ViewRuntime::new(definitions(origin, anchor, signer)).unwrap());
    let store = Store::open_memory(origin, runtime.clone()).unwrap();
    store.bind_fleet("fixture-fleet").unwrap();
    store.pin_fleet_anchor(anchor).unwrap();
    install_steps(&store, &runtime);
    install_mutation_witness(&store);
    Node {
        store,
        runtime,
        anchor: anchor.into(),
        local_signer: signer.map(str::to_owned),
    }
}
fn install_mutation_witness(store: &Store) {
    // Correctness instrumentation only. These triggers must be omitted from a cost unit.
    let c = store.connection.write();
    c.execute_batch(
        "CREATE TABLE fixture_projection_updates(name TEXT PRIMARY KEY,n INTEGER NOT NULL)",
    )
    .unwrap();
    for table in [
        "app_rows",
        "app_work",
        "app_work_inputs",
        "app_unread",
        "app_prior_hosts",
        "app_limit_candidates",
        "app_limit_nodes",
        "app_fleet_facts",
        "app_fleet_signers",
        "ivm_views",
        "ivm_heads",
        "ivm_contributions",
        "ivm_claim_views",
        "ivm_claim_ranks",
        "ivm_keys",
        "ivm_view_errors",
        "ivm_view_status",
    ] {
        for action in ["INSERT", "UPDATE", "DELETE"] {
            c.execute_batch(&format!(
                "CREATE TRIGGER fixture_{table}_{action} AFTER {action} ON {table} BEGIN
                INSERT INTO fixture_projection_updates VALUES('{table}',1)
                ON CONFLICT(name) DO UPDATE SET n=n+1; END;"
            ))
            .unwrap();
        }
    }
}
fn projection_updates(n: &Node) -> BTreeMap<String, u64> {
    n.store
        .readers
        .get()
        .prepare("SELECT name,n FROM fixture_projection_updates ORDER BY name")
        .unwrap()
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}
fn ordinary() -> Node {
    node("alder", "unconfigured-test-anchor", None)
}
fn install_steps(store: &Store, runtime: &ViewRuntime) {
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    for (id, run, path, parent) in STEPS {
        tx.execute(
            "INSERT INTO app_steps VALUES(?1,?2,?3,?4,'pending')",
            params![id, run, path, parent],
        )
        .unwrap();
    }
    let prior = source_cut(&tx).unwrap().unwrap();
    let changes = runtime
        .views
        .local_change(
            &tx,
            &LocalChange {
                kind: "mission.layout.install".into(),
                old_keys: BTreeSet::new(),
                new_keys: STEPS.iter().map(|s| s.0.to_owned()).collect(),
                evaluation_time_unix_ms: 0,
            },
            smallclaims::ivm::SourceCut {
                local_generation: prior.local_generation + 1,
                ..prior
            },
        )
        .unwrap();
    assert!(changes.deferred.is_empty());
    assert_eq!(changes.changed.len(), STEPS.len());
    tx.commit().unwrap();
}
fn append(n: &Node, subject: &str, kind: &str, actor: Option<&str>, f: Value) -> ClaimRecord {
    n.store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: actor.map(str::to_owned),
            fields: serde_json::from_value(f).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
}
fn append_raw(n: &Node, body: Value) -> ClaimRecord {
    let mut writer = n.store.connection.write();
    let tx = writer.transaction().unwrap();
    let claim = n
        .runtime
        .append_claim_tx(
            &tx,
            &n.store.origin,
            body["subject"].as_str().unwrap(),
            "intent.desired",
            Some("person/fixture"),
            &body,
            &[],
            None,
        )
        .unwrap();
    tx.commit().unwrap();
    claim
}
fn rows(c: &Connection, view: &str) -> history::Rows {
    c.prepare("SELECT id,payload FROM app_rows WHERE view=?1 ORDER BY id")
        .unwrap()
        .query_map([view], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .unwrap()
        .map(|r| {
            let (id, payload) = r.unwrap();
            (id, serde_json::from_str(&payload).unwrap())
        })
        .collect()
}
fn semantic_tokens(n: &Node) -> Vec<smallclaims::ivm::Token> {
    n.store
        .read_snapshot(|_| {
            NAMES
                .iter()
                .map(|name| n.runtime.views.token(&n.store.readers.get(), name, 1))
                .collect()
        })
        .unwrap()
}
fn check(n: &Node) {
    n.store
        .read_snapshot(|_| {
            let c = n.store.readers.get();
            for name in NAMES {
                assert!(
                    matches!(n.runtime.views.readiness(&c, name, 1)?, Readiness::Ready(_)),
                    "{name}: {:?}",
                    n.runtime.views.availability(&c, name, 1)?
                );
            }
            let claims = history::raw(&c)?;
            assert_eq!(rows(&c, "agent-card"), history::card(&claims));
            let (mail, counts) = history::mailbox(&claims);
            assert_eq!(rows(&c, "mailbox"), mail);
            let actual_counts = c
                .prepare("SELECT recipient,count FROM app_unread ORDER BY recipient")?
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
            // A former recipient's zero is a retained counter tombstone, equivalent to absence.
            assert_eq!(
                actual_counts
                    .into_iter()
                    .filter(|(_, count)| *count != 0)
                    .collect::<BTreeMap<_, _>>(),
                counts
                    .into_iter()
                    .filter(|(_, count)| *count != 0)
                    .collect::<BTreeMap<_, _>>()
            );
            assert_eq!(rows(&c, "mission-tree"), history::tree(&claims, STEPS));
            assert_eq!(rows(&c, "account-limits"), history::limits(&claims));
            let (desired, prior) = history::desired(&claims);
            assert_eq!(rows(&c, "desired-by-host"), desired);
            let actual_prior = c
                .prepare("SELECT subject,host FROM app_prior_hosts ORDER BY subject,host")?
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            assert_eq!(
                actual_prior,
                prior
                    .into_iter()
                    .flat_map(|(s, hosts)| hosts.into_iter().map(move |h| (s.clone(), h)))
                    .collect::<Vec<_>>()
            );
            let local = n
                .local_signer
                .as_deref()
                .map(|key| (n.store.origin.as_str(), key));
            assert_eq!(rows(&c, "fleet-membership"), history::fleet(&c, local)?);
            Ok(())
        })
        .unwrap();
}
fn noise(n: &Node, subject: &str) {
    let before = semantic_tokens(n);
    let updates = projection_updates(n);
    append(
        n,
        subject,
        "example.future",
        None,
        json!({"unrecognized":true}),
    );
    append(
        n,
        subject,
        "work.future",
        Some("agent/fixture/a"),
        json!({"status":"invented"}),
    );
    assert_eq!(semantic_tokens(n), before);
    assert_eq!(
        projection_updates(n),
        updates,
        "unknown kinds touched projection rows"
    );
    check(n);
}
fn replicate_permutations(n: &Node, anchor_prefix: u64) {
    let exchange = n
        .store
        .export_replication_exchange("fixture-fleet", &ReplicationInventory::default())
        .unwrap();
    check(n);
    let mut orders = BTreeSet::new();
    for variant in 0..3 {
        let target = node(["birch", "cedar", "dahlia"][variant], &n.anchor, None);
        let mut delivery_order = exchange.envelopes.clone();
        delivery_order.sort_by_key(|e| e.sequence);
        let split = delivery_order.partition_point(|e| e.sequence <= anchor_prefix);
        let mut tail = delivery_order.split_off(split);
        match variant {
            1 => tail.reverse(),
            2 => {
                let len = tail.len();
                if len > 1 {
                    tail.rotate_left(len / 2);
                }
            }
            _ => {}
        }
        let require_distinct = tail.len() >= 3;
        delivery_order.extend(tail);
        if require_distinct {
            assert!(
                orders.insert(
                    delivery_order
                        .iter()
                        .map(|e| e.sequence)
                        .collect::<Vec<_>>()
                )
            );
        }
        for envelope in delivery_order {
            let mut delivery = exchange.clone();
            delivery.envelopes = vec![envelope];
            target
                .store
                .receive_replication_exchange("alder", "fixture-fleet", &delivery)
                .unwrap();
            target.store.validate_replication_backlog().unwrap();
            target.store.project_replication_backlog().unwrap();
            check(&target);
            let before = semantic_tokens(&target);
            target
                .store
                .receive_replication_exchange("alder", "fixture-fleet", &delivery)
                .unwrap();
            target.store.validate_replication_backlog().unwrap();
            target.store.project_replication_backlog().unwrap();
            assert_eq!(semantic_tokens(&target), before);
            check(&target);
        }
        for view in NAMES {
            assert_eq!(
                rows(&target.store.readers.get(), view),
                rows(&n.store.readers.get(), view)
            );
        }
        assert_eq!(
            history::raw(&target.store.readers.get()).unwrap().len(),
            history::raw(&n.store.readers.get()).unwrap().len()
        );
    }
}

#[test]
fn agent_card_incarnation_status_cumulative_usage_and_actor_work_converge() {
    let n = ordinary();
    let a = "agent/fixture/a";
    for (subject, kind, actor, f) in [
        (
            a,
            "runtime.observed",
            None,
            json!({"incarnation_id":"i1","status":"running"}),
        ),
        (
            a,
            "harness.observed",
            None,
            json!({"incarnation_id":"i1","state":"idle","provider_auth":false}),
        ),
        (
            a,
            "harness.usage",
            None,
            json!({"incarnation_id":"i1","semantics":"session_cumulative","total_tokens":100}),
        ),
        (
            a,
            "harness.observed",
            None,
            json!({"incarnation_id":"i1","state":"idle"}),
        ),
        (
            STEPS[0].0,
            "work.progress",
            Some(a),
            json!({"claim_incarnation":"i1","status":"claimed"}),
        ),
        (
            a,
            "runtime.observed",
            None,
            json!({"incarnation_id":"i2","status":"running"}),
        ),
        (
            a,
            "harness.usage",
            None,
            json!({"incarnation_id":"i1","semantics":"session_cumulative","total_tokens":200}),
        ),
        (
            a,
            "harness.observed",
            None,
            json!({"incarnation_id":"i2","state":"working","provider_auth":true}),
        ),
        (
            a,
            "harness.usage",
            None,
            json!({"incarnation_id":"i2","semantics":"session_cumulative","total_tokens":10}),
        ),
        (
            a,
            "harness.usage",
            None,
            json!({"incarnation_id":"i2","semantics":"session_cumulative","total_tokens":3}),
        ),
        (
            STEPS[1].0,
            "work.claimed",
            Some(a),
            json!({"claim_incarnation":"i2","status":"claimed"}),
        ),
        (
            STEPS[1].0,
            "work.submitted",
            Some(a),
            json!({"claim_incarnation":"i2","status":"verifying"}),
        ),
    ] {
        append(&n, subject, kind, actor, f);
        check(&n);
        if kind == "work.progress" {
            assert_eq!(
                row(&n.store.readers.get(), "agent-card", a)
                    .unwrap()
                    .unwrap()["status"],
                "needs-login"
            );
        }
    }
    let card = row(&n.store.readers.get(), "agent-card", a)
        .unwrap()
        .unwrap();
    assert_eq!(card["usage"], 10);
    assert_eq!(card["incarnation"], "i2");
    assert_eq!(card["status"], "working");
    assert_eq!(card["work"], json!([]));
    noise(&n, a);
    noise(&n, STEPS[0].0);
    replicate_permutations(&n, 0);
}

fn reading(
    driver: &str,
    account: &str,
    time: u64,
    weekly: Option<f64>,
    reset: Option<u64>,
) -> Value {
    json!({"driver":driver,"account":account,"account_ref":Value::Null,"weekly_percent":weekly,
        "weekly_resets_at_unix_ms":reset,"five_hour_percent":10.0,"five_hour_resets_at_unix_ms":50,
        "measured_at_unix_ms":time})
}
#[test]
fn account_limits_original_weekly_time_reset_hour_boundary_and_provider_partition() {
    let n = ordinary();
    let mut newest = None;
    for (seat, f) in [
        (
            "agent/fixture/old",
            reading("claude", "acct", 1_000_000, Some(100.0), Some(100)),
        ),
        (
            "agent/fixture/a",
            reading("claude", "acct", 10_000_000, Some(97.0), Some(100)),
        ),
        (
            "agent/fixture/b",
            reading("claude", "acct", 11_000_000, Some(20.0), Some(100)),
        ),
        (
            "agent/fixture/b",
            reading("claude", "acct", 24_000_000, None, None),
        ),
    ] {
        newest = Some(append(&n, seat, "harness.limits", None, f));
        check(&n);
    }
    let key = limits_view::account_key(&reading("claude", "acct", 0, None, None));
    let selected = row(&n.store.readers.get(), "account-limits", &key)
        .unwrap()
        .unwrap();
    assert_eq!(selected["weekly_percent"], 97.0);
    assert_eq!(selected["measured_at_unix_ms"], 10_000_000);
    // A latest-claim or latest-per-seat-only oracle would choose the partial/latest low reading.
    assert!(newest.unwrap().body["fields"]["weekly_percent"].is_null());
    for (seat, f) in [
        (
            "agent/fixture/b",
            reading("claude", "acct", 11_500_000, Some(4.0), Some(200)),
        ),
        (
            "agent/fixture/a",
            reading("claude", "acct", 12_000_000, Some(99.0), Some(100)),
        ),
        (
            "agent/fixture/c",
            reading("codex", "acct", 5_000_000, Some(98.0), Some(300)),
        ),
        (
            "agent/fixture/d",
            reading("codex", "acct", 8_600_000, Some(10.0), Some(300)),
        ),
    ] {
        append(&n, seat, "harness.limits", None, f);
        check(&n);
    }
    assert_eq!(
        row(&n.store.readers.get(), "account-limits", &key)
            .unwrap()
            .unwrap()["weekly_percent"],
        4.0
    );
    let other = limits_view::account_key(&reading("codex", "acct", 0, None, None));
    assert_eq!(
        row(&n.store.readers.get(), "account-limits", &other)
            .unwrap()
            .unwrap()["weekly_percent"],
        98.0
    );
    append(
        &n,
        "agent/fixture/d",
        "harness.limits",
        None,
        reading("codex", "acct", 8_600_001, Some(10.0), Some(300)),
    );
    check(&n);
    assert_eq!(
        row(&n.store.readers.get(), "account-limits", &other)
            .unwrap()
            .unwrap()["weekly_percent"],
        10.0
    );
    for seat in ["agent/fixture/z", "agent/fixture/a"] {
        append(
            &n,
            seat,
            "harness.limits",
            None,
            reading("claude", "acct", 12_000_000, Some(4.0), Some(200)),
        );
        check(&n);
    }
    assert_eq!(
        row(&n.store.readers.get(), "account-limits", &key)
            .unwrap()
            .unwrap()["measured_by"],
        "agent/fixture/z"
    );
    let mut exact_tie = reading("claude", "acct", 12_000_000, Some(4.0), Some(200));
    exact_tie["five_hour_percent"] = json!(99.0);
    append(&n, "agent/fixture/z", "harness.limits", None, exact_tie);
    check(&n);
    assert_eq!(
        row(&n.store.readers.get(), "account-limits", &key)
            .unwrap()
            .unwrap()["five_hour_percent"],
        99.0
    );
    for (time, reset, percent) in [
        (1_000_000, Some(100), Some(20.0)),
        (1_001_000, Some(200), Some(5.0)),
    ] {
        let mut partial = reading("claude", "partial-only", time, None, None);
        partial["five_hour_resets_at_unix_ms"] = json!(reset);
        partial["five_hour_percent"] = json!(percent);
        append(&n, "agent/fixture/partial", "harness.limits", None, partial);
        check(&n);
    }
    let partial = limits_view::account_key(&reading("claude", "partial-only", 0, None, None));
    assert_eq!(
        row(&n.store.readers.get(), "account-limits", &partial)
            .unwrap()
            .unwrap()["five_hour_percent"],
        5.0
    );
    noise(&n, "agent/fixture/a");
    replicate_permutations(&n, 0);
}

#[test]
fn mailbox_unread_counts_read_before_sent_and_recipient_replacement_converge() {
    let n = ordinary();
    for (id, kind, f) in [
        (
            "message/a",
            "message.sent",
            json!({"to":"person/ada","from":"person/blair","body":"first"}),
        ),
        ("message/b", "message.read", json!({"by":"person/ada"})),
        (
            "message/b",
            "message.sent",
            json!({"to":"person/ada","from":"person/blair","body":"second"}),
        ),
        (
            "message/c",
            "message.sent",
            json!({"to":"person/ada","from":"person/blair","body":"third"}),
        ),
        ("message/c", "message.closed", json!({"by":"person/ada"})),
        (
            "message/a",
            "message.sent",
            json!({"to":"person/robin","from":"person/blair","body":"replacement"}),
        ),
    ] {
        append(&n, id, kind, Some("person/ada"), f);
        check(&n);
    }
    let c = n.store.readers.get();
    assert_eq!(collection(&c, "mailbox", "person/robin").unwrap().len(), 1);
    assert_eq!(
        c.query_row(
            "SELECT count FROM app_unread WHERE recipient='person/robin'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    drop(c);
    noise(&n, "message/b");
    replicate_permutations(&n, 0);
}

#[test]
fn mission_tree_immutable_layout_nested_run_and_step_state_converge() {
    let n = ordinary();
    for (id, kind, actor, f) in [
        (
            RUN,
            "mission-run.created",
            None,
            json!({"status":"running","parent_step_run":Value::Null}),
        ),
        (
            STEPS[0].0,
            "step-run.state",
            None,
            json!({"status":"ready","readiness_epoch":1}),
        ),
        (
            STEPS[0].0,
            "work.claimed",
            Some("agent/fixture/a"),
            json!({"status":"claimed","claim_incarnation":"i1"}),
        ),
        (
            CHILD_RUN,
            "mission-run.created",
            None,
            json!({"status":"running","parent_step_run":STEPS[0].0}),
        ),
        (
            STEPS[2].0,
            "work.progress",
            Some("agent/fixture/a"),
            json!({"status":"claimed","claim_incarnation":"i1"}),
        ),
        (
            STEPS[2].0,
            "step-run.state",
            None,
            json!({"status":"completed","readiness_epoch":1}),
        ),
        (
            STEPS[0].0,
            "step-run.state",
            None,
            json!({"status":"completed","readiness_epoch":1}),
        ),
        (
            STEPS[1].0,
            "step-run.state",
            None,
            json!({"status":"ready","readiness_epoch":1}),
        ),
    ] {
        append(&n, id, kind, actor, f);
        check(&n);
    }
    let c = n.store.readers.get();
    assert_eq!(collection(&c, "mission-tree", RUN).unwrap().len(), 3);
    assert_eq!(
        row(&c, "mission-tree", STEPS[2].0).unwrap().unwrap()["state"],
        "completed"
    );
    assert_eq!(
        row(&c, "mission-tree", CHILD_RUN).unwrap().unwrap()["parent"],
        STEPS[0].0
    );
    drop(c);
    noise(&n, STEPS[0].0);
    replicate_permutations(&n, 0);
}

fn desired_body(name: &str, host: Option<&str>) -> Value {
    let subject = format!("agent/fixture/{name}");
    if let Some(host) = host {
        json!({"subject":subject,"kind":"agent",
        "desired":{"name":"agent","arguments":[format!("fixture/{name}")],"children":[
            {"name":"host","arguments":[host]}, {"name":"workspace","arguments":[format!("/unopened-fixture/{name}")]},
            {"name":"command","arguments":["true"]}]},
        "member":{"kind":"agent","host":host,"runtime_id":format!("fixture.{name}"),
            "workspace":format!("/unopened-fixture/{name}"),"workspace_create":false,"cwd":format!("/unopened-fixture/{name}"),
            "terminal":true,"launch":{"type":"shell","value":"true"},"environment":{},"tags":{"st3.subject":subject},
            "display_name":Value::Null,"lifecycle":"service","restart":"always",
            "restart_intensity":{"attempts":3,"interval_ms":60000,"delay_ms":0,"mode":"delay"},
            "shutdown_timeout_ms":5000,"driver":Value::Null}})
    } else {
        json!({"subject":subject,"kind":"stop","desired":{"stop":subject}})
    }
}
#[test]
fn desired_by_host_unowned_reassignment_stop_and_prior_host_candidates_converge() {
    let n = ordinary();
    append_raw(&n, desired_body("a", Some("amber")));
    check(&n);
    append_raw(&n, desired_body("b", Some("cobalt")));
    check(&n);
    append_raw(&n, desired_body("a", Some("cobalt")));
    check(&n);
    let c = n.store.readers.get();
    assert!(
        collection(&c, "desired-by-host", "amber")
            .unwrap()
            .is_empty()
    );
    assert_eq!(desired_candidates(&c, "amber").unwrap().len(), 1);
    drop(c);
    append_raw(&n, desired_body("a", None));
    check(&n);
    let c = n.store.readers.get();
    let away = desired_candidates(&c, "amber").unwrap();
    assert_eq!(away[0]["kind"], "stop");
    assert!(away[0]["host"].is_null());
    drop(c);
    noise(&n, "agent/fixture/a");
    replicate_permutations(&n, 0);
}

#[test]
fn fleet_direct_anchor_signatures_admission_and_late_removal_match_real_membership_fold() {
    let (key, _) = MemberKey::generate().unwrap();
    let key = Arc::new(key);
    let public = key.public().to_owned();
    let n = node("alder", &public, Some(&public));
    n.store.set_member_key(Some(key)).unwrap();
    n.store
        .admit_fleet_anchor("fixture-fleet", &public, "listening")
        .unwrap();
    check(&n);
    let prefix = n.store.writer_head("alder").unwrap().unwrap().0;
    let (child, _) = MemberKey::generate().unwrap();
    append(
        &n,
        "host/birch",
        "fleet.member-admitted",
        None,
        json!({"member_key":child.public(),"via":"member",
        "sponsor":"alder","mode":"listening","writer_floor":0}),
    );
    check(&n);
    append(
        &n,
        "host/birch",
        "fleet.member-removed",
        None,
        json!({"member_key":child.public(),"high_water":10,
        "removed_by":"alder","reason":"fixture"}),
    );
    check(&n);
    assert_eq!(
        row(&n.store.readers.get(), "fleet-membership", "host/birch")
            .unwrap()
            .unwrap()["state"],
        "ended"
    );
    noise(&n, "host/birch");
    replicate_permutations(&n, prefix);
}

#[test]
fn signatures_only_replication_fences_changed_authority_without_claim_frontier_advance() {
    let (key, _) = MemberKey::generate().unwrap();
    let public = key.public().to_owned();
    let source = node("alder", &public, Some(&public));
    source.store.set_member_key(Some(Arc::new(key))).unwrap();
    source
        .store
        .admit_fleet_anchor("fixture-fleet", &public, "listening")
        .unwrap();
    let exchange = source
        .store
        .export_replication_exchange("fixture-fleet", &ReplicationInventory::default())
        .unwrap();
    let target = node("birch", &public, None);
    target
        .store
        .receive_replication_exchange("alder", "fixture-fleet", &exchange)
        .unwrap();
    target.store.validate_replication_backlog().unwrap();
    target.store.project_replication_backlog().unwrap();
    check(&target);
    let before = target
        .runtime
        .views
        .availability(&target.store.readers.get(), "fleet-membership", 1)
        .unwrap();
    let generation = target
        .runtime
        .views
        .token(&target.store.readers.get(), "fleet-membership", 1)
        .unwrap()
        .generation;
    let index = smallclaims::store::current_index(&target.store.readers.get()).unwrap();
    let prior_rows = rows(&target.store.readers.get(), "fleet-membership");
    let envelope = &exchange.envelopes[0];
    let (additional_key, _) = MemberKey::generate().unwrap();
    let message = smallclaims::fleet::envelope_signature_message(
        "fixture-fleet",
        &envelope.writer,
        envelope.sequence,
        &envelope.hash,
    );
    let mut signatures_only = exchange.clone();
    signatures_only.envelopes.clear();
    signatures_only.signatures = vec![smallclaims::ReplicaEnvelopeSignature {
        writer: envelope.writer.clone(),
        sequence: envelope.sequence,
        hash: envelope.hash.clone(),
        member_key: additional_key.public().to_owned(),
        signature: additional_key.sign(&message),
    }];
    let receipt = target
        .store
        .receive_replication_exchange("alder", "fixture-fleet", &signatures_only)
        .unwrap();
    assert_eq!(receipt.received, 0);
    assert_eq!(receipt.signatures, 1);
    let c = target.store.readers.get();
    let after = target
        .runtime
        .views
        .availability(&c, "fleet-membership", 1)
        .unwrap();
    assert_eq!(after.readiness, Readiness::Fenced);
    assert!(after.token.view_sequence > before.token.view_sequence);
    assert_eq!(after.token.source_sequence, before.token.source_sequence);
    assert_eq!(smallclaims::store::current_index(&c).unwrap(), index);
    assert_eq!(
        c.query_row(
            "SELECT generation FROM ivm_views WHERE name='fleet-membership'",
            [],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        generation
    );
    assert_eq!(rows(&c, "fleet-membership"), prior_rows);
    assert!(
        target
            .runtime
            .views
            .token(&c, "fleet-membership", 1)
            .is_err()
    );
    drop(c);
    // Cryptographically valid late evidence is not a new claim. Unsupported authority
    // reevaluation leaves diagnostic rows intact and prevents their use as ready membership.
    let duplicate = target
        .store
        .receive_replication_exchange("alder", "fixture-fleet", &signatures_only)
        .unwrap();
    assert_eq!(duplicate.signatures, 0);
    assert_eq!(
        target
            .runtime
            .views
            .availability(&target.store.readers.get(), "fleet-membership", 1)
            .unwrap(),
        after
    );
}

#[test]
fn unsupported_checkpoint_fails_before_copy_or_history_work_and_preserves_ready_views() {
    let n = ordinary();
    append_raw(&n, desired_body("a", Some("amber")));
    check(&n);
    let before = semantic_tokens(&n);
    let cut = source_cut(&n.store.readers.get()).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let scratch = temp.path().join("must-not-be-created");
    assert!(
        n.store
            .plan_checkpoint(0, &scratch)
            .unwrap_err()
            .to_string()
            .contains("ivm-checkpoint-unavailable")
    );
    assert!(!scratch.exists());
    assert!(
        n.store
            .apply_checkpoint_drop("checkpoint/fixture", &[], &[])
            .unwrap_err()
            .to_string()
            .contains("ivm-checkpoint-unavailable")
    );
    let plan = n
        .runtime
        .plan_checkpoint_drops(&smallclaims::store::checkpoint::SealedSet::default());
    assert!(plan.claims.is_empty() && plan.envelopes.is_empty());
    assert_eq!(
        plan.rules_digest,
        "smallclaims.ivm.checkpoint-unavailable.v1"
    );
    let copy = temp.path().join("must-not-be-opened.sqlite3");
    assert!(
        smallclaims::store::checkpoint::prove_on_copy(
            &*n.runtime,
            &copy,
            &smallclaims::store::checkpoint::SealedSet::default(),
            &plan
        )
        .unwrap_err()
        .to_string()
        .contains("ivm-checkpoint-unavailable")
    );
    assert!(!copy.exists());
    assert_eq!(semantic_tokens(&n), before);
    assert_eq!(source_cut(&n.store.readers.get()).unwrap(), cut);
    check(&n);
}

fn checkpoint_state_rows(n: &Node) -> BTreeMap<String, Vec<Vec<String>>> {
    let c = n.store.readers.get();
    [
        "claims",
        "sqlite_sequence",
        "replica_records",
        "replica_envelopes",
        "checkpoints",
        "checkpoint_envelopes",
        "checkpoint_claims",
        "ivm_views",
        "ivm_source",
        "ivm_view_status",
        "ivm_status_frontier",
        "ivm_keys",
        "app_rows",
    ]
    .into_iter()
    .map(|table| {
        let mut statement = c
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let columns = statement.column_count();
        let rows = statement
            .query_map([], |r| {
                (0..columns)
                    .map(|i| r.get_ref(i).map(|v| format!("{v:?}")))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        (table.to_owned(), rows)
    })
    .collect()
}

#[test]
fn direct_checkpoint_sealing_refuses_unavailable_runtime_but_explicit_backup_remains_available() {
    let n = ordinary();
    append_raw(&n, desired_body("a", Some("amber")));
    check(&n);
    let c = n.store.readers.get();
    assert_eq!(
        c.query_row("SELECT COUNT(*) FROM replica_envelopes", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
    drop(c);
    let before = checkpoint_state_rows(&n);
    let tokens = semantic_tokens(&n);
    let cut = i64::MAX as u128;
    assert!(
        n.store
            .checkpoint_sealed_set(cut)
            .unwrap_err()
            .to_string()
            .contains("ivm-checkpoint-unavailable")
    );
    assert!(
        n.store
            .checkpoint_sealed_set_through(cut, None)
            .unwrap_err()
            .to_string()
            .contains("ivm-checkpoint-unavailable")
    );
    assert!(
        n.store
            .checkpoint_sealed_identities(cut, None)
            .unwrap_err()
            .to_string()
            .contains("ivm-checkpoint-unavailable")
    );
    assert_eq!(checkpoint_state_rows(&n), before);
    assert_eq!(semantic_tokens(&n), tokens);
    // Backup is a general explicit snapshot operation, not checkpoint proof or restore.
    assert!(
        n.store
            .checkpoint_status(cut, &[])
            .unwrap_err()
            .to_string()
            .contains("ivm-checkpoint-unavailable")
    );
    assert!(
        n.store
            .checkpoint_manifest_need()
            .unwrap_err()
            .to_string()
            .contains("ivm-checkpoint-unavailable")
    );
    assert_eq!(checkpoint_state_rows(&n), before);
    // Inspect the resulting raw DB without Store::open or any projection lifecycle callback.
    let temp = tempfile::tempdir().unwrap();
    let copy = temp.path().join("explicit-backup.sqlite3");
    n.store.copy_store_to(&copy).unwrap();
    let raw =
        Connection::open_with_flags(&copy, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(
        raw.query_row("SELECT COUNT(*) FROM claims", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        raw.query_row("SELECT COUNT(*) FROM replica_envelopes", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        rows(&raw, "desired-by-host"),
        rows(&n.store.readers.get(), "desired-by-host")
    );
    assert_eq!(
        source_cut(&raw).unwrap(),
        source_cut(&n.store.readers.get()).unwrap()
    );
    assert_eq!(checkpoint_state_rows(&n), before);
    check(&n);
}

fn checkpoint_drop_fixture(n: &Node) -> smallclaims::store::checkpoint::CheckpointManifest {
    use smallclaims::store::checkpoint::{
        CheckpointManifest, EnvelopeKey, EnvelopeTombstone, SealedClaim, claim_tombstone,
    };
    let claim = append_raw(n, desired_body("a", Some("amber")));
    let exchange = n
        .store
        .export_replication_exchange("fixture-fleet", &ReplicationInventory::default())
        .unwrap();
    assert_eq!(exchange.envelopes.len(), 1);
    let e = &exchange.envelopes[0];
    let key = EnvelopeKey {
        writer: e.writer.clone(),
        sequence: e.sequence,
        envelope_hash: e.hash.clone(),
    };
    CheckpointManifest {
        checkpoint: "checkpoint/fixture".into(),
        cut_unix_ms: e.accepted_at_unix_ms + 1,
        envelopes: vec![EnvelopeTombstone {
            writer: e.writer.clone(),
            sequence: e.sequence,
            envelope_hash: e.hash.clone(),
            accepted_at_unix_ms: e.accepted_at_unix_ms,
        }],
        claims: vec![claim_tombstone(&SealedClaim {
            claim,
            envelope: key,
            valid: true,
            protected: false,
        })],
    }
}

#[test]
fn interrupted_trim_resume_and_checkpoint_step_fail_without_touching_persisted_state() {
    use smallclaims::store::{
        checkpoint::record_checkpoint_tombstones_tx, checkpoint_agreement::CheckpointContext,
    };
    let n = ordinary();
    let manifest = checkpoint_drop_fixture(&n);
    // Model persisted interrupted trim metadata. This does not simulate claim admission,
    // repair or a verified certificate; the runtime must reject before it consumes the state.
    let mut writer = n.store.connection.write();
    let tx = writer.transaction().unwrap();
    record_checkpoint_tombstones_tx(
        &tx,
        &manifest.checkpoint,
        &manifest.envelopes,
        &manifest.claims,
    )
    .unwrap();
    tx.execute("INSERT INTO checkpoints(id,cut_unix_ms,state,updated_at_unix_ms) VALUES(?1,?2,'trimming',0)",params![manifest.checkpoint,i64::try_from(manifest.cut_unix_ms).unwrap()]).unwrap();
    tx.commit().unwrap();
    drop(writer);
    check(&n);
    let rows_before = checkpoint_state_rows(&n);
    let tokens_before = semantic_tokens(&n);
    let mut actions = Vec::new();
    assert!(
        n.store
            .finish_trim(&manifest.checkpoint, &mut actions)
            .unwrap_err()
            .to_string()
            .contains("ivm-checkpoint-unavailable")
    );
    assert!(
        n.store
            .apply_stable_checkpoints(&[], &mut actions)
            .unwrap_err()
            .to_string()
            .contains("ivm-checkpoint-unavailable")
    );
    let temp = tempfile::tempdir().unwrap();
    let scratch = temp.path().join("no-checkpoint-work");
    let context = CheckpointContext {
        now_unix_ms: manifest.cut_unix_ms,
        configured_peers: vec![],
        scratch: scratch.clone(),
        reviewer: "person/fixture".into(),
    };
    assert!(
        n.store
            .checkpoint_step(&context)
            .unwrap_err()
            .to_string()
            .contains("ivm-checkpoint-unavailable")
    );
    assert!(actions.is_empty());
    assert!(!scratch.exists());
    assert_eq!(checkpoint_state_rows(&n), rows_before);
    assert_eq!(semantic_tokens(&n), tokens_before);
    check(&n);
}

#[test]
fn direct_trim_and_resume_override_fail_before_tombstone_state_or_frontier_mutation() {
    let n = ordinary();
    let manifest = checkpoint_drop_fixture(&n);
    let before = checkpoint_state_rows(&n);
    let tokens = semantic_tokens(&n);
    for exact in [false, true] {
        let mut actions = Vec::new();
        assert!(
            n.store
                .trim_checkpoint(
                    &manifest.checkpoint,
                    manifest.cut_unix_ms,
                    "unavailable",
                    &manifest.envelopes,
                    &manifest.claims,
                    exact,
                    &mut actions
                )
                .unwrap_err()
                .to_string()
                .contains("ivm-checkpoint-unavailable")
        );
        assert!(actions.is_empty());
        assert_eq!(checkpoint_state_rows(&n), before);
    }
    assert!(
        n.store
            .resume_checkpoints("person/fixture", "reviewed")
            .unwrap_err()
            .message
            .contains("ivm-checkpoint-unavailable")
    );
    assert!(
        n.store
            .forget_tombstones_for_tests()
            .unwrap_err()
            .to_string()
            .contains("ivm-checkpoint-unavailable")
    );
    assert_eq!(checkpoint_state_rows(&n), before);
    assert_eq!(semantic_tokens(&n), tokens);
    check(&n);
}

#[test]
fn manifest_adoption_restore_and_unverified_entry_fail_before_certificate_scan_or_mutation() {
    let n = ordinary();
    let manifest = checkpoint_drop_fixture(&n);
    let before = checkpoint_state_rows(&n);
    let tokens = semantic_tokens(&n);
    // The failure is the unavailable runtime contract, before certificate interpretation,
    // rather than a substitute not-stable or success-looking empty action response.
    assert!(
        n.store
            .adopt_checkpoint(&manifest)
            .unwrap_err()
            .message
            .contains("ivm-checkpoint-unavailable")
    );
    assert!(
        n.store
            .restore_checkpoint_history(&manifest)
            .unwrap_err()
            .message
            .contains("ivm-checkpoint-unavailable")
    );
    assert!(
        n.store
            .adopt_checkpoint_unverified_for_tests(&manifest)
            .unwrap_err()
            .to_string()
            .contains("ivm-checkpoint-unavailable")
    );
    assert_eq!(checkpoint_state_rows(&n), before);
    assert_eq!(semantic_tokens(&n), tokens);
    check(&n);
}

#[test]
fn redundant_record_position_preserves_canonical_minimum_and_ready_availability() {
    let n = ordinary();
    let claim = append_raw(&n, desired_body("a", Some("amber")));
    let before = n
        .runtime
        .views
        .availability(&n.store.readers.get(), "desired-by-host", 1)
        .unwrap();
    let mut writer = n.store.connection.write();
    let tx = writer.transaction().unwrap();
    let position: u64 = tx
        .query_row(
            "SELECT position FROM ivm_claim_ranks WHERE claim_id=?1",
            [&claim.id],
            |r| r.get(0),
        )
        .unwrap();
    for (suffix, p) in [("same", position), ("higher", position + 100)] {
        tx.execute("INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,raw,state,claim_id,updated_at_unix_ms) VALUES(?1,'fixture',1,?2,?3,X'','valid',?2,'0')", params![format!("record/{suffix}/{}", claim.id), claim.id, p]).unwrap();
        assert_eq!(
            n.runtime
                .views
                .availability(&tx, "desired-by-host", 1)
                .unwrap(),
            before
        );
    }
    tx.execute(
        "UPDATE replica_records SET position=?1 WHERE record_ref=?2",
        params![position + 200, format!("record/same/{}", claim.id)],
    )
    .unwrap();
    assert_eq!(
        n.runtime
            .views
            .readiness(&tx, "desired-by-host", 1)
            .unwrap(),
        Readiness::Fenced
    );
    tx.rollback().unwrap();
    drop(writer);
    assert_eq!(
        n.runtime
            .views
            .availability(&n.store.readers.get(), "desired-by-host", 1)
            .unwrap(),
        before
    );
    check(&n);
}

#[test]
fn unsupported_owned_authority_preserves_admission_and_fences_effect_readiness() {
    let n = ordinary();
    append_raw(&n, desired_body("a", Some("amber")));
    check(&n);
    let before = row(&n.store.readers.get(), "desired-by-host", "agent/fixture/a").unwrap();
    let mut owned = desired_body("a", Some("cobalt"));
    owned["owned_set"] = json!("owned-set/fixture");
    let claim = append_raw(&n, owned);
    let c = n.store.readers.get();
    assert_eq!(
        c.query_row(
            "SELECT COUNT(*) FROM claims WHERE id=?1",
            [&claim.id],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        1
    );
    assert!(matches!(
        n.runtime.views.readiness(&c, "desired-by-host", 1).unwrap(),
        Readiness::Fenced
    ));
    assert!(
        n.runtime
            .views
            .availability(&c, "desired-by-host", 1)
            .unwrap()
            .error
            .unwrap()
            .contains("owned-set authority")
    );
    assert_eq!(
        row(&c, "desired-by-host", "agent/fixture/a").unwrap(),
        before
    );
    assert!(n.runtime.views.token(&c, "desired-by-host", 1).is_err());
    // A raw stale row is diagnostic only. It supplies no authorization for starts or stops.
}

#[test]
fn unsigned_anchor_claim_fences_unproven_authority_despite_self_reported_signers() {
    let n = ordinary();
    append(
        &n,
        "host/alder",
        "fleet.member-admitted",
        None,
        json!({"member_key":n.anchor,"via":"anchor",
        "writer_floor":0,"signers":[n.anchor]}),
    );
    assert!(matches!(
        n.runtime
            .views
            .readiness(&n.store.readers.get(), "fleet-membership", 1)
            .unwrap(),
        Readiness::Fenced
    ));
    assert!(rows(&n.store.readers.get(), "fleet-membership").is_empty());
    assert!(
        n.runtime
            .views
            .token(&n.store.readers.get(), "fleet-membership", 1)
            .is_err()
    );
}

#[test]
fn unsupported_fleet_sponsor_window_and_removal_authority_are_fenced() {
    for field in ["via", "sponsor", "writer_floor", "mode", "removed_by"] {
        let (key, _) = MemberKey::generate().unwrap();
        let public = key.public().to_owned();
        let n = node("alder", &public, Some(&public));
        n.store.set_member_key(Some(Arc::new(key))).unwrap();
        n.store
            .admit_fleet_anchor("fixture-fleet", &public, "listening")
            .unwrap();
        check(&n);
        let (child, _) = MemberKey::generate().unwrap();
        let (kind, mut input) = if field == "removed_by" {
            (
                "fleet.member-removed",
                json!({"member_key":child.public(),"high_water":10,"removed_by":"birch"}),
            )
        } else {
            (
                "fleet.member-admitted",
                json!({"member_key":child.public(),"via":"member","sponsor":"alder","mode":"listening","writer_floor":0}),
            )
        };
        if field != "removed_by" {
            input[field] = if field == "writer_floor" {
                json!(10)
            } else {
                json!("unsupported")
            };
        }
        let claim = append(&n, "host/birch", kind, None, input);
        let c = n.store.readers.get();
        assert_eq!(
            c.query_row(
                "SELECT COUNT(*) FROM claims WHERE id=?1",
                [&claim.id],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            n.runtime
                .views
                .readiness(&c, "fleet-membership", 1)
                .unwrap(),
            Readiness::Fenced
        );
        assert!(row(&c, "fleet-membership", "host/birch").unwrap().is_none());
        assert!(n.runtime.views.token(&c, "fleet-membership", 1).is_err());
    }
}

#[test]
fn rollback_restores_range_summaries_unread_counters_rows_generations_and_cut() {
    let n = ordinary();
    append(
        &n,
        "agent/fixture/a",
        "harness.limits",
        None,
        reading("claude", "acct", 10_000_000, Some(97.0), Some(100)),
    );
    append(
        &n,
        "message/a",
        "message.sent",
        Some("person/blair"),
        json!({"to":"person/ada","from":"person/blair","body":"hello"}),
    );
    check(&n);
    let before = semantic_tokens(&n);
    let before_cut = source_cut(&n.store.readers.get()).unwrap();
    let mut writer = n.store.connection.write();
    let tx = writer.transaction().unwrap();
    n.runtime
        .append_claim_tx(
            &tx,
            &n.store.origin,
            "agent/fixture/b",
            "harness.limits",
            None,
            &json!({"fields":reading("claude","acct",11_000_000,Some(1.0),Some(200))}),
            &[],
            None,
        )
        .unwrap();
    n.runtime
        .append_claim_tx(
            &tx,
            &n.store.origin,
            "message/a",
            "message.read",
            Some("person/ada"),
            &json!({"fields":{}}),
            &[],
            None,
        )
        .unwrap();
    assert_eq!(
        tx.query_row(
            "SELECT count FROM app_unread WHERE recipient='person/ada'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    tx.rollback().unwrap();
    drop(writer);
    assert_eq!(semantic_tokens(&n), before);
    assert_eq!(source_cut(&n.store.readers.get()).unwrap(), before_cut);
    check(&n);
}

#[test]
fn hour_range_partition_has_no_gaps_overlap_or_time_rounding() -> Result<()> {
    // Structural oracle checks exact integer inclusivity, not a mirror of the range algorithm.
    for (low, high) in [
        (0, 0),
        (0, 3_600_000),
        (255, 256),
        (65_535, 65_536),
        (10_000_000, 13_600_000),
        (i64::MAX as u64 - 3_600_000, i64::MAX as u64),
    ] {
        let blocks = limits_view::blocks(low, high);
        let mut next = low;
        assert!(blocks.len() <= 2 * 8 * 255);
        for (level, prefix) in blocks {
            let start = prefix << (level * 8);
            let span = 1u64 << (level * 8);
            assert_eq!(start, next);
            assert!(span - 1 <= high - start);
            next = start + span;
        }
        assert_eq!(next, high + 1);
    }
    Ok(())
}

#[test]
fn persisted_custom_outputs_reopen_without_reapplying_history() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("views.sqlite");
    let anchor = "unconfigured-test-anchor";
    let runtime = Arc::new(ViewRuntime::new(definitions("alder", anchor, None)).unwrap());
    let store = Store::open(&path, "alder", runtime.clone()).unwrap();
    store.bind_fleet("fixture-fleet").unwrap();
    store.pin_fleet_anchor(anchor).unwrap();
    install_steps(&store, &runtime);
    let n = Node {
        store,
        runtime,
        anchor: anchor.into(),
        local_signer: None,
    };
    append(
        &n,
        "agent/fixture/a",
        "harness.limits",
        None,
        reading("claude", "acct", 10_000_000, Some(97.0), Some(100)),
    );
    append(
        &n,
        "message/a",
        "message.sent",
        Some("person/blair"),
        json!({"to":"person/ada","from":"person/blair","body":"hello"}),
    );
    append_raw(&n, desired_body("a", Some("amber")));
    check(&n);
    let tokens = semantic_tokens(&n);
    let cut = source_cut(&n.store.readers.get()).unwrap();
    let raw = history::raw(&n.store.readers.get()).unwrap();
    let derived = NAMES
        .iter()
        .map(|name| rows(&n.store.readers.get(), name))
        .collect::<Vec<_>>();
    drop(n);
    let runtime = Arc::new(ViewRuntime::new(definitions("alder", anchor, None)).unwrap());
    let store = Store::open(&path, "alder", runtime.clone()).unwrap();
    let n = Node {
        store,
        runtime,
        anchor: anchor.into(),
        local_signer: None,
    };
    check(&n);
    assert_eq!(semantic_tokens(&n), tokens);
    assert_eq!(source_cut(&n.store.readers.get()).unwrap(), cut);
    assert_eq!(
        history::raw(&n.store.readers.get())
            .unwrap()
            .iter()
            .map(|c| &c.id)
            .collect::<Vec<_>>(),
        raw.iter().map(|c| &c.id).collect::<Vec<_>>()
    );
    assert_eq!(
        NAMES
            .iter()
            .map(|name| rows(&n.store.readers.get(), name))
            .collect::<Vec<_>>(),
        derived
    );
}

#[test]
fn actor_work_reassignment_retracts_previous_card_membership() {
    let n = ordinary();
    let a = "agent/fixture/a";
    let b = "agent/fixture/b";
    for actor in [a, b] {
        append(
            &n,
            actor,
            "runtime.observed",
            None,
            json!({"incarnation_id":"current","status":"running"}),
        );
        append(
            &n,
            actor,
            "harness.observed",
            None,
            json!({"incarnation_id":"current","state":"idle","provider_auth":true}),
        );
    }
    append(
        &n,
        STEPS[0].0,
        "work.claimed",
        Some(a),
        json!({"claim_incarnation":"current","status":"claimed"}),
    );
    check(&n);
    assert_eq!(
        row(&n.store.readers.get(), "agent-card", a)
            .unwrap()
            .unwrap()["work"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    append(
        &n,
        STEPS[0].0,
        "work.claimed",
        Some(b),
        json!({"claim_incarnation":"current","status":"claimed"}),
    );
    check(&n);
    assert_eq!(
        row(&n.store.readers.get(), "agent-card", a)
            .unwrap()
            .unwrap()["work"],
        json!([])
    );
    assert_eq!(
        row(&n.store.readers.get(), "agent-card", b)
            .unwrap()
            .unwrap()["work"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        row(&n.store.readers.get(), "agent-card", b)
            .unwrap()
            .unwrap()["status"],
        "working"
    );
    noise(&n, STEPS[0].0);
    replicate_permutations(&n, 0);
}

#[test]
fn late_prior_host_membership_advances_targeted_feed_without_selected_row_change() {
    desired_wakes::late_prior_host();
}

#[test]
fn unsupported_run_step_generation_owners_fence_even_without_owned_set_marker() {
    desired_wakes::owner_markers();
}

#[test]
fn same_incarnation_auth_restoration_changes_permission_while_work_is_unchanged() {
    desired_wakes::auth_restoration();
}
