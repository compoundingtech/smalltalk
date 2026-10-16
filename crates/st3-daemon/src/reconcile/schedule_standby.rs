//! A standby decision is local knowledge, not an exclusive lease or global negative.
//! The parent schedule paths own wakes and must recheck this decision before starting work.
use std::collections::BTreeMap;
use std::sync::PoisonError;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::model::ScheduleSpec;
use crate::store::Store;

pub const RUN_TWICE_WARNING: &str = "While hosts cannot hear each other a job may run on both; jobs must be safe to run twice. Write jobs so a second run is harmless.";
pub const COLD_STANDBY_LIMIT: &str = "If the standby restarts while the primary is down, the standby cannot take over until it has heard the primary once; a run in that window may be skipped (catch-up policy applies when the primary returns).";

#[derive(Clone)]
pub(crate) struct Heartbeat {
    epoch: String,
    exchange_at: u128,
    observed: Instant,
    observed_wall: u128,
    deadline_wall: u128,
    view: Value,
}

pub(crate) enum Readiness {
    Ready(Value),
    Wait { key: String, until: u128 },
    Held,
}

/// Only a current-incarnation COMPLETE primary exchange seeds the local monotonic clock.
/// Missing rows, old peer success, or an incomplete view never silently mean "primary absent".
pub(crate) fn readiness(
    store: &Store,
    schedule: &str,
    spec: &ScheduleSpec,
    revision: &str,
    occurrence: u64,
    started_at: u128,
    now: u128,
) -> Result<Readiness> {
    let Some(standby) = spec.standby.as_ref().filter(|standby| standby.host == store.origin) else {
        return Ok(Readiness::Held);
    };
    let adoption = match store.schedule_standby_adoption_epoch(spec) {
        Ok(epoch) => epoch,
        Err(_) => return Ok(Readiness::Held),
    };
    if !store.schedule_standby_history_complete()? {
        store.standby_observations.lock().unwrap_or_else(PoisonError::into_inner).remove(schedule);
        return Ok(Readiness::Held);
    }
    let Some(exchange_at) = store.replication_peer_last_success(&spec.host)?
        .filter(|at| *at >= started_at && *at <= now) else {
        return Ok(Readiness::Held);
    };
    let sync = store.replication_peer_sync(std::slice::from_ref(&spec.host));
    let Some(sync) = sync.get(&spec.host)
        .filter(|sync| sync.measured_at_unix_ms >= started_at
            && sync.measured_at_unix_ms <= now && sync.peer_only_envelopes == 0) else {
        return Ok(Readiness::Held);
    };
    let epoch = smallclaims::hash::canonical_hash(&(adoption, revision, spec)).expect("schedule role serializes");
    let mut observations = store.standby_observations.lock().unwrap_or_else(PoisonError::into_inner);
    let observation = observations.entry(schedule.into()).or_insert_with(|| Heartbeat {
        epoch: epoch.clone(), exchange_at, observed: Instant::now(),
        observed_wall: now, deadline_wall: now.saturating_add(u128::from(standby.after_ms)).saturating_add(1),
        view: json!({"primary":spec.host,"measured_at_unix_ms":sync.measured_at_unix_ms.to_string(),"peer_only_envelopes":0}),
    });
    if observation.epoch != epoch || observation.exchange_at != exchange_at {
        *observation = Heartbeat {
            epoch: epoch.clone(), exchange_at, observed: Instant::now(),
        observed_wall: now, deadline_wall: now.saturating_add(u128::from(standby.after_ms)).saturating_add(1),
            view: json!({"primary":spec.host,"measured_at_unix_ms":sync.measured_at_unix_ms.to_string(),"peer_only_envelopes":0}),
        };
    }
    let elapsed = observation.observed.elapsed();
    let wall_elapsed = now.checked_sub(observation.observed_wall);
    if wall_elapsed.is_none_or(|wall| wall.abs_diff(elapsed.as_millis()) > 1_000) {
        // A wall-clock jump cannot manufacture silence or leave an immediate-wake spin.
        observations.remove(schedule);
        return Ok(Readiness::Held);
    }
    let after = Duration::from_millis(standby.after_ms);
    if elapsed <= after {
        // Strictly older, including at sub-millisecond boundaries. One logical deadline per
        // role/occurrence; a renewed heartbeat supersedes it through the parent wake interface.
        return Ok(Readiness::Wait {
            key: format!("standby-eligibility:{schedule}:{epoch}:{occurrence}"),
            until: observation.deadline_wall,
        });
    }
    Ok(Readiness::Ready(json!({
        "host":store.origin,"primary":spec.host,
        "reason":"primary heartbeat stale; occurrence absent only in this member's complete applied view",
        "primary_heartbeat_age_ms":elapsed.as_millis().min(u128::from(u64::MAX)) as u64,
        "last_primary_exchange_unix_ms":exchange_at.to_string(),
        "role_epoch":epoch,"observed_primary_view":observation.view,
        "run_twice_warning":RUN_TWICE_WARNING,"cold_standby_limit":COLD_STANDBY_LIMIT,
    })))
}

