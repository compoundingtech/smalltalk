//! `notify-chain`: a binding through a composing profile also subscribes to the same-scheme
//! carriers its `supervisor` ancestors declare.
//!
//! Resync notifies a carrier's OWNER. For a profile whose layers compose along the supervisor
//! edge, that leaves every descendant's effective view dependent on carriers no one tells it
//! about. These tests pin both directions: with the flag an ancestor change reaches the
//! descendant, and without it the supervisor edge alone carries nothing.

#![cfg(feature = "wasm-resolver")]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DEMO_WASM_SRC: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/crates/agent-spec/tests/fixtures/demo_resolver.wasm"
);

/// The demo resolver denotes `<agent_dir>/resources/goal.md` for any subject of its scheme, so
/// each agent's own declaration resolves to that agent's own carrier — exactly the per-layer
/// shape a composing profile has.
fn catalog_with_profile(catalog: &Path, notify_chain: bool) {
    fs::create_dir_all(catalog.join("resolvers")).unwrap();
    fs::copy(DEMO_WASM_SRC, catalog.join("resolvers/goal.wasm")).unwrap();
    let chain = if notify_chain {
        "  notify-chain #true\n"
    } else {
        ""
    };
    fs::write(
        st2::catalog::config_path(catalog),
        format!(
            "profile \"dev.schickling.agent-goal\" {{\n  wasm \"resolvers/goal.wasm\"\n  class \"immediate\"\n{chain}}}\n"
        ),
    )
    .unwrap();
}

/// One seat declaring the profiled binding, optionally supervised and optionally retired.
fn write_agent(
    root: &Path,
    identity: &str,
    supervisor: Option<&str>,
    retirement: Option<&str>,
) -> PathBuf {
    let dir = root.join(format!("agents/hetz/{identity}"));
    fs::create_dir_all(&dir).unwrap();
    let supervisor = supervisor
        .map(|s| format!("  supervisor \"{s}\"\n"))
        .unwrap_or_default();
    let retirement = retirement.map(|r| format!("  {r}\n")).unwrap_or_default();
    fs::write(
        dir.join("agent.kdl"),
        format!(
            "agent \"{identity}\" {{\n  host \"hetz\"\n{supervisor}{retirement}  command \"agent\"\n  \
             resource \"goal\" uri=\"dev.schickling.agent-goal://hetz/{identity}\" reason=\"Layer.\"\n}}\n"
        ),
    )
    .unwrap();
    dir
}

fn resync_bodies(agent_dir: &Path) -> Vec<String> {
    let mut bodies = Vec::new();
    if let Ok(entries) = fs::read_dir(agent_dir.join("resources/inbox")) {
        for entry in entries.flatten() {
            if let Ok(contents) = fs::read_to_string(entry.path())
                && contents.contains("stream: resync")
            {
                bodies.push(contents);
            }
        }
    }
    bodies.sort();
    bodies
}

