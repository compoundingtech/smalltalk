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

/// The tests' starting time: now, since the store stamps some times with the wall clock.
fn start_time() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()
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

/// Fold the store's missions list as its refresher does, from its newest publication unless
/// the projections were replaced since, check the publication against the direct read and a
/// fold from nothing, and return it with whether any row changed.
fn fold(store: &Store) -> (Arc<Publication<MissionRows>>, bool) {
    let list = store.published_missions_list();
    list.start();
    let now = now_ms();
    let (base, generation) = list.base();
    let changed = match fold_missions(store, base.as_deref()).unwrap() {
        Some((publication, changed)) => {
            list.publish(publication, generation).expect("no projection replaced meanwhile");
            changed
        }
        None => false,
    };
    let publication = list.newest().expect("a published list");
    assert_eq!(publication.cut, store.index().unwrap(), "published at the current cut");
    let expected = oracle(store);
    assert_eq!(rows(&publication), expected, "the published rows are the direct fold's");
    let (fresh, _) = fold_missions(store, None).unwrap().unwrap();
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
    let clock = Clock::at(start_time());
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("graph.db"), "cedar").unwrap();
    for name in ["garden/alpha", "garden/beta", "garden/gamma"] {
        mission(&store, name);
    }
    let alpha = start(&store, "garden/alpha", "alpha-1");
    let beta = start(&store, "garden/beta", "beta-1");
    let (publication, _) = fold(&store);
    assert_eq!(publication.rows.order.len(), 3);

    // Nothing changed: the publication stays current and no window rereads.
    let (_, changed) = fold(&store);
    assert!(!changed);

    // A claim and progress refold only the claimed run's mission.
    let (plant, ash) = step(&alpha, "plant");
    store.set_step_state(&plant, "ready", None).unwrap();
    act(&store, &plant, &ash, "claim", "claim-plant");
    let (_, changed) = fold(&store);
    assert!(changed);
    act(&store, &plant, &ash, "progress", "progress-plant");
    let (publication, _) = fold(&store);

    // The lease ends with no new claim: the card shows the step ready again.
    let lease = store.next_lease_end(now_ms()).unwrap().expect("the claim holds a lease");
    assert_eq!(publication.valid_until_unix_ms.map(|until| until <= lease), Some(true));
    clock.set(lease + 1);
    let (_, changed) = fold(&store);
    assert!(changed, "the expired lease refolds alpha");

    // A failed step fails the run; the mission stays listed while recently ended, then leaves.
    let (water, birch) = step(&beta, "water");
    store.set_step_state(&water, "ready", None).unwrap();
    act(&store, &water, &birch, "claim", "claim-water");
    act(&store, &water, &birch, "fail", "fail-water");
    fold(&store);
    // Retrying the only failed step reopens the run.
    store.retry_failed_step(&water, "person/operator", "try again", "retry-water").unwrap();
    fold(&store);
    store.set_mission_run_state(&beta.id, "cancelled", "terminal", Some("no longer needed")).unwrap();
    let (publication, _) = fold(&store);
    assert!(publication.rows.order.iter().any(|id| id.ends_with("garden/beta")));
    let until = publication.rows.visible_until().expect("beta leaves the list");
    assert_eq!(publication.valid_until_unix_ms.map(|valid| valid <= until), Some(true));
    clock.set(until - 1);
    let (publication, changed) = fold(&store);
    assert!(!changed, "still recently ended");
    assert!(publication.rows.order.iter().any(|id| id.ends_with("garden/beta")));
    clock.set(until);
    let (publication, changed) = fold(&store);
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
    let (publication, _) = fold(&store);
    assert!(!publication.rows.order.iter().any(|id| id.ends_with("garden/gamma")));

    // Reopened, the store's first fold is the same list.
    drop(store);
    let store = Store::open(&root.path().join("graph.db"), "cedar").unwrap();
    let (reopened, _) = fold(&store);
    assert_eq!(rows(&reopened), rows(&publication));
}

