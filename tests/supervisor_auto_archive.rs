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
const DIRECT_LEDGER: &str = ".st2/direct-dead-observed.json";
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
    seed_ledger_at(root, LEDGER, "st2.catalog-retired-observed.v1", observed);
}

/// Place each direct actor's observed PTY death `age_ms` in the past.
fn seed_direct_ledger(root: &Path, observed: &[(&str, u64)]) {
    seed_ledger_at(
        root,
        DIRECT_LEDGER,
        "st2.catalog-direct-dead-observed.v1",
        observed,
    );
}

fn seed_ledger_at(root: &Path, path: &str, schema: &str, observed: &[(&str, u64)]) {
    let rows = observed
        .iter()
        .map(|(identity, age_ms)| format!("      \"{identity}\": {}", now_ms() - age_ms))
        .collect::<Vec<_>>()
        .join(",\n");
    write(
        root,
        path,
        &format!(
            "{{\n  \"schema\": \"{schema}\",\n  \"hosts\": {{\n    \"h\": {{\n{rows}\n    }}\n  }}\n}}\n"
        ),
    );
}

fn ledger(root: &Path) -> Option<serde_json::Value> {
    ledger_at(root, LEDGER)
}

fn ledger_at(root: &Path, path: &str) -> Option<serde_json::Value> {
    let body = fs::read(root.join(path)).ok()?;
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

/// Seats the archive permanently refuses must not fill every pass's 25-seat window: the bound
/// counts seats that can leave, so an eligible retired seat sorted after them and a due dead
/// direct actor still leave in the same pass.
#[test]
fn permanently_refused_retired_seats_do_not_starve_eligible_seats_or_direct_actors() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    let blocked: Vec<String> = (0..26).map(|index| format!("blocked-{index:02}")).collect();
    for identity in &blocked {
        seat(&catalog, identity, RETIRED);
        write(
            &catalog,
            &format!(".st2/archive/h/{identity}/agent.kdl"),
            &format!("agent \"{identity}\" {{ host \"h\" }}\n"),
        );
    }
    seat(&catalog, "zz-clean", RETIRED);
    let mut observed: Vec<(&str, u64)> = blocked
        .iter()
        .map(|identity| (identity.as_str(), 8 * DAY_MS))
        .collect();
    observed.push(("zz-clean", 8 * DAY_MS));
    seed_ledger(&catalog, &observed);
    direct_actor(&catalog, "direct.omp.2ahzpbs3");
    seed_direct_ledger(&catalog, &[("direct.omp.2ahzpbs3", 8 * DAY_MS)]);

    let output = up_once(&catalog, &bin);

    for identity in ["zz-clean", "direct.omp.2ahzpbs3"] {
        assert!(
            catalog.join(".st2/archive/h").join(identity).is_dir(),
            "{identity} must leave despite the refused seats:\n{}\n{}",
            stdout(&output),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    for identity in &blocked {
        assert!(
            catalog.join("agents/h").join(identity).is_dir(),
            "{identity}"
        );
    }
}

/// A supervisor and its retired dependent straddling the 25-seat cutoff must not split: the
/// supervisor may leave only in a batch that also carries the dependent, never ahead of it.
#[test]
fn a_supervisor_never_leaves_ahead_of_a_retired_dependent_cut_by_the_pass_limit() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    let fillers: Vec<String> = (0..24).map(|index| format!("a-{index:02}")).collect();
    for identity in &fillers {
        seat(&catalog, identity, RETIRED);
    }
    seat(&catalog, "boss", RETIRED);
    seat(
        &catalog,
        "worker",
        &format!("{RETIRED}\n  supervisor \"h.boss\""),
    );
    let mut observed: Vec<(&str, u64)> = fillers
        .iter()
        .map(|identity| (identity.as_str(), 8 * DAY_MS))
        .collect();
    observed.extend([("boss", 8 * DAY_MS), ("worker", 8 * DAY_MS)]);
    seed_ledger(&catalog, &observed);

    let first = up_once(&catalog, &bin);
    let stderr = String::from_utf8_lossy(&first.stderr);

    assert!(
        catalog.join("agents/h/boss").is_dir(),
        "the supervisor must not leave while its dependent stays live:\n{}\n{stderr}",
        stdout(&first)
    );
    assert!(
        stderr.contains("auto-archive skipped h.boss [supervisor-referenced]"),
        "{stderr}"
    );
    assert!(
        catalog.join(".st2/archive/h/worker").is_dir(),
        "the dependent takes the last slot:\n{}",
        stdout(&first)
    );

    let second = up_once(&catalog, &bin);
    assert!(
        catalog.join(".st2/archive/h/boss").is_dir(),
        "with its dependent gone the supervisor leaves next pass:\n{}\n{}",
        stdout(&second),
        String::from_utf8_lossy(&second.stderr)
    );
}

/// Refused seats do not consume the 25-seat limit, so the scan itself is bounded: one pass
/// examines at most 100 due seats, reports the rest as deferred, and the next pass resumes after
/// the last seat examined, so an eligible seat behind a wall of refusals still leaves.
#[test]
fn a_pass_over_many_refused_seats_examines_a_bounded_window_and_resumes_after_it() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    let blocked: Vec<String> = (0..101)
        .map(|index| format!("blocked-{index:03}"))
        .collect();
    for identity in &blocked {
        seat(&catalog, identity, RETIRED);
        write(
            &catalog,
            &format!(".st2/archive/h/{identity}/agent.kdl"),
            &format!("agent \"{identity}\" {{ host \"h\" }}\n"),
        );
    }
    seat(&catalog, "zz-clean", RETIRED);
    let mut observed: Vec<(&str, u64)> = blocked
        .iter()
        .map(|identity| (identity.as_str(), 8 * DAY_MS))
        .collect();
    observed.push(("zz-clean", 8 * DAY_MS));
    seed_ledger(&catalog, &observed);

    let first = up_once(&catalog, &bin);
    let stderr = String::from_utf8_lossy(&first.stderr);

    assert_eq!(
        stderr.matches("auto-archive skipped").count(),
        100,
        "one pass examines at most four times its limit:\n{stderr}"
    );
    assert!(
        stderr.contains(
            "auto-archive examined 100 due retired seats and deferred 2 to the next pass"
        ),
        "the unexamined remainder must be reported:\n{stderr}"
    );
    assert!(catalog.join("agents/h/zz-clean").is_dir(), "{stderr}");

    let second = up_once(&catalog, &bin);
    assert!(
        catalog.join(".st2/archive/h/zz-clean").is_dir(),
        "the next pass resumes after the window and reaches the eligible seat:\n{}",
        String::from_utf8_lossy(&second.stderr)
    );
    for identity in &blocked {
        assert!(
            catalog.join("agents/h").join(identity).is_dir(),
            "{identity}"
        );
    }
}

