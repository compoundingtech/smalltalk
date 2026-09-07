#![cfg(unix)]
//! `st2 up` archives retired, quiescent seats once their retirement outlives `archive-after`.
//!
//! The grace period is measured from the supervisor's first observation of the retirement, which it
//! records in `.st2/retired-observed.json` — st2 keeps no timestamp for a desired-state edit. These
//! tests seed that ledger to place a retirement in the past instead of waiting out a real clock.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const LEDGER: &str = ".st2/retired-observed.json";
const RETIRED: &str = "desired-state \"retired\" reason=\"Migration finished\"";
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

fn write(root: &Path, relative: &str, body: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// A `pty list` shim whose JSON is the only runtime evidence the archive gate consults.
fn pty_shim(bin: &Path, sessions: &str) {
    fs::create_dir_all(bin).unwrap();
    let path = bin.join("pty");
    fs::write(
        &path,
        format!("#!/bin/sh\nif [ \"$1\" = list ]; then printf '{sessions}\\n'; fi\n"),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A catalog with an empty session registry and a `PATH` holding only the `pty` shim.
fn fixture(temporary: &tempfile::TempDir) -> (PathBuf, PathBuf) {
    let catalog = temporary.path().join("catalog");
    fs::create_dir_all(&catalog).unwrap();
    let bin = temporary.path().join("bin");
    pty_shim(&bin, "[]");
    (catalog, bin)
}

fn seat(root: &Path, identity: &str, lifecycle: &str) {
    write(
        root,
        &format!("agents/h/{identity}/agent.kdl"),
        &format!("agent \"{identity}\" {{\n  host \"h\"\n  {lifecycle}\n  command \"true\"\n}}\n"),
    );
    write(
        root,
        &format!("agents/h/{identity}/resources/goal.md"),
        "# goal\n\nfinish the migration\n",
    );
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Place each identity's observed retirement `age_ms` in the past.
fn seed_ledger(root: &Path, observed: &[(&str, u64)]) {
    let rows = observed
        .iter()
        .map(|(identity, age_ms)| format!("      \"{identity}\": {}", now_ms() - age_ms))
        .collect::<Vec<_>>()
        .join(",\n");
    write(
        root,
        LEDGER,
        &format!(
            "{{\n  \"schema\": \"st2.catalog-retired-observed.v1\",\n  \"hosts\": {{\n    \"h\": {{\n{rows}\n    }}\n  }}\n}}\n"
        ),
    );
}

fn ledger(root: &Path) -> Option<serde_json::Value> {
    let body = fs::read(root.join(LEDGER)).ok()?;
    Some(serde_json::from_slice(&body).unwrap())
}

fn up_once(root: &Path, bin: &Path) -> Output {
    let home = root.parent().unwrap().join("home");
    fs::create_dir_all(&home).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["up", "--catalog", root.to_str().unwrap()])
        .args(["--host", "h", "--once"])
        .env("PATH", bin)
        .env("HOME", &home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("PTY_ROOT", home.join("pty"))
        .env_remove("CATALOG")
        .env_remove("ST_ROOT")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "st2 up --once failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_pass_records_a_fresh_retirement_and_archives_nothing() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    seat(&catalog, "gone", RETIRED);

    let output = up_once(&catalog, &bin);

    assert!(
        catalog.join("agents/h/gone").is_dir(),
        "a retirement inside its grace period must stay in the live catalog:\n{}",
        stdout(&output)
    );
    assert!(!stdout(&output).contains("archived"), "{}", stdout(&output));
    let ledger = ledger(&catalog).expect("the pass must record when it first saw the retirement");
    assert_eq!(ledger["schema"], "st2.catalog-retired-observed.v1");
    let observed = ledger["hosts"]["h"]["gone"].as_u64().unwrap();
    assert!(
        now_ms().saturating_sub(observed) < 5 * 60 * 1000,
        "the observation must carry this pass's clock, got {observed}"
    );
}

#[test]
fn a_retirement_older_than_the_grace_period_is_archived_with_a_tombstone() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    seat(&catalog, "gone", RETIRED);
    let spec = fs::read(catalog.join("agents/h/gone/agent.kdl")).unwrap();
    let goal = fs::read(catalog.join("agents/h/gone/resources/goal.md")).unwrap();
    seed_ledger(&catalog, &[("gone", 8 * DAY_MS)]);

    let output = up_once(&catalog, &bin);

    assert!(
        stdout(&output).contains("archived (1): h.gone"),
        "the pass must report the seat it archived:\n{}",
        stdout(&output)
    );
    assert!(
        !catalog.join("agents/h/gone").exists(),
        "the seat must leave the live declaration plane"
    );
    assert_eq!(
        fs::read(catalog.join(".st2/archive/h/gone/agent.kdl")).unwrap(),
        spec,
        "the declaration moves byte-identically"
    );
    assert_eq!(
        fs::read(catalog.join(".st2/archive/h/gone/resources/goal.md")).unwrap(),
        goal,
        "the whole identity directory moves, not just the spec"
    );
    let tombstone: serde_json::Value = serde_json::from_slice(
        &fs::read(catalog.join(".st2/archive/h/gone.tombstone.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(tombstone["schema"], "st2.catalog-archive-tombstone.v1");
    assert_eq!(tombstone["id"], "h.gone");
    assert_eq!(tombstone["reason"], "Migration finished");
    assert!(tombstone["archivedAt"].as_u64().unwrap() > 0);
}

#[test]
fn archive_after_zero_disables_the_pass_entirely() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    write(
        &catalog,
        "catalog.kdl",
        "catalog {\n  archive-after \"0\"\n}\n",
    );
    seat(&catalog, "gone", RETIRED);

    let output = up_once(&catalog, &bin);

    assert!(
        catalog.join("agents/h/gone").is_dir(),
        "`archive-after \"0\"` is the operator's off switch:\n{}",
        stdout(&output)
    );
    assert!(
        ledger(&catalog).is_none(),
        "a disabled pass must not even record observations"
    );
}

#[test]
fn a_declared_grace_period_overrides_the_seven_day_default() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    write(
        &catalog,
        "catalog.kdl",
        "catalog {\n  archive-after \"12h\"\n}\n",
    );
    seat(&catalog, "gone", RETIRED);
    // Younger than the 7-day default, older than the declared 12 hours.
    seed_ledger(&catalog, &[("gone", DAY_MS)]);

    up_once(&catalog, &bin);

    assert!(
        !catalog.join("agents/h/gone").exists(),
        "the declared grace period decides, not the default"
    );
}

#[test]
fn a_running_or_suspended_seat_is_never_archived() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    seat(&catalog, "live", "desired-state \"running\"");
    seat(
        &catalog,
        "held",
        "desired-state \"suspended\" reason=\"paused\"",
    );
    // Even with the clock already run out, only a retired declaration may leave.
    seed_ledger(&catalog, &[("live", 9 * DAY_MS), ("held", 9 * DAY_MS)]);

    let output = up_once(&catalog, &bin);

    assert!(
        catalog.join("agents/h/live").is_dir(),
        "{}",
        stdout(&output)
    );
    assert!(
        catalog.join("agents/h/held").is_dir(),
        "{}",
        stdout(&output)
    );
    assert!(
        !catalog.join(".st2/archive").exists(),
        "{}",
        stdout(&output)
    );
    let ledger = ledger(&catalog).unwrap();
    assert_eq!(
        ledger["hosts"]["h"],
        serde_json::json!({}),
        "rows for seats that are not retired must be pruned"
    );
}

#[test]
fn a_seat_whose_task_record_survives_is_reported_and_left_alone() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    pty_shim(&bin, "[{\"name\":\"h.gone\",\"status\":\"exited\"}]");
    seat(&catalog, "gone", RETIRED);
    seed_ledger(&catalog, &[("gone", 8 * DAY_MS)]);

    let output = up_once(&catalog, &bin);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        catalog.join("agents/h/gone").is_dir(),
        "an incomplete retirement must fail closed"
    );
    assert!(
        stderr.contains("auto-archive skipped h.gone [runtime-record-present]"),
        "the refusal must be surfaced, not swallowed:\n{stderr}"
    );
}

/// A `catalog apply` can re-create a declaration under a name the archive still holds. That one
/// collision must be a reported skip, not an error that aborts the batch — otherwise the
/// supervisor's pass wedges on it forever and the other due seats never drain.
#[test]
fn an_occupied_archive_slot_is_skipped_without_stalling_the_rest_of_the_batch() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    seat(&catalog, "collide", RETIRED);
    seat(&catalog, "clean", RETIRED);
    seed_ledger(&catalog, &[("collide", 8 * DAY_MS), ("clean", 8 * DAY_MS)]);
    write(
        &catalog,
        ".st2/archive/h/collide/agent.kdl",
        "agent \"collide\" { host \"h\" }\n",
    );

    let output = up_once(&catalog, &bin);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains("auto-archive skipped h.collide [archive-occupied]"),
        "the collision must be surfaced:\n{stderr}"
    );
    assert!(
        catalog.join("agents/h/collide").is_dir(),
        "the colliding seat stays put"
    );
    assert!(
        !catalog.join("agents/h/clean").exists(),
        "the rest of the batch must still drain:\n{}",
        stdout(&output)
    );
}