#[test]
fn published_missions_follow_replication_in_any_order_and_fold_from_nothing_after_a_trim() {
    let _clock = Clock::at(start_time());
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
        fold(&target);
        let mut permuted = exchange.clone();
        if reverse {
            permuted.envelopes.reverse();
        }
        target.bind_fleet(FLEET).unwrap();
        target.receive_replication_exchange("cedar", FLEET, &permuted).unwrap();
        target.validate_replication_backlog().unwrap();
        target.apply_replication_repairs().unwrap();
        // A fold between admission and projection reads the claims before their rows exist;
        // the pass that projects them moves the frontier, and the next fold reads them again.
        fold(&target);
        let projected = target.project_replication_backlog().unwrap();
        assert!(projected);
        let (publication, _) = fold(&target);
        assert_eq!(publication.rows.order.len(), 3, "{reverse}");
    }

    // A checkpoint trim deletes claims without a new one: the list folds from nothing.
    let store = Store::open_memory("cedar").unwrap();
    mission(&store, "garden/alpha");
    start(&store, "garden/alpha", "alpha-1");
    fold(&store);
    let before = store.published_missions_list().rebuilds()["start"];
    let plan = crate::store::plan_drops(&store.checkpoint_sealed_set(now_ms() + 1_000).unwrap());
    store
        .apply_checkpoint_drop("checkpoint/garden", &plan.envelopes, &plan.claims)
        .unwrap();
    fold(&store);
    // One fold from nothing for the trim, and the check's own.
    assert_eq!(store.published_missions_list().rebuilds()["start"], before + 2, "the trim folds from nothing");
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
/// Fold the store's work list as its refresher does, from its newest publication unless the
/// projections were replaced since.
fn fold_work_checked(store: &Store) -> (Arc<Publication<WorkRows>>, bool) {
    let list = store.published_work_list();
    list.start();
    let (base, generation) = list.base();
    let changed = match fold_work(store, base.as_deref()).unwrap() {
        Some((publication, changed)) => {
            list.publish(publication, generation).expect("no projection replaced meanwhile");
            changed
        }
        None => false,
    };
    let publication = list.newest().expect("a published list");
    assert_eq!(publication.cut, store.index().unwrap(), "published at the current cut");
    let time = publication.rows.time_unix_ms;
    assert!(time >= store.projection_time_at(publication.cut).unwrap());
    let (fresh, _) = fold_work(store, None).unwrap().unwrap();
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
    let started = start_time();
    let clock = Clock::at(started);
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
    let (publication, _) = fold_work_checked(&store);
    assert_eq!(publication.rows.order.len(), 4);
    let (_, changed) = fold_work_checked(&store);
    assert!(!changed, "nothing changed");

    // A seat's queue moves: no row changes, but the seat's windows reorder.
    store
        .move_seat_queue_run(&crate::model::SeatQueueMoveRequest {
            agent: "agent/garden/ash".into(),
            run: beta.id.clone(),
            placement: "top".into(),
            anchor: None,
            reason: Some("beta first".into()),
            actor: "person/operator".into(),
            idempotency_key: "move-beta".into(),
        })
        .unwrap();
    let (_, changed) = fold_work_checked(&store);
    assert!(changed, "the moved queue reorders ash's windows");

    // A claim starts the step's execution time, which grows with the list's time while
    // unrelated claims move the cut.
    let (plant, ash) = step(&alpha, "plant");
    act(&store, &plant, &ash, "claim", "claim-plant");
    fold_work_checked(&store);
    for minute in 1..=3 {
        clock.set(started + minute * 60_000);
        mission(&store, &format!("garden/filler-{minute}"));
        let (_, changed) = fold_work_checked(&store);
        assert!(changed, "the running step's time moved");
    }
    act(&store, &plant, &ash, "progress", "progress-plant");
    fold_work_checked(&store);

    // A quiet renewal moves the lease with no claim; the next fold still shows it.
    fold(&store);
    clock.set(started + 4 * 60_000);
    let before = store.index().unwrap();
    store
        .work_action(&plant, "renew", &WorkRequest {
            actor: Some(ash.clone()),
            incarnation: Some(format!("{ash}-1")),
            summary: None,
            reason: None,
            evidence: Vec::new(),
            idempotency_key: "renew-plant-quietly".into(),
        })
        .unwrap();
    assert_eq!(store.index().unwrap(), before, "the renewal appended no claim");
    // Folded on its own, with no claim and no new cut, in both lists: the card shows the
    // step's `since`, which the renewal moved too.
    let (_, changed) = fold_work_checked(&store);
    assert!(changed, "the quiet renewal moved the step's lease");
    let (_, changed) = fold(&store);
    assert!(changed, "the quiet renewal moved the step's since");



    // The lease ends: once the list's time passes it, the step is ready again.
    let lease = store.next_lease_end(0).unwrap().expect("the claim holds a lease");
    clock.set(lease + 1);
    mission(&store, "garden/after-lease");
    let (_, changed) = fold_work_checked(&store);
    assert!(changed);

    // Another seat claims and submits its next work; a cancelled run's work leaves the list.
    let (water, birch) = step(&alpha, "water");
    act(&store, &water, &birch, "claim", "claim-water");
    act(&store, &water, &birch, "complete", "complete-water");
    fold_work_checked(&store);
    store.set_mission_run_state(&beta.id, "cancelled", "terminal", Some("no longer needed")).unwrap();
    let (publication, _) = fold_work_checked(&store);
    assert!(beta.steps.iter().all(|step| !publication.rows.rows.contains_key(&step.subject)));

    // Reopened, the store's first fold is the same list.
    drop(store);
    let store = Store::open(&root.path().join("graph.db"), "cedar").unwrap();
    let (reopened, _) = fold_work_checked(&store);
    assert_eq!(reopened.rows.order, publication.rows.order);
}

