//! The published lists against the full computations they replace: after every change, the
//! rows a fold publishes from the previous publication equal both a fold from nothing and the
//! rows a window read folds directly at the same cut.
use super::*;
use crate::model::{MissionRunRequest, WorkRequest};
use smallclaims::store::set_thread_clock;

const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";

/// Resets the thread's clock however the test ends.
struct Clock;

impl Clock {
    fn at(now: u128) -> Self {
        set_thread_clock(Some(now));
        Self
    }

    fn set(&self, now: u128) {
        set_thread_clock(Some(now));
    }
}

impl Drop for Clock {
    fn drop(&mut self) {
        set_thread_clock(None);
    }
}

fn publish(store: &Store, source: &str, key: &str) {
    let intent = crate::graph::parse_intent(source, store.origin()).unwrap();
    let preview = store
        .mission(&intent, crate::model::IntentInput { kdl: source.into(), source_name: None })
        .unwrap();
    store.apply_as(&intent, &preview.subject_tokens, key, Some("person/operator")).unwrap();
}

fn mission(store: &Store, name: &str) {
    publish(
        store,
        &format!(
            r#"version 2
mission "{name}" state="ready" {{
  goal "Grow the garden."
  concurrent-runs max=4
  step "plant" {{ assigned-to "agent/garden/ash" }}
  step "water" {{ assigned-to "agent/garden/birch" }}
}}"#
        ),
        &format!("publish-{name}"),
    );
}