/// The direct-actor half of the bounded scan: refused dead actors do not consume the limit either,
/// so one pass examines at most 100 of them, reports the rest as deferred, and the next pass
/// resumes after its cursor in the direct ledger, reaching a clean actor behind the refusals.
#[test]
fn a_pass_over_many_refused_direct_actors_examines_a_bounded_window_and_resumes_after_it() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    // Encoded PTY IDs keep identity order equal to the plain names' order.
    let encode = |pty_id: &str| {
        let hex: String = pty_id.bytes().map(|byte| format!("{byte:02x}")).collect();
        format!("direct.omp.x-{hex}")
    };
    let blocked: Vec<String> = (0..101)
        .map(|index| encode(&format!("blocked-{index:03}")))
        .collect();
    for identity in &blocked {
        direct_actor(&catalog, identity);
        write(
            &catalog,
            &format!(".st2/archive/h/{identity}/resources/note"),
            "occupied\n",
        );
    }
    let clean = encode("zz-clean");
    direct_actor(&catalog, &clean);
    let mut observed: Vec<(&str, u64)> = blocked
        .iter()
        .map(|identity| (identity.as_str(), 8 * DAY_MS))
        .collect();
    observed.push((clean.as_str(), 8 * DAY_MS));
    seed_direct_ledger(&catalog, &observed);

    let first = up_once(&catalog, &bin);
    let stderr = String::from_utf8_lossy(&first.stderr);

    assert_eq!(
        stderr.matches("auto-archive skipped").count(),
        100,
        "one pass examines at most four times its limit:\n{stderr}"
    );
    assert!(
        stderr.contains(
            "auto-archive examined 100 due dead direct actors and deferred 2 to the next pass"
        ),
        "the unexamined remainder must be reported:\n{stderr}"
    );
    assert!(catalog.join("agents/h").join(&clean).is_dir(), "{stderr}");
    assert_eq!(
        ledger_at(&catalog, DIRECT_LEDGER).unwrap()["resumeAfter"]["h"],
        blocked[99].as_str(),
        "the cursor is persisted in the direct ledger"
    );

    let second = up_once(&catalog, &bin);
    assert!(
        catalog.join(".st2/archive/h").join(&clean).is_dir(),
        "the next pass resumes after the window and reaches the clean actor:\n{}",
        String::from_utf8_lossy(&second.stderr)
    );
    for identity in &blocked {
        assert!(
            catalog.join("agents/h").join(identity).is_dir(),
            "{identity}"
        );
    }
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