#[test]
fn published_work_follows_replication_in_any_order_and_folds_from_nothing_after_a_trim() {
    let _clock = Clock::at(start_time());
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
        fold_work_checked(&target);
        let mut permuted = exchange.clone();
        if reverse {
            permuted.envelopes.reverse();
        }
        target.bind_fleet(FLEET).unwrap();
        target.receive_replication_exchange("cedar", FLEET, &permuted).unwrap();
        target.validate_replication_backlog().unwrap();
        target.apply_replication_repairs().unwrap();
        fold_work_checked(&target);
        assert!(target.project_replication_backlog().unwrap());
        let (publication, _) = fold_work_checked(&target);
        assert!(publication.rows.rows.contains_key(&plant), "{reverse}");
    }

    let store = Store::open_memory("cedar").unwrap();
    mission(&store, "garden/alpha");
    let alpha = start(&store, "garden/alpha", "alpha-1");
    store.set_step_state(&step(&alpha, "plant").0, "ready", None).unwrap();
    fold_work_checked(&store);
    let before = store.published_work_list().rebuilds()["start"];
    let plan = crate::store::plan_drops(&store.checkpoint_sealed_set(now_ms() + 1_000).unwrap());
    store.apply_checkpoint_drop("checkpoint/garden", &plan.envelopes, &plan.claims).unwrap();
    fold_work_checked(&store);
    // One fold from nothing for the trim, and the check's own.
    assert_eq!(store.published_work_list().rebuilds()["start"], before + 2, "the trim folds from nothing");
}