fn start(store: &Store, name: &str, key: &str) -> crate::model::MissionRunView {
    store
        .create_mission_run(&MissionRunRequest {
            mission: name.into(),
            revision: None,
            workspace: "/work/garden".into(),
            requester: Some("person/operator".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: key.into(),
        })
        .unwrap()
}

fn step(run: &crate::model::MissionRunView, path: &str) -> (String, String) {
    let step = run.steps.iter().find(|step| step.step == path).unwrap();
    (step.subject.clone(), step.assigned_to.clone().unwrap())
}

fn act(store: &Store, subject: &str, actor: &str, action: &str, key: &str) {
    store
        .work_action(
            subject,
            action,
            &WorkRequest {
                actor: Some(actor.into()),
                incarnation: Some(format!("{actor}-1")),
                summary: Some(format!("{action} by {actor}")),
                reason: Some(format!("{action} for the test")),
                evidence: vec!["doc/garden/evidence".into()],
                idempotency_key: key.into(),
            },
        )
        .unwrap();
}

/// What a missions window folds directly: every shown mission in order, with its card.
fn oracle(store: &Store) -> (Vec<String>, BTreeMap<String, Value>) {
    store
        .read_snapshot(|_| {
            let ids = store.mission_collection_ids(false, 0, 100_000)?;
            let cards = client_v0::mission_list_cards_with(store, &ids, now_ms(), &store.human_attention_runs()?)?;
            let cards = cards
                .into_iter()
                .map(|card| (card["id"].as_str().unwrap().to_owned(), card))
                .collect();
            Ok((ids, cards))
        })
        .unwrap()
}

fn rows(publication: &Publication<MissionRows>) -> (Vec<String>, BTreeMap<String, Value>) {
    let cards = publication
        .rows
        .order
        .iter()
        .map(|id| (id.clone(), (*publication.rows.cards[id]).clone()))
        .collect();
    (publication.rows.order.clone(), cards)
}

/// Fold from `base`, check the publication against the oracle and a fold from nothing, and
/// return it with whether any row changed.
fn fold(store: &Store, base: Option<Publication<MissionRows>>) -> (Publication<MissionRows>, bool) {
    let now = now_ms();
    let (publication, changed) = match fold_missions(store, base.as_ref(), now).unwrap() {
        Some(folded) => folded,
        None => (base.expect("a current publication is the base"), false),
    };
    assert_eq!(publication.cut, store.index().unwrap(), "published at the current cut");
    let expected = oracle(store);
    assert_eq!(rows(&publication), expected, "the published rows are the direct fold's");
    let (fresh, _) = fold_missions(store, None, now).unwrap().unwrap();
    assert_eq!(rows(&fresh), expected, "a fold from nothing agrees");
    // Every window a socket holds is the direct read's, including its bound and continuation.
    for limit in [1, 2, 200] {
        let direct = store
            .read_snapshot(|_| {
                let mut ids = store.mission_collection_ids(false, 0, limit + 1)?;
                let mut has_more = ids.len() > limit;
                ids.truncate(limit);
                let mut items = client_v0::mission_list_cards_with(store, &ids, now, &store.human_attention_runs()?)?;
                has_more |= client_v0::bound_mission_cards(&mut items)?;
                Ok((items, has_more))
            })
            .unwrap();
        assert_eq!(mission_window(&publication.rows, limit).unwrap(), direct, "limit {limit}");
    }
    (publication, changed)
}

#[test]
fn published_missions_match_the_direct_fold_through_work_leases_failure_and_retirement() {
    let clock = Clock::at(1_800_000_000_000);
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("graph.db"), "cedar").unwrap();
    for name in ["garden/alpha", "garden/beta", "garden/gamma"] {
        mission(&store, name);
    }
    let alpha = start(&store, "garden/alpha", "alpha-1");
    let beta = start(&store, "garden/beta", "beta-1");
    let (publication, _) = fold(&store, None);
    assert_eq!(publication.rows.order.len(), 3);

    // Nothing changed: the publication stays current and no window rereads.
    let (publication, changed) = fold(&store, Some(publication));
    assert!(!changed);

    // A claim and progress refold only the claimed run's mission.
    let (plant, ash) = step(&alpha, "plant");
    store.set_step_state(&plant, "ready", None).unwrap();
    act(&store, &plant, &ash, "claim", "claim-plant");
    let (publication, changed) = fold(&store, Some(publication));
    assert!(changed);
    act(&store, &plant, &ash, "progress", "progress-plant");
    let (publication, _) = fold(&store, Some(publication));

    // The lease ends with no new claim: the card shows the step ready again.
    let lease = store.next_lease_end(now_ms()).unwrap().expect("the claim holds a lease");
    assert_eq!(publication.valid_until_unix_ms.map(|until| until <= lease), Some(true));
    clock.set(lease + 1);
    let (publication, changed) = fold(&store, Some(publication));
    assert!(changed, "the expired lease refolds alpha");

    // A failed step fails the run; the mission stays listed while recently ended, then leaves.
    let (water, birch) = step(&beta, "water");
    store.set_step_state(&water, "ready", None).unwrap();
    act(&store, &water, &birch, "claim", "claim-water");
    act(&store, &water, &birch, "fail", "fail-water");
    let (publication, _) = fold(&store, Some(publication));
    // Retrying the only failed step reopens the run.
    store.retry_failed_step(&water, "person/operator", "try again", "retry-water").unwrap();
    let (publication, _) = fold(&store, Some(publication));
    store.set_mission_run_state(&beta.id, "cancelled", "terminal", Some("no longer needed")).unwrap();
    let (publication, _) = fold(&store, Some(publication));
    assert!(publication.rows.order.iter().any(|id| id.ends_with("garden/beta")));
    let until = publication.rows.visible_until().expect("beta leaves the list");
    assert_eq!(publication.valid_until_unix_ms.map(|valid| valid <= until), Some(true));
    clock.set(until - 1);
    let (publication, changed) = fold(&store, Some(publication));
    assert!(!changed, "still recently ended");
    assert!(publication.rows.order.iter().any(|id| id.ends_with("garden/beta")));
    clock.set(until);
    let (publication, changed) = fold(&store, Some(publication));
    assert!(changed);
    assert!(!publication.rows.order.iter().any(|id| id.ends_with("garden/beta")));

    // A new mission, a retired one, and a rejected write that changes nothing.
    mission(&store, "garden/delta");
    store.retire_mission("mission/garden/gamma", "person/operator", "retire-gamma").unwrap();
    assert!(store
        .work_action(&plant, "progress", &WorkRequest {
            actor: Some("agent/garden/nobody".into()),
            incarnation: None,
            summary: Some("not mine".into()),
            reason: None,
            evidence: Vec::new(),
            idempotency_key: "rejected".into(),
        })
        .is_err());
    let (publication, _) = fold(&store, Some(publication));
    assert!(!publication.rows.order.iter().any(|id| id.ends_with("garden/gamma")));

    // Reopened, the store's first fold is the same list.
    drop(store);
    let store = Store::open(&root.path().join("graph.db"), "cedar").unwrap();
    let (reopened, _) = fold(&store, None);
    assert_eq!(rows(&reopened), rows(&publication));
}