/// Show records this member's observation, never a cluster-wide eligibility assertion.
pub fn display(store: &Store, schedule: &str, spec: &ScheduleSpec) -> Option<Value> {
    let standby = spec.standby.as_ref()?;
    let adoption = store.schedule_standby_adoption_epoch(spec);
    let observations = store.standby_observations.lock().unwrap_or_else(PoisonError::into_inner);
    let current_epoch = adoption.as_ref().ok().and_then(|adoption| {
        let revision = store.selected_desired_revision(schedule).ok().flatten()?;
        smallclaims::hash::canonical_hash(&(adoption, revision, spec)).ok()
    });
    let heartbeat = observations.get(schedule)
        .filter(|sample| current_epoch.as_ref() == Some(&sample.epoch));
    let (eligibility, reason) = match &adoption {
        Err(error) => ("held", error.message.as_str()),
        Ok(_) if heartbeat.is_none() => ("unknown", "no complete primary exchange observed this incarnation; cold standby is not eligible"),
        Ok(_) => ("unknown", "heartbeat age is this member's observation; due occurrence and role must be revalidated before admission"),
    };
    Some(json!({
        "primary_host":spec.host,"standby_host":standby.host,"after_ms":standby.after_ms,
        "eligibility":eligibility,"reason":reason,
        "primary_heartbeat_age_ms":heartbeat.map(|sample| sample.observed.elapsed().as_millis().min(u128::from(u64::MAX)) as u64),
        "heartbeat_basis":if heartbeat.is_some() {"current-incarnation"} else {"unknown"},
        "observed_primary_view":heartbeat.map(|sample| &sample.view),
        "run_twice_warning":RUN_TWICE_WARNING,"cold_standby_limit":COLD_STANDBY_LIMIT,
    }))
}

