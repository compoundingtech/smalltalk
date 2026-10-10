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
    assert_eq!(publication.rows.orders_cut, publication.cut, "seat orders read at the rows' own cut");
    // Each shown seat's queue order, kept incrementally, is the order a fold from nothing reads
    // at the same cut: a stale order shows here even for a seat no fixture actor reads.
    // Nothing writes meanwhile, so both folds read the same cut; a skipped check would hide.
    assert_eq!(fresh.cut, publication.cut, "the fold from nothing reads the same cut");
    let shown = publication.rows.seats_shown();
    assert_eq!(shown, fresh.rows.seats_shown());
    for seat in &shown {
        assert_eq!(
            publication.rows.seat_orders.get(seat),
            fresh.rows.seat_orders.get(seat),
            "{seat}'s queue order"
        );
        assert_eq!(publication.rows.seat_runs.get(seat), fresh.rows.seat_runs.get(seat), "the runs {seat}'s queue read");
    }
    // The reverse index is exactly the inverse of the registered seats' runs.
    let mut inverse = HashMap::<String, BTreeSet<String>>::new();
    for (seat, runs) in publication.rows.seat_runs.iter() {
        for run in runs.iter() {
            inverse.entry(run.clone()).or_default().insert(seat.clone());
        }
    }
    let index = publication.rows.run_seats.iter().map(|(run, seats)| (run.clone(), (**seats).clone())).collect::<HashMap<_, _>>();
    assert_eq!(index, inverse, "run_seats inverts seat_runs, with no empty bucket");
    for actor in ACTORS {
        // Every page the HTTP list slices from the publication, in turn, is the direct read.
        let (all, _) = work_oracle(store, time, actor, usize::MAX);
        let indexes = actor.map(|actor| publication.rows.actor_rows(actor).0);
        for limit in [1, 2, 3] {
            let (mut offset, mut pages) = (0, Vec::new());
            loop {
                let (items, has_more) =
                    publication.rows.page_of(indexes.as_deref().map(Vec::as_slice), offset, limit);
                assert!(items.len() <= limit);
                offset += items.len();
                pages.extend(items);
                if !has_more {
                    break;
                }
            }
            assert_eq!(pages, all, "every page: {actor:?} limit {limit}");
        }
    }
    for actor in ACTORS {
        for limit in [1, 2, 200] {
            let expected = work_oracle(store, time, actor, limit);
            let window = work_window(&publication.rows, actor, limit);
            assert_eq!(window, expected, "{actor:?} limit {limit}");
            if fresh.rows.time_unix_ms == time {
                let fresh = work_window(&fresh.rows, actor, limit);
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
    // And back, so alpha's step is ash's next work again.
    store
        .move_seat_queue_run(&crate::model::SeatQueueMoveRequest {
            agent: "agent/garden/ash".into(),
            run: alpha.id.clone(),
            placement: "top".into(),
            anchor: None,
            reason: Some("alpha first".into()),
            actor: "person/operator".into(),
            idempotency_key: "move-alpha".into(),
        })
        .unwrap();
    let (_, changed) = fold_work_checked(&store);
    assert!(changed);

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
    // claim on the run itself. The approved one moves the step from ash's queue to birch's.
    let proposed = |goal: &str, seat: &str, key: &str| {
        let source = format!(
            r#"version 2
mission "garden/proposed" state="ready" revisions="human-only" revision-reviewer="person/reviewer" {{
  goal "Grow by proposal."
  agent "owner" {{ workspace "."; command "true" }}
  step "plant" {{ assigned-to {seat:?}; goal {goal:?} }}
}}"#
        );
        let intent = crate::graph::parse_intent(&source, store.origin()).unwrap();
        let preview = store
            .mission(&intent, crate::model::IntentInput { kdl: source.clone(), source_name: None })
            .unwrap();
        store.apply_as(&intent, &preview.subject_tokens, key, Some("person/operator")).unwrap();
        intent.missions["garden/proposed"].clone()
    };
    proposed("Plant in beds.", "agent/garden/ash", "publish-proposed-beds");
    let run = start(&store, "garden/proposed", "proposed-1");
    fold(&store);
    fold_work_checked(&store);
    let rows = proposed("Plant in rows.", "agent/garden/birch", "publish-proposed-rows");
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

#[test]
fn published_lists_follow_a_nested_run_tree() {
    let _clock = Clock::at(start_time());
    let store = Store::open_memory("cedar").unwrap();
    for name in ["garden/parent", "garden/child"] {
        mission(&store, name);
    }
    let child_of = |parent: &crate::model::MissionRunView, key: &str| {
        let (starter, _) = step(parent, "plant");
        store
            .create_child_mission_run(
                &MissionRunRequest {
                    mission: "garden/child".into(),
                    revision: None,
                    workspace: "/work/garden".into(),
                    requester: Some("person/operator".into()),
                    mode: None,
                    inputs: BTreeMap::new(),
                    idempotency_key: key.into(),
                },
                parent,
                &starter,
                None,
            )
            .unwrap()
    };
    let ready = |run: &crate::model::MissionRunView| {
        for path in ["plant", "water"] {
            store.set_step_state(&step(run, path).0, "ready", None).unwrap();
        }
    };
    // Root, child and grandchild: three levels of one tree, and a second tree beside it.
    let root = start(&store, "garden/parent", "root-1");
    let child = child_of(&root, "child-1");
    let grandchild = child_of(&child, "grandchild-1");
    let other = start(&store, "garden/parent", "root-2");
    let other_child = child_of(&other, "child-2");
    for run in [&root, &child, &grandchild, &other, &other_child] {
        ready(run);
    }
    fold(&store);
    fold_work_checked(&store);

    // A child's state ends its own and its child's steps; the root's rows stay.
    store.set_mission_run_state(&child.id, "cancelled", "terminal", Some("child done")).unwrap();
    fold(&store);
    let (work, _) = fold_work_checked(&store);
    for run in [&child, &grandchild] {
        assert!(run.steps.iter().all(|step| !work.rows.rows.contains_key(&step.subject)));
    }
    assert!(root.steps.iter().all(|step| work.rows.rows.contains_key(&step.subject)));

    // A root's state ends the steps of every run under it.
    store.set_mission_run_state(&other.id, "cancelled", "terminal", Some("tree done")).unwrap();
    fold(&store);
    let (work, _) = fold_work_checked(&store);
    for run in [&other, &other_child] {
        assert!(run.steps.iter().all(|step| !work.rows.rows.contains_key(&step.subject)));
    }
}

fn app_state(root: &std::path::Path) -> AppState {
    AppState {
        store: Arc::new(Store::open(&root.join("graph.db"), "cedar").unwrap()),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "cedar".into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: root.join("unused-pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: crate::model::PlannerSpec::default(),
    }
}

fn work_query(actor: Option<&str>, limit: usize) -> ClientListQuery {
    ClientListQuery { actor: actor.map(str::to_owned), limit: Some(limit), ..Default::default() }
}

/// A current work page through the route's handler, under the snapshot the request middleware
/// would give it: the store's for a first page, the cursor's for a continuation.
async fn work_page(state: &AppState, query: ClientListQuery) -> Result<crate::model::ClientResourcePage, ApiError> {
    let snapshot = match &query.cursor {
        Some(cursor) => decode_client_cursor_snapshot(cursor).unwrap(),
        None => new_client_snapshot(state),
    };
    client_work(State(state.clone()), Extension(snapshot), Query(query)).await.map(|(_, Json(page))| page)
}

/// Every page of the current work list, first to last, and that none read the list directly.
async fn every_work_page(state: &AppState, actor: Option<&str>, limit: usize) -> Vec<Value> {
    let direct = state.store.direct_work_reads();
    let mut page = work_page(state, work_query(actor, limit)).await.unwrap();
    let mut items = page.items.clone();
    while let Some(cursor) = page.page.next_cursor.clone() {
        assert!(page.items.len() == limit && page.page.has_more);
        page = work_page(state, ClientListQuery { cursor: Some(cursor), ..work_query(actor, limit) }).await.unwrap();
        items.extend(page.items.clone());
    }
    assert_eq!(state.store.direct_work_reads(), direct, "no page read the list directly");
    items
}

fn served(list: &PublishedList<WorkRows>) -> (Arc<Publication<WorkRows>>, crate::store::published_list::PublicationId) {
    match list.current() {
        crate::store::published_list::ListRead::Served(publication, id) => (publication, id),
        _ => panic!("the work list is served"),
    }
}

#[tokio::test]
async fn http_work_pages_come_from_publications_and_never_read_the_list_directly() {
    let root = tempfile::tempdir().unwrap();
    let state = app_state(root.path());
    let store = &state.store;
    for name in ["garden/alpha", "garden/beta"] {
        mission(store, name);
    }
    let alpha = start(store, "garden/alpha", "alpha-1");
    let beta = start(store, "garden/beta", "beta-1");
    for run in [&alpha, &beta] {
        for path in ["plant", "water"] {
            store.set_step_state(&step(run, path).0, "ready", None).unwrap();
        }
    }
    // With no refresher, a page reads directly, as before.
    let direct = store.direct_work_reads();
    assert_eq!(work_page(&state, work_query(None, 200)).await.unwrap().items.len(), 4);
    assert_eq!(store.direct_work_reads(), direct + 1);
    // Held before its refresher starts: not ready, retryably, and never a direct read.
    let list = store.published_work_list();
    store.hold_collection_view("work");
    let refused = work_page(&state, work_query(None, 200)).await.unwrap_err();
    assert_eq!((refused.status, refused.code.as_str()), (StatusCode::SERVICE_UNAVAILABLE, WORK_LIST_NOT_READY));
    assert_eq!(refused.details["reason"], WORK_LIST_NOT_READY);
    assert!(client_error_retryable(refused.status, Some(&refused.code)));
    assert_eq!(store.direct_work_reads(), direct + 1);
    list.start();
    assert_eq!(work_page(&state, work_query(None, 200)).await.unwrap_err().code, WORK_LIST_NOT_READY);

    // Every page, for every actor, is the direct read at the publication's cut and time.
    let (publication, _) = fold_work_checked(store);
    for actor in ACTORS {
        let (expected, _) = work_oracle(store, publication.rows.time_unix_ms, actor, usize::MAX);
        for limit in [1, 3, 200] {
            assert_eq!(every_work_page(&state, actor, limit).await, expected, "{actor:?} limit {limit}");
        }
    }

    // A cursor keeps its publication after a newer one of the same generation.
    let first = work_page(&state, work_query(None, 1)).await.unwrap();
    let cursor = first.page.next_cursor.clone().unwrap();
    let second = publication.rows.page_of(None, 1, 1).0;
    mission(store, "garden/gamma");
    let gamma = start(store, "garden/gamma", "gamma-1");
    store.set_step_state(&step(&gamma, "plant").0, "ready", None).unwrap();
    let (newer, _) = fold_work_checked(store);
    assert!(newer.cut > publication.cut);
    let next = work_page(&state, ClientListQuery { cursor: Some(cursor.clone()), ..work_query(None, 1) }).await.unwrap();
    assert_eq!(next.items, second, "the first page's publication, not the newer one");
    // A forget replaced the projections behind it: it expires, and first pages wait for the
    // fold from nothing.
    list.forget();
    let expired = work_page(&state, ClientListQuery { cursor: Some(cursor), ..work_query(None, 1) }).await.unwrap_err();
    assert_eq!((expired.status, expired.code.as_str()), (StatusCode::GONE, "page-cursor-expired"));
    assert_eq!(work_page(&state, work_query(None, 1)).await.unwrap_err().code, WORK_LIST_NOT_READY);
    fold_work_checked(store);
    // A forget between acquiring a publication and emitting its page refuses the page; a
    // withdrawal alone does not, since its rows still hold at their own cut.
    let (acquired, id) = served(list);
    list.forget();
    let refused = client_work_published_page(&state, &acquired, id, &work_query(None, 1)).unwrap_err();
    assert_eq!(refused.code, WORK_LIST_NOT_READY);
    fold_work_checked(store);
    let (acquired, id) = served(list);
    list.withdraw();
    assert!(client_work_published_page(&state, &acquired, id, &work_query(None, 1)).is_ok());
    assert!(list.serve_again(list.generation()));

    // Fresh: a publication at the request's cut answers at once.
    let fresh = ClientListQuery { fresh: true, ..work_query(None, 200) };
    assert_eq!(work_page(&state, fresh.clone()).await.unwrap().items.len(), 5);
    // A commit the list has not folded: the bounded wait ends not fresh, naming both cuts.
    mission(store, "garden/delta");
    let newest = served(list).0.cut;
    let waited = std::time::Instant::now();
    let stale = work_page(&state, fresh.clone()).await.unwrap_err();
    assert!(waited.elapsed() >= WORK_LIST_FRESH_WAIT);
    assert_eq!(stale.code, WORK_LIST_NOT_FRESH);
    assert_eq!(stale.details["wanted_cut"], json!(store.index().unwrap()));
    assert_eq!(stale.details["newest_cut"], json!(newest));
    // Folded while it waits: the wait ends with that publication.
    let waiting = tokio::spawn({
        let (state, fresh) = (state.clone(), fresh.clone());
        async move { work_page(&state, fresh).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let folder = Arc::clone(store);
    tokio::task::spawn_blocking(move || refresh_work_once(&folder)).await.unwrap();
    let page = waiting.await.unwrap().unwrap();
    assert_eq!(page.items.len(), 5);
    // Withdrawn while it waits: not ready.
    mission(store, "garden/echo");
    let waiting = tokio::spawn({
        let (state, fresh) = (state.clone(), fresh.clone());
        async move { work_page(&state, fresh).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    list.withdraw();
    assert_eq!(waiting.await.unwrap().unwrap_err().code, WORK_LIST_NOT_READY);
    refresh_work_once(store);
    // The refresher ends while it waits: unavailable, and not retryable.
    mission(store, "garden/foxtrot");
    let waiting = tokio::spawn({
        let (state, fresh) = (state.clone(), fresh.clone());
        async move { work_page(&state, fresh).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    list.end();
    let ended = waiting.await.unwrap().unwrap_err();
    assert_eq!(ended.code, WORK_LIST_ENDED);
    assert!(!client_error_retryable(ended.status, Some(&ended.code)));
    let direct = store.direct_work_reads();
    assert_eq!(work_page(&state, work_query(None, 1)).await.unwrap_err().code, WORK_LIST_ENDED);
    assert_eq!(store.direct_work_reads(), direct, "an ended list is never read directly");
}

#[test]
fn an_actors_order_is_built_once_per_publication_by_its_first_read() {
    let _clock = Clock::at(start_time());
    let store = Store::open_memory("cedar").unwrap();
    mission(&store, "garden/alpha");
    let alpha = start(&store, "garden/alpha", "alpha-1");
    for path in ["plant", "water"] {
        store.set_step_state(&step(&alpha, path).0, "ready", None).unwrap();
    }
    let (publication, _) = fold_work(&store, None).unwrap().unwrap();
    let rows = &publication.rows;
    let count = |counter: &std::sync::atomic::AtomicUsize| counter.load(std::sync::atomic::Ordering::Relaxed);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| rows.actor_rows("agent/garden/ash"));
        }
    });
    assert_eq!((count(&rows.actors.builds), count(&rows.actors.hits)), (1, 7), "concurrent reads wait for one build");
    assert_eq!(count(&rows.actors.scanned), rows.order.len(), "the build scans the current rows once");
    let (indexes, _) = rows.actor_rows("agent/garden/ash");
    // One kept actor: its allocated indexes, its name, and its entry and cell.
    assert!(rows.actors.retained_bytes() >= indexes.capacity() * 4 + "agent/garden/ash".len());
    // Past the cap, the least recently read actor is dropped and built again when read.
    for n in 0..crate::store::work_list::ACTOR_ORDERS {
        rows.actor_rows(&format!("agent/garden/other-{n}"));
    }
    assert!(rows.actor_rows("agent/garden/ash").1, "evicted, so built again");
    let (kept, in_flight, overflow) = rows.actors.kept();
    assert_eq!((kept, in_flight, overflow), (crate::store::work_list::ACTOR_ORDERS, 0, 0));
    // Each kept actor holds at most four bytes a row of this publication, besides its name and
    // its entry's fixed size.
    assert!(rows.actors.retained_bytes() <= kept * (rows.order.len() * 4 + 256));
}

/// A mission whose one step is assigned to `seat`.
fn seat_mission(store: &Store, name: &str, seat: &str) {
    publish(
        store,
        &format!(
            r#"version 2
mission "{name}" state="ready" {{
  goal "Tend one seat."
  concurrent-runs max=64
  step "plant" {{ assigned-to "{seat}" }}
}}"#
        ),
        &format!("publish-{name}"),
    );
}

/// Start a run of `name` and make its steps ready.
fn ready_run(store: &Store, name: &str, key: &str) -> crate::model::MissionRunView {
    let run = start(store, name, key);
    for step in &run.steps {
        store.set_step_state(&step.subject, "ready", None).unwrap();
    }
    run
}

#[test]
fn a_work_page_clones_only_its_rows_along_each_growing_inventory() {
    let _clock = Clock::at(start_time());
    let store = Store::open_memory("cedar").unwrap();
    let count = |counter: &std::sync::atomic::AtomicUsize| counter.load(std::sync::atomic::Ordering::Relaxed);
    mission(&store, "garden/alpha");
    for n in 0..2 {
        ready_run(&store, "garden/alpha", &format!("alpha-{n}"));
    }
    seat_mission(&store, "garden/birch-more", "agent/garden/birch");
    seat_mission(&store, "garden/ash-more", "agent/garden/ash");
    /// What a fresh publication's first pages cost: the fleet page's clones and serialized
    /// bytes and rows scanned, and ash's first build's scans, page clones and bytes.
    struct Cost {
        rows: usize,
        fleet: (usize, usize, usize),
        ash: (usize, usize, usize),
    }
    let measure = |store: &Store| {
        let (publication, _) = fold_work(store, None).unwrap().unwrap();
        let rows = &publication.rows;
        let (fleet, _) = rows.page_of(None, 0, 2);
        let fleet = (fleet.len(), serde_json::to_vec(&fleet).unwrap().len(), count(&rows.actors.scanned));
        let (indexes, built) = rows.actor_rows("agent/garden/ash");
        assert!(built);
        let (ash, _) = rows.page_of(Some(&indexes), 0, 2);
        let ash = (count(&rows.actors.scanned), ash.len(), serde_json::to_vec(&ash).unwrap().len());
        Cost { rows: rows.order.len(), fleet, ash }
    };
    let base = measure(&store);
    assert_eq!((base.fleet.0, base.fleet.2), (2, 0), "a fleet page clones its two rows and scans none");
    assert_eq!(base.ash.0, base.rows, "ash's first read scans the current rows once: the residual");
    let grows = |store: &Store, axis: &str, before: &Cost| {
        let after = measure(store);
        assert_eq!((after.fleet.0, after.fleet.2), (2, 0), "{axis}: still two clones, no scan");
        assert!(after.fleet.1 <= before.fleet.1 + 256, "{axis}: the fleet page's bytes do not grow");
        assert_eq!(after.ash.1, 2, "{axis}");
        assert!(after.ash.2 <= before.ash.2 + 256, "{axis}: ash's page bytes do not grow");
        assert_eq!(after.ash.0, after.rows, "{axis}: ash's build scans exactly the current rows");
        after
    };
    // Unrelated current work: another seat's rows.
    for n in 0..8 {
        ready_run(&store, "garden/birch-more", &format!("birch-{n}"));
    }
    let unrelated = grows(&store, "unrelated current", &base);
    assert_eq!(unrelated.rows, base.rows + 8);
    // Ash's own work past its first page.
    for n in 0..8 {
        ready_run(&store, "garden/ash-more", &format!("ash-{n}"));
    }
    let off_page = grows(&store, "ash off-page", &unrelated);
    assert_eq!(off_page.rows, unrelated.rows + 8);
    // Finished work: never held, so nothing a page or build reads grows.
    for n in 0..8 {
        let run = start(&store, "garden/ash-more", &format!("done-{n}"));
        let (plant, ash) = step(&run, "plant");
        store.set_step_state(&plant, "ready", None).unwrap();
        act(&store, &plant, &ash, "claim", &format!("claim-done-{n}"));
        act(&store, &plant, &ash, "complete", &format!("complete-done-{n}"));
    }
    let history = grows(&store, "history", &off_page);
    assert_eq!(history.rows, off_page.rows, "finished work is not held");
}

#[test]
fn an_incremental_fold_reads_only_the_seats_it_can_reorder() {
    let _clock = Clock::at(start_time());
    let store = Store::open_memory("cedar").unwrap();
    let mut runs = BTreeMap::new();
    for n in 0..12 {
        let (name, seat) = (format!("garden/seat-{n}"), format!("agent/garden/seat-{n}"));
        seat_mission(&store, &name, &seat);
        runs.insert(n, ready_run(&store, &name, &format!("seat-{n}-1")));
    }
    let second = ready_run(&store, "garden/seat-5", "seat-5-2");
    store.published_work_list().start();
    // From nothing: every shown seat once, charged as the cold fold.
    refresh_work_once(&store);
    let cold = store.seat_order_reads();
    assert_eq!(cold, 12);
    // A commit no row or queue reads: no seat.
    mission(&store, "garden/unrelated");
    refresh_work_once(&store);
    assert_eq!(store.seat_order_reads(), cold, "an unrelated commit rereads no seat");
    // One seat's step changes: that seat alone, and its queue is unchanged, so the seat maps
    // are shared with the previous publication, not copied.
    let before = store.published_work().unwrap();
    let (plant, seat) = step(&runs[&3], "plant");
    act(&store, &plant, &seat, "claim", "claim-seat-3");
    refresh_work_once(&store);
    assert_eq!(store.seat_order_reads(), cold + 1, "only seat 3");
    let after = store.published_work().unwrap();
    assert!(after.cut > before.cut);
    assert!(Arc::ptr_eq(&before.rows.seat_orders, &after.rows.seat_orders), "an unchanged queue copies no order map");
    assert!(Arc::ptr_eq(&before.rows.seat_runs, &after.rows.seat_runs));
    assert!(Arc::ptr_eq(&before.rows.run_seats, &after.rows.run_seats));
    assert_eq!(after.rows.actors.unchanged_seats.load(std::sync::atomic::Ordering::Relaxed), 1);
    // One seat's queue moves: that seat alone.
    store
        .move_seat_queue_run(&crate::model::SeatQueueMoveRequest {
            agent: "agent/garden/seat-5".into(),
            run: second.id.clone(),
            placement: "top".into(),
            anchor: None,
            reason: Some("second first".into()),
            actor: "person/operator".into(),
            idempotency_key: "move-seat-5".into(),
        })
        .unwrap();
    refresh_work_once(&store);
    assert_eq!(store.seat_order_reads(), cold + 2, "only seat 5");
    let moved = store.published_work().unwrap();
    assert!(!Arc::ptr_eq(&after.rows.seat_orders, &moved.rows.seat_orders), "a changed queue copies the map");
    assert_ne!(after.rows.seat_orders.get("agent/garden/seat-5"), moved.rows.seat_orders.get("agent/garden/seat-5"));
    // And the list is still the direct read for each actor.
    fold_work_checked(&store);
}

#[tokio::test]
async fn an_edited_or_foreign_published_work_cursor_expires() {
    let root = tempfile::tempdir().unwrap();
    let state = app_state(root.path());
    let store = &state.store;
    mission(store, "garden/alpha");
    for n in 0..2 {
        ready_run(store, "garden/alpha", &format!("alpha-{n}"));
    }
    store.published_work_list().start();
    let (publication, _) = fold_work_checked(store);
    let first = |actor: Option<&'static str>| {
        let state = state.clone();
        async move { work_page(&state, work_query(actor, 1)).await.unwrap().page.next_cursor.unwrap() }
    };
    let edit = |cursor: &str, change: &dyn Fn(&mut ClientPageCursor)| {
        let mut decoded = decode_client_cursor(cursor).unwrap();
        change(&mut decoded);
        encode_client_cursor(&decoded).unwrap()
    };
    let continue_with = |state: &AppState, cursor: String, actor: Option<&str>| {
        let query = ClientListQuery { cursor: Some(cursor), actor: actor.map(str::to_owned), ..Default::default() };
        let state = state.clone();
        async move { work_page(&state, query).await }
    };
    let expired = |result: Result<crate::model::ClientResourcePage, ApiError>, case: &str| {
        let error = result.expect_err(case);
        assert_eq!((error.status, error.code.as_str()), (StatusCode::GONE, "page-cursor-expired"), "{case}");
    };
    let cursor = first(None).await;
    // An unedited continuation is the next slice of the same publication.
    let next = continue_with(&state, cursor.clone(), None).await.unwrap();
    assert_eq!(next.items, publication.rows.page_of(None, 1, 1).0);
    // 1. An edited limit, with no limit asked: above the maximum, or another in range.
    for limit in [CLIENT_MAX_PAGE_ITEMS + 1, usize::MAX, 0, 2] {
        let edited = edit(&cursor, &|cursor| cursor.limit = limit);
        expired(continue_with(&state, edited, None).await, &format!("limit {limit}"));
    }
    // 2. Ash's cursor carrying birch's page's digest.
    let ash = first(Some("agent/garden/ash")).await;
    let birch = decode_client_cursor(&first(Some("agent/garden/birch")).await).unwrap();
    let forged = edit(&ash, &|cursor| {
        cursor.items_digest = birch.items_digest.clone();
        cursor.expires_at_unix_ms = birch.expires_at_unix_ms;
    });
    expired(continue_with(&state, forged, Some("agent/garden/ash")).await, "another actor's digest");
    // 3. Another store's list in the same process, at the same generation.
    let other_root = tempfile::tempdir().unwrap();
    let other = app_state(other_root.path());
    mission(&other.store, "garden/alpha");
    other.store.published_work_list().start();
    fold_work_checked(&other.store);
    assert_eq!(other.store.published_work_list().generation(), store.published_work_list().generation());
    expired(continue_with(&other, cursor.clone(), None).await, "another store's list");
    // Each other issued filter edited.
    let fields: [(&str, &dyn Fn(&mut ClientPageCursor)); 3] = [
        ("person", &|cursor| cursor.person = Some("person/someone-else".into())),
        ("status", &|cursor| cursor.status = Some("ready".into())),
        ("native_only", &|cursor| cursor.native_only = !cursor.native_only),
    ];
    for (field, change) in fields {
        let edited = edit(&cursor, change);
        let decoded = decode_client_cursor(&edited).unwrap();
        let query = ClientListQuery {
            cursor: Some(edited),
            person: decoded.person,
            status: decoded.status,
            native_only: decoded.native_only,
            ..Default::default()
        };
        expired(work_page(&state, query).await, field);
    }
    // 5. The snapshot edited: its id, or its index alone.
    let edited = edit(&cursor, &|cursor| cursor.snapshot.id.push('x'));
    expired(continue_with(&state, edited, None).await, "snapshot id");
    let edited = edit(&cursor, &|cursor| cursor.snapshot.store_index += 1);
    expired(continue_with(&state, edited, None).await, "snapshot index");
    // 4. A forget since the first page.
    store.published_work_list().forget();
    expired(continue_with(&state, cursor, None).await, "forgotten");
}

/// Declare `seats` as agents.
fn agents(store: &Store, seats: &[&str]) {
    let source = seats
        .iter()
        .map(|seat| format!("agent \"{seat}\" {{ workspace \"/tmp\"; command \"true\" }}\n"))
        .collect::<String>();
    publish(store, &format!("version 2\n{source}"), &format!("publish-agents-{}", seats.join("-")));
}

fn claims_rebuilds(store: &Store) -> u64 {
    store.published_work_list().rebuilds().get("claims").copied().unwrap_or(0)
}

fn handoff(store: &Store, step: &str, from: &str, to: &str, key: &str) -> Result<crate::model::StepRunView, St3Error> {
    store.handoff_work(
        step,
        &crate::model::WorkHandoffRequest {
            actor: from.into(),
            incarnation: Some(format!("{from}-1")),
            to: to.into(),
            note: "Yours now.".into(),
            evidence: Vec::new(),
            idempotency_key: key.into(),
        },
    )
}

fn move_run(store: &Store, seat: &str, run: &str, placement: &str, anchor: Option<&str>, key: &str) -> smallclaims::claim::ClaimRecord {
    store
        .move_seat_queue_run(&crate::model::SeatQueueMoveRequest {
            agent: seat.into(),
            run: run.into(),
            placement: placement.into(),
            anchor: anchor.map(str::to_owned),
            reason: Some("the test's order".into()),
            actor: "person/operator".into(),
            idempotency_key: key.into(),
        })
        .unwrap()
}

#[test]
fn a_step_handed_off_before_it_was_ever_held_rereads_its_old_seat() {
    let _clock = Clock::at(start_time());
    let store = Store::open_memory("cedar").unwrap();
    let (a, b) = ("agent/garden/seat-a", "agent/garden/seat-b");
    agents(&store, &["garden/seat-a", "garden/seat-b"]);
    seat_mission(&store, "garden/tend-a", a);
    ready_run(&store, "garden/tend-a", "tend-a-1");
    let second = start(&store, "garden/tend-a", "tend-a-2");
    // The second run first in a's queue, so a may claim its step once it is ready.
    move_run(&store, a, &second.id, "top", None, "second-first");
    store.published_work_list().start();
    let (base, _) = fold_work_checked(&store);
    let (plant, seat) = step(&second, "plant");
    assert!(!base.rows.rows.contains_key(&plant), "never held at the base");
    assert!(base.rows.run_seats[&crate::store::work_list::run_key(&second.id)].contains(a), "a read the second run");
    let (rebuilds, lookups) = (claims_rebuilds(&store), store.seat_lookups());
    // In one fold window: ready, claimed by a, handed to b in place.
    store.set_step_state(&plant, "ready", None).unwrap();
    act(&store, &plant, &seat, "claim", "claim-second");
    handoff(&store, &plant, &seat, b, "handoff-second").unwrap();
    refresh_work_once(&store);
    assert_eq!(claims_rebuilds(&store), rebuilds, "a warm fold");
    assert!(store.seat_lookups() > lookups, "the second run was looked up");
    assert!(store.last_seats_read().contains(a), "a, no longer its assignee, was read again");
    // And a's order, without the second run, is what a fold from nothing reads.
    let (after, _) = fold_work_checked(&store);
    assert!(after.rows.seat_orders[a].iter().all(|run| *run != crate::store::work_list::run_key(&second.id)));
}

#[test]
fn a_seat_rereads_when_a_run_its_move_only_anchors_on_changes() {
    let _clock = Clock::at(start_time());
    let store = Store::open_memory("cedar").unwrap();
    let (ash, birch) = ("agent/garden/ash", "agent/garden/birch");
    agents(&store, &["garden/ash", "garden/birch"]);
    seat_mission(&store, "garden/tend-ash", ash);
    let r1 = ready_run(&store, "garden/tend-ash", "tend-ash-1");
    let r3 = ready_run(&store, "garden/tend-ash", "tend-ash-3");
    // Ash anchors a move on R1, then hands R1's step to birch in place: ash keeps no step in
    // R1, so R1 is in ash's queue's runs only through that move.
    move_run(&store, ash, &r3.id, "after", Some(&r1.id), "r3-after-r1");
    let (plant, seat) = step(&r1, "plant");
    act(&store, &plant, &seat, "claim", "claim-r1");
    handoff(&store, &plant, &seat, birch, "handoff-r1").unwrap();
    store.published_work_list().start();
    let (base, _) = fold_work_checked(&store);
    let r1_key = crate::store::work_list::run_key(&r1.id);
    assert!(base.rows.seat_runs[ash].contains(&r1_key), "ash's move names R1");
    assert!(base.rows.seat_orders.get(ash).is_none_or(|order| !order.contains(&r1_key)), "R1 is not live for ash");
    // No row-derived trigger: R1's row names no ash, as assignee, holder or offer.
    let names_ash = |rows: &WorkRows| {
        rows.rows.get(&plant).is_some_and(|row| {
            row.view.assigned_to.as_deref() == Some(ash)
                || row.view.claimant.as_deref() == Some(ash)
                || row.view.available_to.iter().any(|offered| offered == ash)
        })
    };
    assert!(!names_ash(&base.rows), "R1's row at the base names ash");
    let (rebuilds, lookups) = (claims_rebuilds(&store), store.seat_lookups());
    // In the window, a claim on R1's step by neither ash nor its holder.
    store.set_step_state(&plant, "blocked", Some("waiting for frost to pass")).unwrap();
    refresh_work_once(&store);
    let after = store.published_work().unwrap();
    assert!(!names_ash(&after.rows), "R1's row after names ash");
    assert_eq!(claims_rebuilds(&store), rebuilds, "a warm fold");
    assert!(store.seat_lookups() > lookups, "R1 was looked up");
    assert!(store.last_seats_read().contains(ash), "ash was read again, through R1 alone");
    fold_work_checked(&store);
}

#[test]
fn an_operator_repair_that_drops_a_move_folds_the_list_from_nothing() {
    let _clock = Clock::at(start_time());
    let store = Store::open_memory("cedar").unwrap();
    let ash = "agent/garden/ash";
    seat_mission(&store, "garden/tend-ash", ash);
    let r1 = ready_run(&store, "garden/tend-ash", "tend-ash-1");
    let r2 = ready_run(&store, "garden/tend-ash", "tend-ash-2");
    let moved = move_run(&store, ash, &r2.id, "top", None, "r2-first");
    // The move arrived by replication and was found invalid.
    let record = format!("record/{}", "ab".repeat(32));
    store.record_replica_for_test(&record, &moved.id, "invalid");
    store.published_work_list().start();
    let (base, _) = fold_work_checked(&store);
    assert_eq!(base.rows.seat_orders[ash].first(), Some(&crate::store::work_list::run_key(&r2.id)), "the move counts");
    let rebuilds = claims_rebuilds(&store);
    // The operator repairs the record through apply: no claim about ash, and no forget.
    publish(
        &store,
        &format!("version 2\nrepair \"{record}\" {{\n  replacement \"{}\"\n  reason \"replaced by the operator\"\n}}\n", moved.id),
        "repair-move",
    );
    refresh_work_once(&store);
    assert_eq!(claims_rebuilds(&store), rebuilds + 1, "the repair folded the list from nothing");
    let (after, _) = fold_work_checked(&store);
    assert_eq!(after.rows.seat_orders[ash].first(), Some(&crate::store::work_list::run_key(&r1.id)), "the repaired move no longer counts");
}

#[test]
fn one_repair_folds_the_list_from_nothing_once_while_a_lagging_frontier_advances() {
    let _clock = Clock::at(start_time());
    let store = Store::open_memory("cedar").unwrap();
    let ash = "agent/garden/ash";
    seat_mission(&store, "garden/tend-ash", ash);
    ready_run(&store, "garden/tend-ash", "tend-ash-1");
    let r2 = ready_run(&store, "garden/tend-ash", "tend-ash-2");
    let moved = move_run(&store, ash, &r2.id, "top", None, "r2-first");
    let record = format!("record/{}", "cd".repeat(32));
    store.record_replica_for_test(&record, &moved.id, "invalid");
    // Replication has projected only the first claim: the frontier lags far behind.
    store.set_projection_frontier_for_test(1);
    store.published_work_list().start();
    refresh_work_once(&store);
    let rebuilds = claims_rebuilds(&store);
    // The operator repairs the move through apply, above the lagging frontier.
    publish(
        &store,
        &format!("version 2\nrepair \"{record}\" {{\n  replacement \"{}\"\n  reason \"replaced by the operator\"\n}}\n", moved.id),
        "repair-move",
    );
    let repaired_at = store.index().unwrap();
    refresh_work_once(&store);
    assert_eq!(claims_rebuilds(&store), rebuilds + 1, "the repair folds the list from nothing");
    // The frontier advances three times, still below the repair: each fold reads the repair's
    // claim again, and none folds from nothing again.
    for (n, frontier) in [2_u64, 3, 4].into_iter().enumerate() {
        assert!(frontier < repaired_at);
        mission(&store, &format!("garden/later-{n}"));
        store.set_projection_frontier_for_test(frontier);
        refresh_work_once(&store);
        let published = store.published_work().unwrap();
        assert_eq!(published.cut, store.index().unwrap(), "fold {n} reached the current cut");
        assert_eq!(published.rows.frontier, frontier, "fold {n} saw the advanced frontier");
        assert_eq!(claims_rebuilds(&store), rebuilds + 1, "fold {n} stayed warm");
    }
    fold_work_checked(&store);
}

#[tokio::test]
async fn the_published_work_page_cache_reuses_a_page_evicts_by_contract_and_purges_on_forget() {
    let root = tempfile::tempdir().unwrap();
    let state = app_state(root.path());
    let store = &state.store;
    // More rows than any page below asks for, so every page has a next one and an entry.
    seat_mission(store, "garden/tend", "agent/garden/ash");
    for n in 0..40 {
        ready_run(store, "garden/tend", &format!("tend-{n}"));
    }
    let list = store.published_work_list();
    list.start();
    fold_work_checked(store);
    let instance = list.instance();
    // The same first page, asked again: the same cursor and expiry, and no new entry.
    let first = work_page(&state, work_query(None, 1)).await.unwrap();
    let held = published_work_pages_of(instance);
    let again = work_page(&state, work_query(None, 1)).await.unwrap();
    assert_eq!(again.page.next_cursor, first.page.next_cursor);
    assert_eq!(again.page.cursor_expires_at, first.page.cursor_expires_at, "the original expiry, not extended");
    assert_eq!(published_work_pages_of(instance), held, "a repeated page adds nothing");
    // A withdrawal leaves its continuation valid: same generation, rows at their own cut.
    list.withdraw();
    let next = work_page(&state, ClientListQuery { cursor: first.page.next_cursor.clone(), ..work_query(None, 1) })
        .await
        .unwrap();
    assert_eq!(next.items.len(), 1);
    assert!(list.serve_again(list.generation()));
    // Distinct contracts (limits) fill the cache: the oldest is evicted, and its cursor expires.
    let oldest = first.page.next_cursor.clone().unwrap();
    for limit in 2..=33 {
        work_page(&state, work_query(None, limit)).await.unwrap();
    }
    let evicted = work_page(&state, ClientListQuery { cursor: Some(oldest), ..work_query(None, 1) }).await.unwrap_err();
    assert_eq!((evicted.status, evicted.code.as_str()), (StatusCode::GONE, "page-cursor-expired"));
    // A forget purges this list's entries at the next access, and its cursors expire.
    let live = work_page(&state, work_query(None, 3)).await.unwrap().page.next_cursor.unwrap();
    assert!(published_work_pages_of(instance) > 0);
    list.forget();
    let forgotten = work_page(&state, ClientListQuery { cursor: Some(live), ..work_query(None, 3) }).await.unwrap_err();
    assert_eq!(forgotten.code, "page-cursor-expired");
    assert_eq!(published_work_pages_of(instance), 0, "the replaced generation's entries are gone");
}

/// Wait, bounded in the test's own time, until the refresher has published the current cut.
async fn wait_published(store: &Store) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !store.published_work().is_some_and(|published| published.cut == store.index().unwrap()) {
        assert!(tokio::time::Instant::now() < deadline, "the refresher never published the current cut");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_fresh_read_cuts_the_refresher_pause_short_and_commits_keep_it() {
    let root = tempfile::tempdir().unwrap();
    let state = app_state(root.path());
    let store = &state.store;
    seat_mission(store, "garden/tend", "agent/garden/ash");
    ready_run(store, "garden/tend", "tend-1");
    start_published_work(&state);
    wait_published(store).await;
    // A commit just after a fold waits out the pause before the next fold.
    let committed = tokio::time::Instant::now();
    ready_run(store, "garden/tend", "tend-2");
    wait_published(store).await;
    assert!(committed.elapsed() >= REFRESH_PAUSE - Duration::from_millis(100), "{:?}", committed.elapsed());
    // A fresh read just after a fold asks for the next one at once.
    ready_run(store, "garden/tend", "tend-3");
    let asked = tokio::time::Instant::now();
    let page = work_page(&state, ClientListQuery { fresh: true, ..work_query(None, 200) }).await.unwrap();
    assert_eq!(page.items.len(), 3);
    assert!(asked.elapsed() < REFRESH_PAUSE / 2, "{:?}", asked.elapsed());
    // Two fresh reads at once: one request serves both.
    ready_run(store, "garden/tend", "tend-4");
    let fresh = ClientListQuery { fresh: true, ..work_query(None, 200) };
    let (first, second) = tokio::join!(work_page(&state, fresh.clone()), work_page(&state, fresh));
    assert_eq!((first.unwrap().items.len(), second.unwrap().items.len()), (4, 4));
}

#[tokio::test(start_paused = true)]
async fn sustained_fresh_requests_fold_no_faster_than_the_fresh_floor() {
    let root = tempfile::tempdir().unwrap();
    let state = app_state(root.path());
    let store = &state.store;
    seat_mission(store, "garden/tend", "agent/garden/ash");
    ready_run(store, "garden/tend", "tend-1");
    start_published_work(&state);
    wait_published(store).await;
    let list = store.published_work_list();
    let (before, began) = (list.folds(), tokio::time::Instant::now());
    // Fifty fresh requests a second for two seconds.
    for _ in 0..100 {
        list.request_fresh();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (folds, span) = (list.folds() - before, began.elapsed());
    let most = (span.as_millis() / FRESH_PAUSE.as_millis()) as usize + 2;
    assert!(folds <= most, "{folds} folds in {span:?}, at most {most}");
    assert!(folds >= 2, "fresh requests did cut the pause: {folds} folds in {span:?}");
}

/// Bring every cached page of `list` to expire `ms` from now, as if its TTL were that short.
/// Returns the new expiry.
fn expire_published_work_pages_in(list: u64, ms: u128) -> u128 {
    let expires_at = client_now_ms() + ms;
    let mut pages = published_work_pages().lock().unwrap();
    for page in pages.iter_mut().filter(|page| page.id.list == list) {
        page.expires_at_unix_ms = expires_at;
    }
    expires_at
}

#[tokio::test]
async fn a_cached_work_page_keeps_its_expiry_on_reuse_and_is_released_once_it_passes() {
    let root = tempfile::tempdir().unwrap();
    let state = app_state(root.path());
    let store = &state.store;
    seat_mission(store, "garden/tend", "agent/garden/ash");
    for n in 0..3 {
        ready_run(store, "garden/tend", &format!("tend-{n}"));
    }
    let list = store.published_work_list();
    list.start();
    fold_work_checked(store);
    let instance = list.instance();
    work_page(&state, work_query(None, 1)).await.unwrap();
    assert_eq!(published_work_pages_of(instance), 1);
    // Shortly before its expiry, the same page asked again reuses the entry and its expiry.
    let expires_at = expire_published_work_pages_in(instance, 200);
    let reused = work_page(&state, work_query(None, 1)).await.unwrap();
    assert_eq!(reused.page.cursor_expires_at, Some(client_timestamp(expires_at)), "not extended");
    assert_eq!(published_work_pages_of(instance), 1);
    let cursor = reused.page.next_cursor.unwrap();
    // Past it, the continuation expires: its own expiry is checked before the cache is.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let expired = work_page(&state, ClientListQuery { cursor: Some(cursor), ..work_query(None, 1) }).await.unwrap_err();
    assert_eq!((expired.status, expired.code.as_str()), (StatusCode::GONE, "page-cursor-expired"));
    // The next cache access releases the expired entry, and that first page makes a new one,
    // with a new expiry: one entry, not two.
    let renewed = work_page(&state, work_query(None, 1)).await.unwrap();
    assert_eq!(published_work_pages_of(instance), 1, "the expired entry was released");
    assert_ne!(renewed.page.cursor_expires_at, Some(client_timestamp(expires_at)));
    // A continuation outlives a withdrawal and the refresher's end until its expiry, while new
    // first pages refuse; only a forget expires it.
    let cursor = renewed.page.next_cursor.unwrap();
    list.end();
    assert_eq!(work_page(&state, work_query(None, 1)).await.unwrap_err().code, WORK_LIST_ENDED);
    let next = work_page(&state, ClientListQuery { cursor: Some(cursor), ..work_query(None, 1) }).await.unwrap();
    assert_eq!(next.items.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_fresh_read_the_newest_publication_covers_asks_for_no_fold() {
    let root = tempfile::tempdir().unwrap();
    let state = app_state(root.path());
    let store = &state.store;
    seat_mission(store, "garden/tend", "agent/garden/ash");
    ready_run(store, "garden/tend", "tend-1");
    start_published_work(&state);
    wait_published(store).await;
    let list = store.published_work_list();
    // Let any pause after the last fold run out, so a stray request would show as a fold.
    tokio::time::sleep(REFRESH_PAUSE * 2).await;
    let folds = list.folds();
    let asked = tokio::time::Instant::now();
    let page = work_page(&state, ClientListQuery { fresh: true, ..work_query(None, 200) }).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(asked.elapsed() < FRESH_PAUSE, "answered at once: {:?}", asked.elapsed());
    tokio::time::sleep(REFRESH_PAUSE * 2).await;
    assert_eq!(list.folds(), folds, "a covered fresh read asks for no fold");
}