#[test]
fn published_missions_follow_replication_in_any_order_and_fold_from_nothing_after_a_trim() {
    let _clock = Clock::at(1_800_000_000_000);
    let source = Store::open_memory("cedar").unwrap();
    for name in ["garden/alpha", "garden/beta"] {
        mission(&source, name);
    }
    let alpha = start(&source, "garden/alpha", "alpha-1");
    let (plant, ash) = step(&alpha, "plant");
    source.set_step_state(&plant, "ready", None).unwrap();
    act(&source, &plant, &ash, "claim", "claim-plant");
    start(&source, "garden/beta", "beta-1");
    source.bind_fleet(FLEET).unwrap();
    let exchange = source
        .export_replication_exchange(FLEET, &Default::default())
        .unwrap();
    assert!(exchange.envelopes.len() >= 3);
    for reverse in [false, true] {
        let target = Store::open_memory("alder").unwrap();
        mission(&target, "garden/local");
        let (publication, _) = fold(&target, None);
        let mut permuted = exchange.clone();
        if reverse {
            permuted.envelopes.reverse();
        }
        target.bind_fleet(FLEET).unwrap();
        target.receive_replication_exchange("cedar", FLEET, &permuted).unwrap();
        target.validate_replication_backlog().unwrap();
        target.apply_replication_repairs().unwrap();
        let projected = target.project_replication_backlog().unwrap();
        assert!(projected);
        let (publication, _) = fold(&target, Some(publication));
        assert_eq!(publication.rows.order.len(), 3, "{reverse}");
    }

    // A checkpoint trim deletes claims without a new one: the list folds from nothing.
    let store = Store::open_memory("cedar").unwrap();
    mission(&store, "garden/alpha");
    start(&store, "garden/alpha", "alpha-1");
    let list = store.published_missions_list();
    list.start("missions");
    let (publication, _) = fold(&store, None);
    list.publish(publication, 0);
    let plan = crate::store::plan_drops(&store.checkpoint_sealed_set(now_ms() + 1_000).unwrap());
    store
        .apply_checkpoint_drop("checkpoint/garden", &plan.envelopes, &plan.claims)
        .unwrap();
    assert!(list.base().is_none(), "forgotten after the trim");
    fold(&store, None);
}

/// The actors whose work windows the parity checks read, besides the unfiltered one.
const ACTORS: [Option<&str>; 4] = [None, Some("agent/garden/ash"), Some("garden/birch"), Some("agent/garden/nobody")];

/// What a work window reads directly at the current cut, at the list's time.
fn work_oracle(store: &Store, time: u128, actor: Option<&str>, limit: usize) -> (Vec<Value>, bool) {
    store
        .read_snapshot(|cut| {
            let mut items = client_work_resources(store, actor, false, time, cut)?;
            let has_more = items.len() > limit;
            items.truncate(limit);
            Ok((items, has_more))
        })
        .unwrap()
}

/// Fold the work list from `base`, check every actor's window against the direct read and a
/// fold from nothing, and return it with whether any row changed.
fn fold_work_checked(store: &Store, base: Option<Publication<WorkRows>>) -> (Publication<WorkRows>, bool) {
    let (publication, changed) = match fold_work(store, base.as_ref(), now_ms()).unwrap() {
        Some(folded) => folded,
        None => (base.expect("a current publication is the base"), false),
    };
    assert_eq!(publication.cut, store.index().unwrap(), "published at the current cut");
    let time = publication.rows.time_unix_ms;
    assert!(time >= store.projection_time_at(publication.cut).unwrap());
    let (fresh, _) = fold_work(store, None, now_ms()).unwrap().unwrap();
    for actor in ACTORS {
        for limit in [1, 2, 200] {
            let expected = work_oracle(store, time, actor, limit);
            let window = store.read_snapshot(|_| work_window(store, &publication.rows, actor, limit)).unwrap();
            assert_eq!(window, expected, "{actor:?} limit {limit}");
            if fresh.rows.time_unix_ms == time {
                let fresh = store.read_snapshot(|_| work_window(store, &fresh.rows, actor, limit)).unwrap();
                assert_eq!(fresh, expected, "from nothing: {actor:?} limit {limit}");
            }
        }
    }
    (publication, changed)
}

