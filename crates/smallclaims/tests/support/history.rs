//! Deliberately slow test-only history folds. Never called by a writer operator or public read.
//! These use raw admitted claims, not ivm_heads/contributions or app_* derived output.
use anyhow::{Result, ensure};
use rusqlite::Connection;
use serde_json::{Value, json};
use smallclaims::{
    ClaimRecord,
    fleet::MemberState,
    store::{canonical, claim_from_row},
};
use std::collections::{BTreeMap, BTreeSet};

pub type Rows = BTreeMap<String, Value>;
pub fn raw(c: &Connection) -> Result<Vec<ClaimRecord>> {
    Ok(c.prepare(&canonical::canonical_sql(
        "SELECT id,store_index,batch_id,subject,kind,origin,actor,
        body,predecessors,accepted_at_unix_ms FROM claims ORDER BY CANONICAL_ASC(claims)",
    ))?
    .query_map([], claim_from_row)?
    .collect::<rusqlite::Result<_>>()?)
}
pub fn card(claims: &[ClaimRecord]) -> Rows {
    let mut runtime = BTreeMap::new();
    let mut harness = BTreeMap::new();
    let mut harness_position = BTreeMap::new();
    let mut auth = BTreeMap::new();
    let mut usage = BTreeMap::new();
    let mut work = BTreeMap::new();
    let mut work_position = BTreeMap::new();
    for (position, c) in claims.iter().enumerate() {
        let f = &c.body["fields"];
        match c.kind.as_str() {
            "runtime.observed" => {
                runtime.insert(c.subject.clone(), f.clone());
            }
            "harness.observed" => {
                harness_position.insert(
                    (
                        c.subject.clone(),
                        f["incarnation_id"].as_str().unwrap().to_owned(),
                    ),
                    position,
                );
                if f["provider_auth"].is_boolean() {
                    auth.insert(
                        (
                            c.subject.clone(),
                            f["incarnation_id"].as_str().unwrap().to_owned(),
                        ),
                        f["provider_auth"].clone(),
                    );
                }
                harness.insert(
                    (
                        c.subject.clone(),
                        f["incarnation_id"].as_str().unwrap().to_owned(),
                    ),
                    f.clone(),
                );
            }
            "harness.usage" => {
                let key = (
                    c.subject.clone(),
                    f["incarnation_id"].as_str().unwrap().to_owned(),
                );
                let total = f["total_tokens"].as_u64().unwrap();
                if usage
                    .get(&key)
                    .is_none_or(|current: &Value| total >= current.as_u64().unwrap())
                {
                    usage.insert(key, json!(total));
                }
            }
            "work.claimed" | "work.progress" | "work.submitted" | "work.failed"
            | "work.released" => {
                work.insert(c.subject.clone(), c);
                work_position.insert(c.subject.clone(), position);
            }
            _ => {}
        }
    }
    runtime.into_iter().map(|(actor,r)| {
        let incarnation=r["incarnation_id"].as_str().unwrap().to_owned();
        let h=harness.get(&(actor.clone(),incarnation.clone()));
        let active=work.iter().filter(|(_,c)|c.actor.as_deref()==Some(actor.as_str()) && matches!(c.kind.as_str(),"work.claimed"|"work.progress")
            && c.body["fields"]["claim_incarnation"]==incarnation).collect::<Vec<_>>();
        let last_work=active.iter().map(|(step,_)|work_position.get(*step).unwrap()).max();
        let last_harness=harness_position.get(&(actor.clone(),incarnation.clone()));
        let status=if r["status"]!="running"{r["status"].clone()}
            else if auth.get(&(actor.clone(),incarnation.clone())).is_some_and(|v|v==false){json!("needs-login")}
            else if last_work.is_some_and(|w|last_harness.is_none_or(|h|w>h)){json!("working")}
            else {h.map(|h|h["state"].clone()).unwrap_or(json!("unknown"))};
        let work=active.into_iter()
            .map(|(step,c)|json!({"step":step,"status":c.body["fields"]["status"],"claim":c.id})).collect::<Vec<_>>();
        let value=json!({"subject":actor,"incarnation":incarnation,"status":status,
            "usage":usage.get(&(actor.clone(),incarnation)),"work":work});(actor,value)
    }).collect()
}
pub fn mailbox(claims: &[ClaimRecord]) -> (Rows, BTreeMap<String, i64>) {
    let mut sent = BTreeMap::new();
    let mut read = BTreeSet::new();
    let mut closed = BTreeSet::new();
    for c in claims {
        match c.kind.as_str() {
            "message.sent" => {
                sent.insert(c.subject.clone(), &c.body["fields"]);
            }
            "message.read" => {
                read.insert(c.subject.clone());
            }
            "message.closed" => {
                closed.insert(c.subject.clone());
            }
            _ => {}
        }
    }
    let mut counts = BTreeMap::new();
    let mut rows = Rows::new();
    for (id, f) in sent {
        let unread = !read.contains(&id) && !closed.contains(&id);
        *counts
            .entry(f["to"].as_str().unwrap().to_owned())
            .or_insert(0) += i64::from(unread);
        rows.insert(
            id.clone(),
            json!({"id":id,"to":f["to"],"from":f["from"],"body":f["body"],"unread":unread}),
        );
    }
    (rows, counts)
}
pub type Step = (
    &'static str,
    &'static str,
    &'static str,
    Option<&'static str>,
);
pub fn tree(claims: &[ClaimRecord], steps: &[Step]) -> Rows {
    let mut rows = Rows::new();
    let mut state = BTreeMap::new();
    for c in claims {
        if c.kind == "mission-run.created" {
            rows.insert(
                c.subject.clone(),
                json!({"id":c.subject,"parent":c.body["fields"]["parent_step_run"],
            "state":c.body["fields"]["status"],"kind":"run"}),
            );
        } else if matches!(
            c.kind.as_str(),
            "step-run.state"
                | "work.claimed"
                | "work.progress"
                | "work.submitted"
                | "work.failed"
                | "work.released"
        ) {
            state.insert(c.subject.clone(), c.body["fields"]["status"].clone());
        }
    }
    for (id, _, path, parent) in steps {
        rows.insert(
            (*id).into(),
            json!({"id":id,"parent":parent,"kind":"step",
        "path":path,"state":state.get(*id).cloned().unwrap_or(json!("pending"))}),
        );
    }
    rows
}
pub fn desired(claims: &[ClaimRecord]) -> (Rows, BTreeMap<String, BTreeSet<String>>) {
    let mut rows = Rows::new();
    let mut prior = BTreeMap::<String, BTreeSet<String>>::new();
    for c in claims.iter().filter(|c| c.kind == "intent.desired") {
        assert!(
            c.body.get("owned_set").is_none(),
            "oracle is the unowned canonical slice"
        );
        let f = &c.body;
        if let Some(host) = f.pointer("/member/host").and_then(Value::as_str) {
            prior
                .entry(c.subject.clone())
                .or_default()
                .insert(host.into());
        }
        rows.insert(
            c.subject.clone(),
            json!({"subject":c.subject,"kind":f["kind"],
            "host":f.pointer("/member/host"),"claim":c.id}),
        );
    }
    (rows, prior)
}