/// A direct OMP actor's directory: no declaration, only what writers created under its identity.
fn direct_actor(root: &Path, identity: &str) {
    write(
        root,
        &format!("agents/h/{identity}/resources/decisions/1790000000000-abc123.md"),
        "# decision\n\nkeep the fold at the actor boundary\n",
    );
}

fn st2(root: &Path, bin: &Path, args: &[&str]) -> Output {
    let home = root.parent().unwrap().join("home");
    Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["--catalog", root.to_str().unwrap()])
        .args(args)
        .env("PATH", bin)
        .env("HOME", &home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("PTY_ROOT", home.join("pty"))
        .env_remove("CATALOG")
        .env_remove("ST_ROOT")
        .output()
        .unwrap()
}

#[test]
fn a_direct_actor_whose_pty_death_outlived_the_grace_period_is_archived_and_a_live_one_stays() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    // `e2jcd9pf` runs; `2ahzpbs3` left an exited record; `web` (encoded `x-776562`) and
    // `3bk3wt7v` have no record at all.
    pty_shim(
        &bin,
        "[{\"name\":\"e2jcd9pf\",\"status\":\"running\"},{\"name\":\"2ahzpbs3\",\"status\":\"exited\"}]",
    );
    seat(
        &catalog,
        "held",
        "desired-state \"suspended\" reason=\"paused\"",
    );
    for identity in [
        "direct.omp.e2jcd9pf",
        "direct.omp.2ahzpbs3",
        "direct.omp.x-776562",
        "direct.omp.3bk3wt7v",
    ] {
        direct_actor(&catalog, identity);
    }
    let decision = fs::read(
        catalog.join("agents/h/direct.omp.2ahzpbs3/resources/decisions/1790000000000-abc123.md"),
    )
    .unwrap();
    // A stale row for the running PTY proves liveness, not the clock, gates the move.
    seed_direct_ledger(
        &catalog,
        &[
            ("direct.omp.e2jcd9pf", 9 * DAY_MS),
            ("direct.omp.2ahzpbs3", 8 * DAY_MS),
            ("direct.omp.x-776562", 8 * DAY_MS),
        ],
    );

    let output = up_once(&catalog, &bin);

    assert!(
        stdout(&output).contains("archived (2)"),
        "the pass must report both dead actors:\n{}",
        stdout(&output)
    );
    assert!(
        catalog.join("agents/h/direct.omp.e2jcd9pf").is_dir(),
        "a running PTY's actor never leaves"
    );
    assert!(
        catalog.join("agents/h/direct.omp.3bk3wt7v").is_dir(),
        "a death first observed this pass serves its own grace period"
    );
    for identity in ["direct.omp.2ahzpbs3", "direct.omp.x-776562"] {
        assert!(
            !catalog.join("agents/h").join(identity).exists(),
            "{identity}"
        );
        assert!(
            catalog.join(".st2/archive/h").join(identity).is_dir(),
            "{identity}"
        );
    }
    assert_eq!(
        fs::read(catalog.join(
            ".st2/archive/h/direct.omp.2ahzpbs3/resources/decisions/1790000000000-abc123.md"
        ))
        .unwrap(),
        decision,
        "the whole actor directory moves byte-identically"
    );

    let tombstone: serde_json::Value = serde_json::from_slice(
        &fs::read(catalog.join(".st2/archive/h/direct.omp.x-776562.tombstone.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(tombstone["schema"], "st2.catalog-archive-tombstone.v1");
    assert_eq!(tombstone["id"], "h.direct.omp.x-776562");
    assert_eq!(tombstone["identity"], "direct.omp.x-776562");
    assert_eq!(
        tombstone["archiveRoot"],
        ".st2/archive/h/direct.omp.x-776562"
    );
    assert!(
        tombstone["reason"]
            .as_str()
            .unwrap()
            .contains("PTY session web "),
        "the reason names the exact decoded PTY session: {tombstone:#}"
    );

    let rows = &ledger_at(&catalog, DIRECT_LEDGER).unwrap()["hosts"]["h"];
    assert!(
        rows.get("direct.omp.e2jcd9pf").is_none(),
        "a running PTY drops its death observation: {rows:#}"
    );
    let fresh = rows["direct.omp.3bk3wt7v"].as_u64().unwrap();
    assert!(now_ms().saturating_sub(fresh) < 5 * 60 * 1000, "{rows:#}");

    let graph = st2(
        &catalog,
        &bin,
        &["catalog", "graph", "--host", "h", "--json"],
    );
    let graph: serde_json::Value = serde_json::from_slice(&graph.stdout).unwrap();
    let archived = graph["archived"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        archived,
        ["h.direct.omp.2ahzpbs3", "h.direct.omp.x-776562"],
        "{graph:#}"
    );
    let direct = graph["directActors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        direct,
        ["h.direct.omp.3bk3wt7v", "h.direct.omp.e2jcd9pf"],
        "{graph:#}"
    );
}

/// Kills the stand-in `exec` process when the test ends, pass or fail.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Only the PTY registry is direct actor liveness. A running `exec` task whose runtime ID equals
/// the dead actor's decoded PTY ID must not keep the actor in the live catalog.
fn assert_exec_record_is_not_pty_evidence(pty_id: &str, identity: &str) {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    seat(
        &catalog,
        "held",
        "desired-state \"suspended\" reason=\"paused\"",
    );
    direct_actor(&catalog, identity);
    seed_direct_ledger(&catalog, &[(identity, 8 * DAY_MS)]);

    let exec = KillOnDrop(Command::new("sleep").arg("300").spawn().unwrap());
    // The exec backend's record for this runtime ID (under `up_once`'s `XDG_STATE_HOME`),
    // published after the process started, as the backend writes it.
    std::thread::sleep(Duration::from_millis(50));
    write(
        temporary.path(),
        &format!("home/state/st2/h/exec/{pty_id}.pid"),
        &exec.0.id().to_string(),
    );

    let output = up_once(&catalog, &bin);

    assert!(
        catalog.join(format!(".st2/archive/h/{identity}")).is_dir(),
        "an exec record is not PTY evidence:\n{}\n{}",
        stdout(&output),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!catalog.join(format!("agents/h/{identity}")).exists());
    drop(exec);
}

#[test]
fn a_running_exec_task_sharing_a_dead_actors_pty_id_does_not_keep_it_alive() {
    assert_exec_record_is_not_pty_evidence("2ahzpbs3", "direct.omp.2ahzpbs3");
}

/// An exec task keeps an explicit runtime ID verbatim, so a non-generated PTY ID (hex-encoded in
/// the actor identity) can collide with it too.
#[test]
fn a_running_exec_task_with_an_explicit_id_does_not_keep_a_hex_encoded_actor_alive() {
    assert_exec_record_is_not_pty_evidence("web", "direct.omp.x-776562");
}

#[test]
fn an_archived_direct_actor_unarchives_to_its_live_directory_and_restarts_its_clock() {
    let temporary = tempfile::tempdir().unwrap();
    let (catalog, bin) = fixture(&temporary);
    seat(
        &catalog,
        "held",
        "desired-state \"suspended\" reason=\"paused\"",
    );
    direct_actor(&catalog, "direct.omp.2ahzpbs3");
    let decision = fs::read(
        catalog.join("agents/h/direct.omp.2ahzpbs3/resources/decisions/1790000000000-abc123.md"),
    )
    .unwrap();
    seed_direct_ledger(&catalog, &[("direct.omp.2ahzpbs3", 8 * DAY_MS)]);

    up_once(&catalog, &bin);
    assert!(catalog.join(".st2/archive/h/direct.omp.2ahzpbs3").is_dir());

    let restored = st2(
        &catalog,
        &bin,
        &[
            "catalog",
            "unarchive",
            "direct.omp.2ahzpbs3",
            "--host",
            "h",
            "--json",
        ],
    );
    assert!(
        restored.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&restored.stdout).unwrap();
    assert_eq!(receipt["id"], "h.direct.omp.2ahzpbs3");
    assert_eq!(receipt["to"], "agents/h/direct.omp.2ahzpbs3");
    assert_eq!(
        fs::read(
            catalog
                .join("agents/h/direct.omp.2ahzpbs3/resources/decisions/1790000000000-abc123.md")
        )
        .unwrap(),
        decision
    );
    assert!(
        !catalog
            .join(".st2/archive/h/direct.omp.2ahzpbs3.tombstone.json")
            .exists()
    );

    // The archive pass pruned the old row, so the restored actor is observed afresh rather than
    // archived again on the very next pass.
    let output = up_once(&catalog, &bin);
    assert!(
        catalog.join("agents/h/direct.omp.2ahzpbs3").is_dir(),
        "an unarchived actor serves a fresh grace period:\n{}",
        stdout(&output)
    );
    let observed = ledger_at(&catalog, DIRECT_LEDGER).unwrap()["hosts"]["h"]["direct.omp.2ahzpbs3"]
        .as_u64()
        .unwrap();
    assert!(now_ms().saturating_sub(observed) < 5 * 60 * 1000);
}