#[test]
fn published_work_matches_the_direct_read_for_every_actor_as_time_passes() {
    let clock = Clock::at(1_800_000_000_000);
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("graph.db"), "cedar").unwrap();
    for name in ["garden/alpha", "garden/beta"] {
        mission(&store, name);
    }
    let alpha = start(&store, "garden/alpha", "alpha-1");
    let beta = start(&store, "garden/beta", "beta-1");
    for run in [&alpha, &beta] {
        for path in ["plant", "water"] {
            store.set_step_state(&step(run, path).0, "ready", None).unwrap();
        }
    }
    let (publication, _) = fold_work_checked(&store, None);
    assert_eq!(publication.rows.order.len(), 4);
    let (publication, changed) = fold_work_checked(&store, Some(publication));
    assert!(!changed, "nothing changed");

    // A claim starts the step's execution time, which grows with the list's time while
    // unrelated claims move the cut.
    let (plant, ash) = step(&alpha, "plant");
    act(&store, &plant, &ash, "claim", "claim-plant");
    let (mut publication, _) = fold_work_checked(&store, Some(publication));
    for minute in 1..=3 {
        clock.set(1_800_000_000_000 + minute * 60_000);
        mission(&store, &format!("garden/filler-{minute}"));
        let (next, changed) = fold_work_checked(&store, Some(publication));
        assert!(changed, "the running step's time moved");
        publication = next;
    }
    act(&store, &plant, &ash, "progress", "progress-plant");
    let (publication, _) = fold_work_checked(&store, Some(publication));

    // The lease ends: once the list's time passes it, the step is ready again.
    let lease = store.next_lease_end(0).unwrap().expect("the claim holds a lease");
    clock.set(lease + 1);
    mission(&store, "garden/after-lease");
    let (publication, changed) = fold_work_checked(&store, Some(publication));
    assert!(changed);

    // Another seat claims and submits; a cancelled run's work leaves the list.
    let (water, birch) = step(&beta, "water");
    act(&store, &water, &birch, "claim", "claim-water");
    act(&store, &water, &birch, "submit", "submit-water");
    let (publication, _) = fold_work_checked(&store, Some(publication));
    store.set_mission_run_state(&beta.id, "cancelled", "terminal", Some("no longer needed")).unwrap();
    let (publication, _) = fold_work_checked(&store, Some(publication));
    assert!(beta.steps.iter().all(|step| !publication.rows.rows.contains_key(&step.subject)));

    // Reopened, the store's first fold is the same list.
    drop(store);
    let store = Store::open(&root.path().join("graph.db"), "cedar").unwrap();
    let (reopened, _) = fold_work_checked(&store, None);
    assert_eq!(reopened.rows.order, publication.rows.order);
}

#[test]
fn published_work_follows_replication_in_any_order_and_folds_from_nothing_after_a_trim() {
    let _clock = Clock::at(1_800_000_000_000);
    let source = Store::open_memory("cedar").unwrap();
    mission(&source, "garden/alpha");
    let alpha = start(&source, "garden/alpha", "alpha-1");
    let (plant, ash) = step(&alpha, "plant");
    source.set_step_state(&plant, "ready", None).unwrap();
    act(&source, &plant, &ash, "claim", "claim-plant");
    source.bind_fleet(FLEET).unwrap();
    let exchange = source.export_replication_exchange(FLEET, &Default::default()).unwrap();
    for reverse in [false, true] {
        let target = Store::open_memory("alder").unwrap();
        mission(&target, "garden/local");
        let local = start(&target, "garden/local", "local-1");
        target.set_step_state(&step(&local, "plant").0, "ready", None).unwrap();
        let (publication, _) = fold_work_checked(&target, None);
        let mut permuted = exchange.clone();
        if reverse {
            permuted.envelopes.reverse();
        }
        target.bind_fleet(FLEET).unwrap();
        target.receive_replication_exchange("cedar", FLEET, &permuted).unwrap();
        target.validate_replication_backlog().unwrap();
        target.apply_replication_repairs().unwrap();
        assert!(target.project_replication_backlog().unwrap());
        let (publication, _) = fold_work_checked(&target, Some(publication));
        assert!(publication.rows.rows.contains_key(&plant), "{reverse}");
    }

    let store = Store::open_memory("cedar").unwrap();
    mission(&store, "garden/alpha");
    let alpha = start(&store, "garden/alpha", "alpha-1");
    store.set_step_state(&step(&alpha, "plant").0, "ready", None).unwrap();
    let list = store.published_work_list();
    list.start("work");
    let (publication, _) = fold_work_checked(&store, None);
    list.publish(publication, 0);
    let plan = crate::store::plan_drops(&store.checkpoint_sealed_set(now_ms() + 1_000).unwrap());
    store.apply_checkpoint_drop("checkpoint/garden", &plan.envelopes, &plan.claims).unwrap();
    assert!(list.base().is_none(), "forgotten after the trim");
    fold_work_checked(&store, None);
}