fn wait_for(condition: impl Fn() -> usize, expected: usize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if condition() >= expected {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    condition() >= expected
}

fn spawn(catalog: &Path) -> st2::resync::ResyncSupervisor {
    st2::event::publish_owner_binding_for_test(catalog, "hetz").unwrap();
    let registry = st2::catalog::declared_profiles(catalog).unwrap();
    let supervisor = st2::resync::ResyncSupervisor::with_profiles(
        catalog.to_path_buf(),
        "hetz".to_owned(),
        registry,
    );
    let found = st2::discover_strict(catalog);
    assert!(
        found.errors.is_empty(),
        "clean discovery: {:?}",
        found.errors
    );
    let specs = found.specs;
    let live_subscription_specs = specs
        .iter()
        .filter(|spec| spec.desired_state.is_running())
        .cloned()
        .collect::<Vec<_>>();
    let diagnostics = supervisor.refresh(&specs, &live_subscription_specs, "hetz", &[], &[]);
    assert!(diagnostics.is_empty(), "clean refresh: {diagnostics:?}");
    std::thread::sleep(Duration::from_millis(300));
    supervisor
}

fn write_layer(agent_dir: &Path, text: &str) {
    let carrier = agent_dir.join("resources/goal.md");
    fs::create_dir_all(carrier.parent().unwrap()).unwrap();
    fs::write(&carrier, text).unwrap();
}

/// The flag on: changing an ancestor's layer reaches the descendant, keyed by the owner so two
/// ancestors cannot collapse onto one supersession key.
#[test]
fn an_ancestor_layer_change_reaches_a_descendant_that_opted_in() {
    let catalog = tempfile::tempdir().unwrap();
    catalog_with_profile(catalog.path(), true);
    let root = write_agent(catalog.path(), "root", None, None);
    let lead = write_agent(catalog.path(), "lead", Some("hetz.root"), None);
    let worker = write_agent(catalog.path(), "worker", Some("hetz.lead"), None);
    let _supervisor = spawn(catalog.path());

    assert_eq!(resync_bodies(&worker).len(), 0, "seeding is silent");

    // Only the direct ancestor's layer is written; the worker's own carrier is untouched.
    write_layer(&lead, "prioritize the migration\n");
    assert!(
        wait_for(|| resync_bodies(&lead).len(), 1),
        "the owning ancestor is notified"
    );
    assert!(
        wait_for(|| resync_bodies(&worker).len(), 1),
        "the descendant that opted in must be notified: {:?}",
        resync_bodies(&worker)
    );
    let body = &resync_bodies(&worker)[0];
    assert!(
        body.contains("key: goal@hetz.lead"),
        "the descendant's event is keyed by the owning ancestor: {body}"
    );

    // A transitive ancestor is on the chain too, and lands under its own key.
    write_layer(&root, "standing safety constraint\n");
    assert!(
        wait_for(|| resync_bodies(&worker).len(), 2),
        "a transitive ancestor's layer also reaches the descendant: {:?}",
        resync_bodies(&worker)
    );
    assert!(
        resync_bodies(&worker)
            .iter()
            .any(|body| body.contains("key: goal@hetz.root")),
        "each ancestor keeps its own supersession key: {:?}",
        resync_bodies(&worker)
    );
}

/// CONTROL. Identical catalog and identical supervisor edges, flag absent: the descendant hears
/// nothing. If this ever fails alongside a passing opt-in test, the opt-in test proves nothing.
#[test]
fn without_notify_chain_a_supervisor_edge_carries_no_fan_out() {
    let catalog = tempfile::tempdir().unwrap();
    catalog_with_profile(catalog.path(), false);
    let _root = write_agent(catalog.path(), "root", None, None);
    let lead = write_agent(catalog.path(), "lead", Some("hetz.root"), None);
    let worker = write_agent(catalog.path(), "worker", Some("hetz.lead"), None);
    let _supervisor = spawn(catalog.path());

    write_layer(&lead, "prioritize the migration\n");
    assert!(
        wait_for(|| resync_bodies(&lead).len(), 1),
        "the owning ancestor is still notified"
    );
    // Well past the immediate window, and past the coalesced window too.
    std::thread::sleep(Duration::from_secs(7));
    assert_eq!(
        resync_bodies(&worker).len(),
        0,
        "supervisor edges alone must carry no fan-out: {:?}",
        resync_bodies(&worker)
    );
}

/// A retired ancestor contributes no layer, and the walk continues through it to the root — in
/// both declaration spellings.
#[test]
fn a_retired_ancestor_is_skipped_and_the_walk_continues_past_it() {
    for retirement in [
        "retired #true",
        "desired-state \"retired\" reason=\"work complete\"",
    ] {
        let catalog = tempfile::tempdir().unwrap();
        catalog_with_profile(catalog.path(), true);
        let root = write_agent(catalog.path(), "root", None, None);
        let lead = write_agent(catalog.path(), "lead", Some("hetz.root"), Some(retirement));
        let worker = write_agent(catalog.path(), "worker", Some("hetz.lead"), None);
        let _supervisor = spawn(catalog.path());

        // The live grandparent beyond the retired ancestor still reaches the descendant.
        write_layer(&root, "standing safety constraint\n");
        assert!(
            wait_for(|| resync_bodies(&worker).len(), 1),
            "the walk must continue through a retired ancestor ({retirement}): {:?}",
            resync_bodies(&worker)
        );
        assert!(
            resync_bodies(&worker)[0].contains("key: goal@hetz.root"),
            "the surviving layer is the root's ({retirement})"
        );

        // The retired ancestor's own layer contributes nothing, even when it changes.
        write_layer(&lead, "directive from a retired seat\n");
        std::thread::sleep(Duration::from_secs(3));
        assert!(
            !resync_bodies(&worker)
                .iter()
                .any(|body| body.contains("key: goal@hetz.lead")),
            "a retired ancestor's layer must be excluded ({retirement}): {:?}",
            resync_bodies(&worker)
        );
    }
}

/// A suspended ancestor remains part of the composed desired-state chain: its layer and the
/// ancestors beyond it reach a live descendant, while the suspended seat owns no subscription.
#[test]
fn a_suspended_ancestor_contributes_its_layer_without_becoming_a_subscription() {
    let catalog = tempfile::tempdir().unwrap();
    catalog_with_profile(catalog.path(), true);
    let root = write_agent(catalog.path(), "root", None, None);
    let lead = write_agent(
        catalog.path(),
        "lead",
        Some("hetz.root"),
        Some("desired-state \"suspended\" reason=\"waiting for capacity\""),
    );
    let worker = write_agent(catalog.path(), "worker", Some("hetz.lead"), None);
    write_layer(&lead, "before\n");
    let specs = st2::discover_strict(catalog.path()).specs;
    let worker_spec = specs.iter().find(|spec| spec.identity == "worker").unwrap();
    let set = st2::resync::watch_set_for_in_catalog(
        worker_spec,
        &specs,
        "hetz",
        &st2::catalog::declared_profiles(catalog.path()).unwrap(),
    );
    assert!(
        set.carriers
            .iter()
            .any(|carrier| carrier.label == "goal@hetz.lead"),
        "a suspended ancestor remains a layer in the live descendant's watch set"
    );
    assert!(
        set.carriers
            .iter()
            .any(|carrier| carrier.label == "goal@hetz.root"),
        "traversal continues through the suspended ancestor"
    );
    let _supervisor = spawn(catalog.path());

    write_layer(&lead, "directive retained by a suspended seat\n");
    assert!(
        wait_for(|| resync_bodies(&worker).len(), 1),
        "the suspended ancestor's layer remains in the live descendant's composed view: {:?}",
        resync_bodies(&worker)
    );
    assert!(
        resync_bodies(&worker)[0].contains("key: goal@hetz.lead"),
        "the suspended layer keeps its owner-qualified key"
    );
    assert!(
        resync_bodies(&lead).is_empty(),
        "the suspended seat itself must own no active subscription"
    );

    write_layer(&root, "standing safety constraint\n");
    assert!(
        wait_for(|| resync_bodies(&worker).len(), 2),
        "chain traversal must continue through the suspended middle: {:?}",
        resync_bodies(&worker)
    );
    assert!(
        resync_bodies(&worker)
            .iter()
            .any(|body| body.contains("key: goal@hetz.root")),
        "the root layer beyond the suspended middle must remain subscribed"
    );
    assert!(
        resync_bodies(&lead).is_empty(),
        "ancestor fan-out must not reactivate the suspended seat"
    );
}

#[test]
fn live_install_reports_an_unwalkable_notify_chain() {
    let catalog = tempfile::tempdir().unwrap();
    catalog_with_profile(catalog.path(), true);
    write_agent(catalog.path(), "worker", Some("hetz.missing"), None);
    st2::event::publish_owner_binding_for_test(catalog.path(), "hetz").unwrap();
    let specs = st2::discover_strict(catalog.path()).specs;
    let registry = st2::catalog::declared_profiles(catalog.path()).unwrap();
    let supervisor = st2::resync::ResyncSupervisor::with_profiles(
        catalog.path().to_path_buf(),
        "hetz".to_owned(),
        registry,
    );

    let diagnostics = supervisor.install_live(&specs[0], &specs, "hetz");
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("supervisor chain is unwalkable")
                && diagnostic.contains("MissingSupervisor")
        }),
        "a chain-aware live install must not silently degrade to a one-spec view: {diagnostics:?}"
    );
}
