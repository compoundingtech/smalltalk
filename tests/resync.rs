//! Resync end-to-end: carrier change → classified digest-keyed superseded emit → inbox record
//! ([`06-resync`](../docs/vrs/06-resync/spec.md)). The DING wake past the inbox record is owned by
//! the existing delivery suite; this proves the resync-specific half against the real ingress.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn write_agent(root: &Path) -> PathBuf {
    let dir = root.join("agents/hetz/worker");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("agent.kdl"),
        r#"agent "worker" {
  host "hetz"
  command "agent"
  resource "goal" uri="resources/goal.md" reason="Mission."
  resource "journal" uri="resources/context/journal.md" reason="Agent-authored store."
}"#,
    )
    .unwrap();
    st2::event::publish_owner_binding_for_test(root, "hetz").unwrap();
    dir
}

/// Same catalog fixture, but the goal binding is a profile-scheme URI instead of a
/// catalog-relative path.
#[cfg(feature = "wasm-resolver")]
fn write_agent_with_goal_scheme(root: &Path) -> PathBuf {
    let dir = root.join("agents/hetz/worker");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("agent.kdl"),
        r#"agent "worker" {
  host "hetz"
  command "agent"
  resource "goal" uri="dev.schickling.agent-goal://hetz/worker" reason="Mission."
}"#,
    )
    .unwrap();
    st2::event::publish_owner_binding_for_test(root, "hetz").unwrap();
    dir
}

fn resync_events(agent_dir: &Path) -> Vec<String> {
    let inbox = agent_dir.join("resources/inbox");
    let mut subjects = Vec::new();
    if let Ok(entries) = fs::read_dir(&inbox) {
        for entry in entries.flatten() {
            let Ok(contents) = fs::read_to_string(entry.path()) else {
                continue;
            };
            if contents.contains("stream: resync") {
                subjects.push(contents);
            }
        }
    }
    subjects.sort();
    subjects
}