#[test]
fn a_pass_archives_at_most_twenty_five_seats_and_drains_the_rest_next_pass() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    let identities: Vec<String> = (0..30).map(|index| format!("seat-{index:02}")).collect();
    for identity in &identities {
        seat(&catalog, identity, RETIRED);
    }
    let observed: Vec<(&str, u64)> = identities
        .iter()
        .map(|identity| (identity.as_str(), 8 * DAY_MS))
        .collect();
    seed_ledger(&catalog, &observed);

    let first = up_once(&catalog, &bin);
    let archived = |root: &Path| {
        identities
            .iter()
            .filter(|identity| root.join(".st2/archive/h").join(identity).is_dir())
            .count()
    };
    assert_eq!(
        archived(&catalog),
        25,
        "one pass stays bounded:\n{}",
        stdout(&first)
    );

    let second = up_once(&catalog, &bin);
    assert_eq!(
        archived(&catalog),
        30,
        "the remainder drains on the next pass:\n{}",
        stdout(&second)
    );

    let third = up_once(&catalog, &bin);
    assert!(
        !stdout(&third).contains("archived"),
        "an already-drained catalog is a no-op:\n{}",
        stdout(&third)
    );
    assert_eq!(
        ledger(&catalog).unwrap()["hosts"]["h"],
        serde_json::json!({}),
        "archived seats leave no ledger rows behind"
    );
}

