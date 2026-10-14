//! Replicated results from the token-free native delivery probe seats.
use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use super::DoctorCheck;
use crate::store::Store;

const PREFIX: &str = "doc/delivery-probes/";
const FORMAT: &str = "st3.delivery-probes.v1";
/// A probe republishes an unchanged report every five minutes (each one is a replicated write),
/// so a report is stale after three missed heartbeats.
const HEARTBEAT_STALE_MS: u128 = 15 * 60_000;

#[derive(Deserialize)]
struct Report {
    format: String,
    source: String,
    agent: String,
    updated_at_unix_ms: u128,
    deadline_ms: u128,
    members: Vec<String>,
    routes: Vec<Route>,
}

#[derive(Deserialize)]
struct Route {
    target: String,
    to: String,
    status: String,
    started_at_unix_ms: Option<u128>,
    message: Option<String>,
    read_at_unix_ms: Option<u128>,
    latency_ms: Option<u128>,
    #[serde(default)]
    late: bool,
    previous_read: Option<PreviousRead>,
}

#[derive(Deserialize)]
struct PreviousRead {
    late: bool,
    latency_ms: u128,
}

pub(super) fn check(
    store: &Store,
    now: u128,
    peers: &[String],
) -> anyhow::Result<Option<DoctorCheck>> {
    let seen = |member: &str| -> anyhow::Result<bool> {
        Ok(member == store.origin()
            || !peers.iter().any(|peer| peer == member)
            || store.replication_peer_refusal(member)?.is_some()
            || store.replication_peer_up(member)?.0)
    };
    let documents = store.list_documents_page(None, Some(PREFIX), false, None, 201)?;
    if documents.is_empty() {
        return Ok(None);
    }
    let mut reports = BTreeMap::new();
    let mut notes = Vec::new();
    let mut warned = documents.len() > 200;
    if warned {
        notes.push("more than 200 probe reports; the fleet result is incomplete".into());
    }
    for document in documents.into_iter().take(200) {
        let bytes = store
            .get_document(&document.name, &document.hash)?
            .ok_or_else(|| anyhow::anyhow!("probe document {} has no content", document.name))?;
        match serde_json::from_slice::<Report>(&bytes) {
            Ok(report)
                if report.format == FORMAT
                    && document.name == format!("{PREFIX}{}", report.source)
                    && (1_000..=60_000).contains(&report.deadline_ms)
                    && report.agent.starts_with("agent/")
                    && report.members.contains(&report.source)
                    && report.members.len() > 1
                    && !report.routes.is_empty() =>
            {
                reports.insert(report.source.clone(), report);
            }
            _ => {
                warned = true;
                notes.push(format!("{} has an invalid probe result", document.name));
            }
        }
    }
    let members = reports
        .values()
        .flat_map(|report| report.members.iter().cloned())
        .collect::<BTreeSet<_>>();
    for member in &members {
        if let Some(reason) = store.replication_peer_refusal(member)? {
            notes.push(format!(
                "{member}: {reason}; checking end-to-end delivery through other members"
            ));
        }
        if !seen(member)? {
            notes.push(format!("{member}: last seen; waiting for an exchange"));
            continue;
        }
        if !reports.contains_key(member) {
            warned = true;
            notes.push(format!("{member}: no probe heartbeat or outgoing results"));
        }
    }
    for report in reports.values() {
        if !seen(&report.source)? {
            continue;
        }
        let age = now.saturating_sub(report.updated_at_unix_ms);
        if age > HEARTBEAT_STALE_MS || report.updated_at_unix_ms > now.saturating_add(5_000) {
            warned = true;
            notes.push(format!(
                "{}: probe heartbeat is stale or its clock is ahead (age {}s)",
                report.source,
                age / 1_000
            ));
        }
        for target in report
            .members
            .iter()
            .filter(|target| *target != &report.source)
        {
            if !seen(target)? {
                continue;
            }
            if !report.routes.iter().any(|route| &route.target == target) {
                warned = true;
                notes.push(format!("{} → {target}: no route result", report.source));
            }
        }
        for route in &report.routes {
            if !seen(&route.target)? {
                continue;
            }
            let path = format!("{} → {}", report.source, route.target);
            if let Some(previous) = &route.previous_read
                && previous.late
            {
                warned = true;
                notes.push(format!("{path}: last completed probe read in {}ms, exceeding the deadline; a new success is pending", previous.latency_ms));
            }
            match (
                route.status.as_str(),
                route.read_at_unix_ms,
                route.latency_ms,
            ) {
                ("read", Some(_), Some(latency)) => {
                    let verified = match route.message.as_deref() {
                        Some(subject) => {
                            let message = store.message(subject)?;
                            let reader = store.claims_for(subject, Some("message.read"))?;
                            message.is_some_and(|message| {
                                message.from == report.agent && message.to == route.to
                            }) && reader.iter().any(|claim| {
                                claim.actor.as_deref() == Some(route.to.as_str())
                                    && Some(claim.accepted_at_unix_ms) == route.read_at_unix_ms
                            })
                        }
                        None => false,
                    };
                    if !verified {
                        warned = true;
                        notes.push(format!(
                            "{path}: reported read has no matching recipient read claim"
                        ));
                        continue;
                    }
                    let late = route.late || latency > report.deadline_ms;
                    warned |= late;
                    notes.push(format!(
                        "{path}: read in {latency}ms{} ({})",
                        if late { "; exceeded the deadline" } else { "" },
                        route.message.as_deref().unwrap_or("missing message id")
                    ));
                }
                ("pending" | "sending" | "overdue", _, _) => {
                    let elapsed = route
                        .started_at_unix_ms
                        .map(|sent| now.saturating_sub(sent));
                    let overdue = route.status == "overdue"
                        || elapsed.is_none_or(|elapsed| elapsed >= report.deadline_ms);
                    warned |= overdue;
                    notes.push(format!(
                        "{path}: {} ({}; age {}s)",
                        if overdue {
                            "no read within the deadline"
                        } else {
                            "waiting for read"
                        },
                        route.message.as_deref().unwrap_or("send not accepted"),
                        elapsed.unwrap_or_default() / 1_000
                    ));
                }
                _ => {
                    warned = true;
                    notes.push(format!(
                        "{path}: no verified read result ({})",
                        route.status
                    ));
                }
            }
        }
    }
    Ok(Some(DoctorCheck {
        name: "delivery-probes".into(),
        status: if warned { "warn" } else { "pass" }.into(),
        message: notes.join("; "),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn put(store: &Store, source: &str, time: u128, status: &str, latency: u128) {
        let target = if source == "amber" { "cobalt" } else { "amber" };
        let subject = if source == "amber" {
            "message/aaaaaaaaaaaaaaaa"
        } else {
            "message/bbbbbbbbbbbbbbbb"
        };
        for (kind, actor, fields) in [
            (
                "message.sent",
                format!("agent/probe/{source}"),
                json!({"from": format!("agent/probe/{source}"), "to": format!("agent/probe/{target}"), "status": "sent", "content": "probe"}),
            ),
            (
                "message.staged",
                format!("agent/probe/{target}"),
                json!({"status": "staged", "recipient": format!("agent/probe/{target}"), "transport": "omp-channel"}),
            ),
            (
                "message.delivered",
                format!("agent/probe/{target}"),
                json!({"status": "delivered"}),
            ),
            (
                "message.read",
                format!("agent/probe/{target}"),
                json!({"status": "read"}),
            ),
        ] {
            store
                .append_claim(&crate::model::ClaimInput {
                    subject: subject.into(),
                    kind: kind.into(),
                    actor: Some(actor),
                    fields: serde_json::from_value(fields).unwrap(),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: Some(format!("{source}:{kind}")),
                })
                .unwrap();
        }
        let read_at =
            store.claims_for(subject, Some("message.read")).unwrap()[0].accepted_at_unix_ms;
        let bytes = serde_json::to_vec(&json!({
            "format": FORMAT, "source": source, "agent": format!("agent/probe/{source}"), "updated_at_unix_ms": time,
            "deadline_ms": 60_000, "members": ["amber", "cobalt"],
            "routes": [{"target": target, "to": format!("agent/probe/{target}"), "status": status, "started_at_unix_ms": 1000,
                "message": subject, "read_at_unix_ms": read_at, "latency_ms": latency}]
        }))
        .unwrap();
        store
            .put_document(
                &format!("{PREFIX}{source}"),
                &bytes,
                &store
                    .latest_document_token(&format!("{PREFIX}{source}"))
                    .unwrap(),
                &format!("{source}:{time}:{status}:{latency}"),
            )
            .unwrap();
    }

    #[test]
    fn delivery_probes_require_every_source_and_current_read_results() {
        let store = Store::open_memory("amber").unwrap();
        assert!(check(&store, 2000, &[]).unwrap().is_none());
        put(&store, "amber", 2000, "read", 400);
        let missing = check(&store, 2000, &[]).unwrap().unwrap();
        assert_eq!(missing.status, "warn");
        assert!(missing.message.contains("cobalt: no probe heartbeat"));
        put(&store, "cobalt", 2000, "read", 600);
        let good = check(&store, 2000, &[]).unwrap().unwrap();
        assert_eq!(good.status, "pass");
        assert!(good.message.contains("amber → cobalt: read in 400ms"));
        // Ten minutes without a changed report is two missed heartbeats, not yet stale.
        assert_eq!(check(&store, 602_000, &[]).unwrap().unwrap().status, "pass");
        let stale = check(&store, 2000 + HEARTBEAT_STALE_MS + 1, &[])
            .unwrap()
            .unwrap();
        assert_eq!(stale.status, "warn");
        assert!(stale.message.contains("probe heartbeat is stale"));
        put(&store, "amber", 100_000, "pending", 400);
        let overdue = check(&store, 100_000, &[]).unwrap().unwrap();
        assert!(overdue.message.contains("no read within the deadline"));
        put(&store, "amber", 100_000, "read", 61_000);
        assert!(
            check(&store, 100_000, &[])
                .unwrap()
                .unwrap()
                .message
                .contains("exceeded the deadline")
        );
    }

    #[test]
    fn absent_members_pause_missing_heartbeats_and_overdue_routes() {
        let store = Store::open_memory("amber").unwrap();
        put(&store, "amber", 100_000, "overdue", 61_000);
        let report = check(&store, 100_000, &["cobalt".into()]).unwrap().unwrap();
        assert_eq!(report.status, "pass");
        assert!(report.message.contains("last seen"));
        assert!(!report.message.contains("deadline"));
    }

    #[test]
    fn a_refused_direct_route_checks_real_indirect_reads_without_warning_for_the_refusal() {
        let store = Store::open_memory("amber").unwrap();
        put(&store, "amber", 2000, "read", 400);
        put(&store, "cobalt", 2000, "read", 600);
        store
            .record_peer_failure(
                "cobalt",
                "refused",
                "refused by that member's Fabric grants; using other members",
            )
            .unwrap();
        let good = check(&store, 2000, &["cobalt".into()]).unwrap().unwrap();
        assert_eq!(good.status, "pass");
        assert!(good.message.contains("Fabric grants"));
        assert!(good.message.contains("amber → cobalt: read in 400ms"));
        put(&store, "amber", 100_000, "overdue", 61_000);
        put(&store, "cobalt", 100_000, "read", 600);
        let overdue = check(&store, 100_000, &["cobalt".into()]).unwrap().unwrap();
        assert_eq!(overdue.status, "warn");
        assert!(overdue.message.contains("no read within the deadline"));
    }

    #[test]
    fn delivery_probes_do_not_accept_delivered_as_read_or_bad_documents() {
        let store = Store::open_memory("amber").unwrap();
        put(&store, "amber", 2000, "delivered", 400);
        put(&store, "cobalt", 2000, "read", 600);
        let check_result = check(&store, 2000, &[]).unwrap().unwrap();
        assert_eq!(check_result.status, "warn");
        assert!(check_result.message.contains("no verified read"));
        let name = format!("{PREFIX}cobalt");
        let hash = store.latest_document_hash(&name).unwrap().unwrap();
        let mut forged: serde_json::Value =
            serde_json::from_slice(&store.get_document(&name, &hash).unwrap().unwrap()).unwrap();
        forged["routes"][0]["read_at_unix_ms"] = json!(1);
        store
            .put_document(
                &name,
                &serde_json::to_vec(&forged).unwrap(),
                &store.latest_document_token(&name).unwrap(),
                "unverified-read",
            )
            .unwrap();
        assert!(
            check(&store, 2000, &[])
                .unwrap()
                .unwrap()
                .message
                .contains("no matching recipient read claim")
        );
        store
            .put_document(
                &format!("{PREFIX}amber"),
                b"{}",
                &store
                    .latest_document_token(&format!("{PREFIX}amber"))
                    .unwrap(),
                "invalid",
            )
            .unwrap();
        assert!(
            check(&store, 2000, &[])
                .unwrap()
                .unwrap()
                .message
                .contains("invalid probe result")
        );
    }
}