/// Compatibility selector transcribed from the pinned limits.rs comparison, without radix nodes.
/// The parser fixes this fixture to identified provider accounts (no declared-label/privacy claim).
pub fn limits(claims: &[ClaimRecord]) -> Rows {
    let mut groups = BTreeMap::<String, Vec<Value>>::new();
    for c in claims.iter().filter(|c| c.kind == "harness.limits") {
        let mut f = c.body["fields"].clone();
        f["measured_by"] = json!(c.subject);
        f["host"] = json!(c.origin);
        let key = json!([f["driver"], f["account"], f["account_ref"]]).to_string();
        groups.entry(key).or_default().push(f);
    }
    groups
        .into_iter()
        .map(|(key, mut readings)| {
            if readings
                .iter()
                .any(|r| r["weekly_percent"].as_f64().is_some())
            {
                readings.retain(|r| r["weekly_percent"].as_f64().is_some());
            }
            let latest = readings
                .iter()
                .map(|r| r["measured_at_unix_ms"].as_u64().unwrap())
                .max()
                .unwrap();
            readings.retain(|r| {
                r["measured_at_unix_ms"].as_u64().unwrap() >= latest.saturating_sub(3_600_000)
            });
            let reset = readings
                .iter()
                .filter_map(|r| r["weekly_resets_at_unix_ms"].as_u64())
                .max();
            readings.retain(|r| r["weekly_resets_at_unix_ms"].as_u64() == reset);
            let chosen = readings
                .into_iter()
                .max_by(|a, b| {
                    a["weekly_percent"]
                        .as_f64()
                        .partial_cmp(&b["weekly_percent"].as_f64())
                        .unwrap()
                        .then_with(|| {
                            if a["weekly_percent"].is_null() && b["weekly_percent"].is_null() {
                                a["five_hour_resets_at_unix_ms"]
                                    .as_u64()
                                    .cmp(&b["five_hour_resets_at_unix_ms"].as_u64())
                                    .then_with(|| {
                                        a["five_hour_percent"]
                                            .as_f64()
                                            .partial_cmp(&b["five_hour_percent"].as_f64())
                                            .unwrap()
                                    })
                            } else {
                                std::cmp::Ordering::Equal
                            }
                        })
                        .then_with(|| {
                            (a["measured_at_unix_ms"].as_u64(), a["measured_by"].as_str()).cmp(&(
                                b["measured_at_unix_ms"].as_u64(),
                                b["measured_by"].as_str(),
                            ))
                        })
                })
                .unwrap();
            (key, chosen)
        })
        .collect()
}
pub fn fleet(c: &Connection, local: Option<(&str, &str)>) -> Result<Rows> {
    let membership = smallclaims::store::fleet_membership_tx_with_local_signer(c, local)?;
    let mut rows = Rows::new();
    let names = raw(c)?
        .into_iter()
        .filter(|c| c.kind.starts_with("fleet."))
        .filter_map(|c| c.subject.strip_prefix("host/").map(str::to_owned))
        .collect::<BTreeSet<_>>();
    for name in names {
        let (inc, state) = match membership.state(&name) {
            MemberState::Current(i) => (i, "current"),
            MemberState::Ended(i) => (i, "ended"),
            MemberState::NotMember => continue,
            other => {
                ensure!(false, "unsupported oracle state {other:?}");
                unreachable!()
            }
        };
        let subject = format!("host/{name}");
        rows.insert(subject.clone(),json!({"subject":subject,
            "member_key":inc.member_key,"admitted":inc.admitted_claim,"start":inc.start,"end":inc.end,"state":state}));
    }
    Ok(rows)
}