#[test]
fn repeated_passes_over_an_archived_seat_change_nothing() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    seat(&catalog, "gone", RETIRED);
    seed_ledger(&catalog, &[("gone", 8 * DAY_MS)]);

    up_once(&catalog, &bin);
    let tombstone = fs::read(catalog.join(".st2/archive/h/gone.tombstone.json")).unwrap();

    let second = up_once(&catalog, &bin);
    assert!(
        !stdout(&second).contains("archived"),
        "the second pass must find nothing to do:\n{}",
        stdout(&second)
    );
    assert_eq!(
        fs::read(catalog.join(".st2/archive/h/gone.tombstone.json")).unwrap(),
        tombstone,
        "the tombstone must not be rewritten"
    );
}

#[test]
fn un_retiring_a_seat_inside_the_grace_period_restarts_its_clock() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    seat(&catalog, "back", RETIRED);
    seed_ledger(&catalog, &[("back", 6 * DAY_MS)]);

    seat(&catalog, "back", "desired-state \"running\"");
    up_once(&catalog, &bin);
    assert_eq!(
        ledger(&catalog).unwrap()["hosts"]["h"],
        serde_json::json!({}),
        "coming back must drop the old observation"
    );

    seat(&catalog, "back", RETIRED);
    up_once(&catalog, &bin);
    assert!(
        catalog.join("agents/h/back").is_dir(),
        "the second retirement serves a fresh grace period, not the first one's remainder"
    );
    let observed = ledger(&catalog).unwrap()["hosts"]["h"]["back"]
        .as_u64()
        .unwrap();
    assert!(
        now_ms().saturating_sub(observed) < 5 * 60 * 1000,
        "the clock must restart at the new observation, got {observed}"
    );
}

#[test]
fn a_contended_authoring_lock_skips_the_pass_instead_of_blocking_it() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, _bin) = fixture(&temporary);
    seat(&catalog, "gone", RETIRED);
    seed_ledger(&catalog, &[("gone", 8 * DAY_MS)]);

    let held = st2::CatalogLock::exclusive(&catalog).unwrap();
    let request = st2::catalog_archive::AutoArchiveRequest {
        catalog: catalog.clone(),
        host: "h".to_owned(),
        grace: Duration::from_secs(7 * 24 * 60 * 60),
        limit: 25,
    };
    // Off-thread with a deadline: a blocking acquisition would hang the test rather than fail it.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(st2::catalog_archive::auto_archive(request));
    });
    let outcome = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the archive pass queued behind a held authoring lock instead of skipping")
        .unwrap();

    assert!(outcome.is_none(), "a contended pass reports no result");
    assert!(
        catalog.join("agents/h/gone").is_dir(),
        "nothing may move while another holder owns the lock"
    );
    drop(held);
}