#[test]
fn published_lists_follow_many_changes_and_a_revision_proposal() {
    let _clock = Clock::at(start_time());
    let store = Store::open_memory("cedar").unwrap();
    mission(&store, "garden/alpha");
    let alpha = start(&store, "garden/alpha", "alpha-1");
    let (plant, ash) = step(&alpha, "plant");
    store.set_step_state(&plant, "ready", None).unwrap();
    store.set_step_state(&step(&alpha, "water").0, "ready", None).unwrap();
    act(&store, &plant, &ash, "claim", "claim-plant");
    fold(&store);
    fold_work_checked(&store);

    // More changed missions and steps than one snapshot refolds: chunks, then one cut.
    for number in 0..FOLD_CHUNK + 3 {
        let name = format!("garden/many-{number}");
        mission(&store, &name);
        let run = start(&store, &name, &format!("many-{number}"));
        for path in ["plant", "water"] {
            store.set_step_state(&step(&run, path).0, "ready", None).unwrap();
        }
    }
    fold(&store);
    fold_work_checked(&store);
    assert_eq!(store.published_missions_list().rebuilds()["chunked: missions changed"], 1);
    assert_eq!(store.published_work_list().rebuilds()["chunked: work changed"], 1);

    // A revision proposal waits for its reviewer and then applies, changing the run with no
    // claim on the run itself.
    let proposed = |goal: &str, key: &str| {
        let source = format!(
            r#"version 2
mission "garden/proposed" state="ready" revisions="human-only" revision-reviewer="person/reviewer" {{
  goal "Grow by proposal."
  agent "owner" {{ workspace "."; command "true" }}
  step "plant" {{ assigned-to "agent/garden/ash"; goal {goal:?} }}
}}"#
        );
        let intent = crate::graph::parse_intent(&source, store.origin()).unwrap();
        let preview = store
            .mission(&intent, crate::model::IntentInput { kdl: source.clone(), source_name: None })
            .unwrap();
        store.apply_as(&intent, &preview.subject_tokens, key, Some("person/operator")).unwrap();
        intent.missions["garden/proposed"].clone()
    };
    proposed("Plant in beds.", "publish-proposed-beds");
    let run = start(&store, "garden/proposed", "proposed-1");
    fold(&store);
    fold_work_checked(&store);
    let rows = proposed("Plant in rows.", "publish-proposed-rows");
    // A proposal cancelled before review releases the run.
    let cancelled = store
        .create_revision_proposal(
            &run.id,
            &rows,
            &format!("agent/{}/owner", run.id),
            "try rows",
            "proposal-cancelled",
        )
        .unwrap();
    fold(&store);
    fold_work_checked(&store);
    store
        .cancel_revision_proposal(&cancelled.id, &format!("agent/{}/owner", run.id), Some("not yet"), "proposal-cancel")
        .unwrap();
    fold(&store);
    fold_work_checked(&store);
    let proposal = store
        .create_revision_proposal(
            &run.id,
            &rows,
            &format!("agent/{}/owner", run.id),
            "grow in rows",
            "proposal-create",
        )
        .unwrap();
    fold(&store);
    fold_work_checked(&store);
    store
        .approve_revision_proposal(
            &proposal.id,
            "person/reviewer",
            proposal.preview_hash.as_deref().unwrap(),
            "proposal-approve",
        )
        .unwrap();
    fold(&store);
    fold_work_checked(&store);
}

#[test]
fn a_forgotten_list_keeps_its_rows_until_refolded_and_withdraws_its_view_too() {
    let _clock = Clock::at(start_time());
    let store = Store::open_memory("cedar").unwrap();
    mission(&store, "garden/alpha");
    fold(&store);
    let list = store.published_missions_list();
    let (_, generation) = list.base();
    store.publish_collection_view("missions");
    assert!(store.collection_view_published("missions"));
    list.forget();
    assert!(store.published_missions().is_some(), "readers keep the rows until the refold");
    // A fold under way when the projections were replaced publishes nothing.
    let (stale, _) = fold_missions(&store, store.published_missions().as_deref()).unwrap().unwrap_or_else(|| {
        fold_missions(&store, None).unwrap().unwrap()
    });
    assert_eq!(list.publish(stale, generation), None);
    // A refresher that then keeps failing withdraws the view, served list or not.
    assert!(list.withdraw());
    withdraw(&store, |store| store.published_missions_list(), "missions");
    assert!(!store.collection_view_published("missions"));
    assert!(store.published_missions().is_none());
    // Its next fold, from nothing, serves the list again.
    fold(&store);
    assert!(store.published_missions().is_some());
}

#[test]
fn a_refresher_that_ends_withdraws_its_list_and_view() {
    let _clock = Clock::at(start_time());
    let store = Arc::new(Store::open_memory("cedar").unwrap());
    mission(&store, "garden/alpha");
    fold(&store);
    store.publish_collection_view("missions");
    let guard = Withdraw { store: Arc::clone(&store), list: |store| store.published_missions_list(), name: "missions" };
    drop(guard);
    assert!(store.published_missions().is_none());
    assert!(!store.collection_view_published("missions"));
}

#[test]
fn a_fold_reads_claims_from_the_frontier_it_saw_and_rebuilds_past_too_many() {
    // No replication pass since the base: read from the base's cut.
    assert_eq!(claims_from(500, 100, 520, 100), Some(500));
    // A pass moved the frontier: read again from where the base saw it.
    assert_eq!(claims_from(500, 100, 520, 520), Some(100));
    // Too many claims to read in one snapshot: fold from nothing.
    assert_eq!(claims_from(500, 500, 500 + FOLD_CLAIMS, 500), Some(500));
    assert_eq!(claims_from(500, 500, 501 + FOLD_CLAIMS, 500), None);
    assert_eq!(claims_from(20_000, 0, 20_010, 20_010), None, "a frontier that first appears");
}