pub(crate) fn add_detail(fields: &mut BTreeMap<String, Value>, detail: Option<&Value>) {
    if let Some(detail) = detail {
        fields.insert("standby".into(), detail.clone());
        fields.insert("reason".into(), Value::String(format!(
            "{}; {RUN_TWICE_WARNING}; {COLD_STANDBY_LIMIT}",
            detail["reason"].as_str().unwrap_or("standby takeover"))));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fleet::MemberKey;
    use crate::model::ClaimInput;
    use std::sync::Arc;

    pub(crate) const FLEET: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
    fn now() -> u128 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() }
    fn append(store: &Store, subject: &str, kind: &str, fields: Value) {
        store.append_claim(&ClaimInput { subject: subject.into(), kind: kind.into(), actor: None,
            fields: serde_json::from_value(fields).unwrap(), evidence: vec![], expected_subject: None, idempotency_key: None }).unwrap();
    }
    pub(crate) fn advertise(store: &Store, feature: u8) {
        append(store, &format!("daemon/{}", store.origin), "daemon.started", json!({"status":"running","features":{"schedule_standby":feature}}));
    }
    pub(crate) fn sync(from: &Store, to: &Store) {
        let exchange = from.export_replication_exchange_answering(FLEET, &to.replication_inventory().unwrap(), &to.replication_signature_requests().unwrap()).unwrap();
        to.receive_replication_exchange(&from.origin, FLEET, &exchange).unwrap();
        let admission = to.validate_replication_backlog().unwrap();
        assert_eq!((admission.invalid, admission.unknown), (0, 0));
        to.apply_replication_repairs().unwrap();
        to.project_replication_backlog().unwrap();
    }
    pub(crate) fn pair() -> (Store, Store, ScheduleSpec) {
        let primary = Store::open_memory("ivory").unwrap();
        let anchor = Arc::new(MemberKey::generate().unwrap().0);
        primary.bind_fleet(FLEET).unwrap();
        primary.pin_fleet_anchor(anchor.public()).unwrap();
        primary.set_member_key(Some(anchor.clone())).unwrap();
        primary.admit_fleet_anchor(FLEET, anchor.public(), "listening").unwrap();
        advertise(&primary, 1);
        let key = Arc::new(MemberKey::generate().unwrap().0);
        append(&primary, "host/jade", "fleet.member-admitted", json!({"fleet_id":FLEET,"member_key":key.public(),"via":"invite","sponsor":"host/ivory","mode":"listening"}));
        let standby = Store::open_memory("jade").unwrap();
        standby.bind_fleet(FLEET).unwrap();
        standby.pin_fleet_anchor(anchor.public()).unwrap();
        standby.set_member_key(Some(key)).unwrap();
        sync(&primary, &standby);
        advertise(&standby, 1);
        sync(&standby, &primary);
        sync(&primary, &standby);
        let parsed = crate::parse_intent(r#"version 2
schedule "orchard" { host "ivory"; standby { host "jade"; after "50ms" }
 every "1h"; anchor "2030-01-01T00:00:00Z"; work { mission "harvest"; workspace "/tmp/orchard" }
}"#, "ivory").unwrap();
        let spec = crate::graph::schedule_spec(&parsed.subjects["schedule/orchard"].desired, "ivory").unwrap();
        (primary, standby, spec)
    }
    fn waiting(value: Readiness) -> (String, u128) {
        match value { Readiness::Wait { key, until } => (key, until), _ => panic!("expected fresh-heartbeat wait") }
    }

    #[tokio::test(start_paused = true)]
    async fn unchanged_heartbeat_has_one_finite_deadline_equal_age_is_fresh_then_stale() {
        let (_primary, standby, spec) = pair();
        let wall = now();
        let initial = waiting(readiness(&standby, "schedule/orchard", &spec, "revision", 7, 0, wall).unwrap());
        tokio::time::advance(Duration::from_millis(25)).await;
        assert_eq!(waiting(readiness(&standby, "schedule/orchard", &spec, "revision", 7, 0, wall+25).unwrap()), initial);
        tokio::time::advance(Duration::from_millis(25)).await;
        assert_eq!(waiting(readiness(&standby, "schedule/orchard", &spec, "revision", 7, 0, wall+50).unwrap()), initial);
        tokio::time::advance(Duration::from_millis(1)).await;
        let Readiness::Ready(detail) = readiness(&standby, "schedule/orchard", &spec, "revision", 7, 0, wall+51).unwrap() else { panic!("stale did not qualify") };
        assert_eq!(detail["primary_heartbeat_age_ms"], 51);
        assert_eq!(detail["host"], "jade");
        assert!(detail["reason"].as_str().unwrap().contains("only in this member"));
    }

    #[tokio::test(start_paused = true)]
    async fn renewal_role_replacement_restart_and_clock_jump_do_not_reuse_stale_authority() {
        let (primary, standby, mut spec) = pair();
        let wall = now();
        let initial = waiting(readiness(&standby, "schedule/orchard", &spec, "revision", 7, 0, wall).unwrap());
        tokio::time::advance(Duration::from_millis(51)).await;
        spec.standby.as_mut().unwrap().after_ms = 75;
        let replacement = waiting(readiness(&standby, "schedule/orchard", &spec, "revision2", 7, 0, wall+51).unwrap());
        assert_ne!(initial.0, replacement.0);
        assert!(matches!(readiness(&standby, "schedule/orchard", &spec, "revision2", 7, wall+1_000, wall+51).unwrap(), Readiness::Held), "restart accepted historic success");
        assert!(matches!(readiness(&standby, "schedule/orchard", &spec, "revision2", 7, 0, wall+5_000).unwrap(), Readiness::Held), "wall jump manufactured elapsed silence");
        // A real new authenticated receipt refreshes the heartbeat, not a peer's claim.
        sync(&primary, &standby);
        let fresh = waiting(readiness(&standby, "schedule/orchard", &spec, "revision2", 7, 0, now()).unwrap());
        assert!(fresh.1 > now());
    }

    #[test]
    fn mixed_or_missing_advertisement_foreign_advertisement_and_unknown_member_hold() {
        let (primary, standby, mut spec) = pair();
        assert!(standby.schedule_standby_adoption_epoch(&spec).is_ok());
        advertise(&primary, 0);
        sync(&primary, &standby);
        assert!(standby.schedule_standby_adoption_epoch(&spec).is_err());
        append(&standby, "daemon/ivory", "daemon.started", json!({"status":"running","features":{"schedule_standby":1}}));
        assert!(standby.schedule_standby_adoption_epoch(&spec).is_err(), "foreign writer advertised for primary");
        advertise(&primary, 1);
        sync(&primary, &standby);
        assert!(standby.schedule_standby_adoption_epoch(&spec).is_ok());
        spec.standby.as_mut().unwrap().host = "typo".into();
        let shown = display(&standby, "schedule/orchard", &spec).unwrap();
        assert_eq!(shown["reason"], "standby member unknown, never eligible");
        assert_eq!(shown["cold_standby_limit"], COLD_STANDBY_LIMIT);
        assert_eq!(shown["run_twice_warning"], RUN_TWICE_WARNING);
    }
    #[test]
    fn unknown_admission_and_partial_primary_exchange_do_not_prove_absence() {
        let (primary, standby, spec) = pair();
        for index in 0..4 {
            append(&primary, "daemon/ivory", "daemon.diagnostic", json!({"severity":"warning","code":format!("note-{index}"),"reason":"invented fixture"}));
        }
        let mut inventory = standby.replication_inventory().unwrap();
        inventory.accepts = Some(1);
        let exchange = primary.export_replication_exchange_answering(FLEET, &inventory, &standby.replication_signature_requests().unwrap()).unwrap();
        standby.receive_replication_exchange("ivory", FLEET, &exchange).unwrap();
        standby.validate_replication_backlog().unwrap();
        standby.project_replication_backlog().unwrap();
        assert!(matches!(readiness(&standby, "schedule/orchard", &spec, "revision", 0, 0, now()).unwrap(), Readiness::Held), "partial exchange supplied a negative witness");

        let (primary, standby, spec) = pair();
        let mut registry = st3_schema::registry().clone();
        registry.claims.remove("daemon.diagnostic").unwrap();
        standby.set_claim_registry(registry);
        append(&primary, "daemon/ivory", "daemon.diagnostic", json!({"severity":"warning","code":"future-note","reason":"invented newer member"}));
        let exchange = primary.export_replication_exchange_answering(FLEET, &standby.replication_inventory().unwrap(), &standby.replication_signature_requests().unwrap()).unwrap();
        standby.receive_replication_exchange("ivory", FLEET, &exchange).unwrap();
        let admission = standby.validate_replication_backlog().unwrap();
        assert!(admission.unknown > 0);
        standby.project_replication_backlog().unwrap();
        assert!(!standby.schedule_standby_history_complete().unwrap());
        assert!(matches!(readiness(&standby, "schedule/orchard", &spec, "revision", 0, 0, now()).unwrap(), Readiness::Held));
    }

    #[test]
    fn publication_refuses_known_unknown_standby_member() {
        let (_primary, standby, _spec) = pair();
        let source = r#"version 2
schedule "bad" { host "ivory"; standby { host "typo"; after "1s" }
 at "2030-01-01T00:00:00Z"; work { mission "harvest"; workspace "/tmp/orchard" }
}"#;
        let intent = crate::parse_intent(source, "jade").unwrap();
        let plan = standby.mission(&intent, crate::model::IntentInput { kdl: source.into(), source_name: None }).unwrap();
        assert!(plan.blockers.iter().any(|blocker| blocker.contains("standby member unknown, never eligible")));
        let error = standby.apply_as(&intent, &plan.subject_tokens, "unknown-standby", Some("person/tester")).unwrap_err();
        assert_eq!(error.code, "schedule-standby-unavailable");
    }

}