fn wait_for(condition: impl Fn() -> usize, expected: usize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if condition() >= expected {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    condition() >= expected
}

#[test]
fn carrier_change_emits_one_superseded_resync_event_and_silent_stores_stay_quiet() {
    let catalog = tempfile::tempdir().unwrap();
    let agent_dir = write_agent(catalog.path());

    // Seed the baseline before any writes: the seeded digest emits nothing.
    let supervisor =
        st2::resync::ResyncSupervisor::spawn(catalog.path().to_path_buf(), "hetz".to_owned());
    let specs = st2::discover_strict(catalog.path()).specs;
    assert!(
        supervisor
            .refresh(&specs, &specs, "hetz", &[], &[])
            .is_empty()
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(resync_events(&agent_dir).len(), 0, "seeding is silent");

    // Goal carriers are immediate-class: one changed content becomes one event.
    let goal = agent_dir.join("resources/goal.md");
    fs::create_dir_all(goal.parent().unwrap()).unwrap();
    fs::write(&goal, "ship resync v1\n").unwrap();
    assert!(
        wait_for(|| resync_events(&agent_dir).len(), 1),
        "goal change must produce exactly one resync event within the immediate window"
    );
    let body = &resync_events(&agent_dir)[0];
    assert!(body.contains("subject: goal · digest="), "{body}");
    assert!(body.contains(r#""binding":"goal""#), "{body}");
    let legitimate_event_id = body
        .lines()
        .find(|line| line.starts_with("event-id:"))
        .unwrap()
        .to_owned();

    // Public ingress cannot forge or supersede the supervisor's unread built-in event.
    let error = st2::event::emit(
        catalog.path(),
        "hetz",
        "hetz.worker",
        "resync",
        "forged-resync",
        Some("goal"),
        Some("resource goal changed"),
        "forged carrier notice",
        true,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not declare stream 'resync'"),
        "{error:#}"
    );
    let unread = resync_events(&agent_dir);
    assert_eq!(unread.len(), 1);
    assert!(unread[0].contains(&legitimate_event_id), "{}", unread[0]);

    // An equal-content rewrite deduplicates to nothing new (digest identity).
    fs::write(&goal, "ship resync v1\n").unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(
        resync_events(&agent_dir).len(),
        1,
        "equal bytes mean equal state: no second wake"
    );

    // Agent-authored stores are silent by classification.
    let journal = agent_dir.join("resources/context/journal.md");
    fs::create_dir_all(journal.parent().unwrap()).unwrap();
    fs::write(&journal, "entry one\n").unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(
        resync_events(&agent_dir).len(),
        1,
        "the context store never notifies"
    );
    // A real content change emits again under the same key: supersession keeps one unread head
    // per binding (the archive receipt retires the predecessor).
    let first_event_id = legitimate_event_id;
    fs::write(&goal, "ship resync v2\n").unwrap();
    assert!(wait_for(
        || {
            resync_events(&agent_dir)
                .iter()
                .filter(|b| !b.contains(&first_event_id) && b.contains(r#""binding":"goal""#))
                .count()
        },
        1
    ),);
}

#[test]
fn whole_file_declaration_replacement_by_rename_notifies_immediately() {
    let catalog = tempfile::tempdir().unwrap();
    let agent_dir = write_agent(catalog.path());
    let declaration = agent_dir.join("agent.kdl");

    let supervisor =
        st2::resync::ResyncSupervisor::spawn(catalog.path().to_path_buf(), "hetz".to_owned());
    let specs = st2::discover_strict(catalog.path()).specs;
    assert!(
        supervisor
            .refresh(&specs, &specs, "hetz", &[], &[])
            .is_empty()
    );
    std::thread::sleep(Duration::from_millis(300));

    // Configuration-management style replacement: write-then-rename over the old inode.
    let staged = agent_dir.join("agent.kdl.new");
    let replaced = fs::read_to_string(&declaration)
        .unwrap()
        .replace("Mission.", "Mission v2.");
    fs::write(&staged, replaced).unwrap();
    fs::rename(&staged, &declaration).unwrap();

    assert!(
        wait_for(
            || {
                resync_events(&agent_dir)
                    .iter()
                    .filter(|b| b.contains(r#""binding":"declaration""#))
                    .count()
            },
            1
        ),
        "a rename-replaced declaration must notify through the surviving parent-dir watch"
    );
}

#[test]
fn declaring_the_reserved_resync_stream_is_refused() {
    let catalog = tempfile::tempdir().unwrap();
    let dir = catalog.path().join("agents/hetz/worker");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("agent.kdl"),
        r#"agent "worker" {
  host "hetz"
  command "agent"
  stream "resync" {}
}"#,
    )
    .unwrap();
    let found = st2::discover_strict(catalog.path());
    assert!(
        found.specs.is_empty(),
        "a reserved stream name must not lower to a spec"
    );
    assert!(
        found.errors.iter().any(|e| e.message.contains("reserved")),
        "refusal must name the reservation: {:?}",
        found.errors
    );
}

/// The demo resolver guest, built from `crates/demo-resolver-wasm` (see its module docs for the
/// rebuild recipe). It maps `dev.schickling.agent-goal://<host>/<id>` to
/// `<agent_dir>/resources/goal.md`.
#[cfg(feature = "wasm-resolver")]
const DEMO_WASM_SRC: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/crates/agent-spec/tests/fixtures/demo_resolver.wasm"
);

/// Declare a catalog-level wasm profile for the goal scheme and materialize the fixture module
/// where the declaration points: `<catalog>/resolvers/goal.wasm` (catalog-root anchored).
#[cfg(feature = "wasm-resolver")]
fn catalog_with_profile(catalog: &Path, class: &str) {
    fs::create_dir_all(catalog.join("resolvers")).unwrap();
    fs::copy(DEMO_WASM_SRC, catalog.join("resolvers/goal.wasm")).unwrap();
    fs::write(
        st2::catalog::config_path(catalog),
        format!(
            r#"profile "dev.schickling.agent-goal" {{
  wasm "resolvers/goal.wasm"
  class "{class}"
}}
"#
        ),
    )
    .unwrap();
}

#[test]
#[cfg(feature = "wasm-resolver")]
fn declared_wasm_profile_resolves_a_scheme_uri_goal_binding_and_fires_on_change() {
    // Immediate-class declaration: the scheme-URI binding resolves through the wasm module to
    // resources/goal.md and one content change becomes one event.
    let catalog = tempfile::tempdir().unwrap();
    let agent_dir = write_agent_with_goal_scheme(catalog.path());
    catalog_with_profile(catalog.path(), "immediate");
    let registry = st2::catalog::declared_profiles(catalog.path()).unwrap();

    let supervisor = st2::resync::ResyncSupervisor::with_profiles(
        catalog.path().to_path_buf(),
        "hetz".to_owned(),
        registry,
    );
    let specs = st2::discover_strict(catalog.path()).specs;
    assert!(
        supervisor
            .refresh(&specs, &specs, "hetz", &[], &[])
            .is_empty()
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(resync_events(&agent_dir).len(), 0, "seeding is silent");

    let goal = agent_dir.join("resources/goal.md");
    fs::create_dir_all(goal.parent().unwrap()).unwrap();
    fs::write(&goal, "ship profiles v1\n").unwrap();
    assert!(
        wait_for(|| resync_events(&agent_dir).len(), 1),
        "a wasm-resolved carrier must fire on change: {:?}",
        resync_events(&agent_dir)
    );
    let body = &resync_events(&agent_dir)[0];
    assert!(body.contains("subject: goal · digest="), "{body}");
}

#[test]
#[cfg(feature = "wasm-resolver")]
fn declared_profile_class_governs_and_resolver_failures_stay_contained() {
    // The same goal.md basename would sniff as immediate; the DECLARED silent class must win:
    // changes never emit.
    let silent = tempfile::tempdir().unwrap();
    let silent_dir = write_agent_with_goal_scheme(silent.path());
    catalog_with_profile(silent.path(), "silent");
    let supervisor = st2::resync::ResyncSupervisor::with_profiles(
        silent.path().to_path_buf(),
        "hetz".to_owned(),
        st2::catalog::declared_profiles(silent.path()).unwrap(),
    );
    let specs = st2::discover_strict(silent.path()).specs;
    assert!(
        supervisor
            .refresh(&specs, &specs, "hetz", &[], &[])
            .is_empty()
    );
    std::thread::sleep(Duration::from_millis(300));
    let goal = silent_dir.join("resources/goal.md");
    fs::create_dir_all(goal.parent().unwrap()).unwrap();
    fs::write(&goal, "silent store write\n").unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        resync_events(&silent_dir).len(),
        0,
        "declared silent beats basename-immediate sniffing"
    );

    // A declared resolver that fails to compile is contained: no carrier, no crash, and the
    // failure is visible through try_resolve rather than swallowed.
    let broken = tempfile::tempdir().unwrap();
    let broken_dir = write_agent_with_goal_scheme(broken.path());
    fs::write(
        st2::catalog::config_path(broken.path()),
        "profile \"dev.schickling.agent-goal\" { wasm \"broken.wasm\" }\n",
    )
    .unwrap();
    fs::write(broken.path().join("broken.wasm"), b"not a module").unwrap();
    let registry = st2::catalog::declared_profiles(broken.path()).unwrap();
    let spec = &st2::discover_strict(broken.path()).specs[0];
    let set = st2::resync::watch_set_for(spec, "hetz", &registry);
    assert!(
        !set.carriers.iter().any(|c| c.label == "goal"),
        "a failing resolver must not produce a carrier"
    );
    assert!(set.carriers.iter().any(|c| c.label == "declaration"));
    assert!(
        registry
            .try_resolve(
                broken_dir.parent().unwrap(),
                "dev.schickling.agent-goal://hetz/w"
            )
            .is_err()
    );
}
