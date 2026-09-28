//! #204 against the real binaries and real processes: a fail-mode task crash-loops into a terminal
//! park, `st2 tasks` reports the fault, `st2 unpark` clears exactly that task, and a healthy peer
//! keeps the same pid and generation throughout.
//!
//! `tests/run.rs` proves the same recovery deterministically against a fake runner, which is the
//! CI-gated proof. This one exists for the claim that a fake runner structurally cannot make: that
//! *no supervisor-wide restart occurred*. A pid and a process generation that survive the whole
//! episode are the only direct evidence of that, and they require real processes.
//!
//! Note that `st2 up --once` cannot appear here. Both single-pass entry points build a fresh
//! `FlappingCap`, so a one-shot reconcile can never park anything; the park is a property of a
//! long-running supervisor, and this test drives a real one.
//!
//! Every wait is on an observed condition with a deadline, never a fixed sleep standing in for one.
//! `XDG_STATE_HOME` is a temp dir, so nothing here can read or clear a real host's parks.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const HOST: &str = "th";
const FLAPPER: &str = "th.flapper.work";
const HEALTHY: &str = "th.healthy.work";
/// The restart policy's observation window. Short enough to test, long enough that "still up after
/// it" means the budget was genuinely forgiven rather than merely not yet spent.
const INTERVAL: Duration = Duration::from_secs(3);
const DEADLINE: Duration = Duration::from_secs(30);

/// Kills the supervisor and tears down its tasks even when an assertion unwinds, so a failing run
/// cannot leave `sleep` processes behind.
struct Fleet {
    supervisor: Child,
    catalog: PathBuf,
    state: PathBuf,
}

impl Drop for Fleet {
    fn drop(&mut self) {
        let _ = self.supervisor.kill();
        let _ = self.supervisor.wait();
        // Only after the supervisor is gone, or it would just relaunch what `down` tears down.
        let _ = self.st2(&["down", "--host", HOST]).status();
    }
}

impl Fleet {
    fn st2(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_st2"));
        command
            .arg("--catalog")
            .arg(&self.catalog)
            .args(args)
            .env("XDG_STATE_HOME", &self.state)
            .env("HOME", &self.state);
        command
    }

    fn tasks(&self) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let output = self
                .st2(&["tasks", "--host", HOST, "--json"])
                .output()
                .expect("run st2 tasks");
            let stdout = String::from_utf8(output.stdout).expect("st2 tasks emits utf-8");
            let value: serde_json::Value = serde_json::from_str(&stdout)
                .unwrap_or_else(|error| panic!("st2 tasks emitted {stdout:?}: {error}"));
            let transient_start_time_race = value["errors"].as_array().is_some_and(|errors| {
                !errors.is_empty()
                    && errors.iter().all(|error| {
                        error.as_str().is_some_and(|error| {
                            error.contains("recorded startTimeTicks does not match the live pid")
                        })
                    })
            });
            if value["complete"] != true && transient_start_time_race && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            assert_eq!(
                value["complete"], true,
                "the inventory was incomplete: {}",
                value["errors"]
            );
            assert!(
                output.status.success(),
                "a complete inventory must exit zero (errors: {})",
                value["errors"]
            );
            return value;
        }
    }

    fn row(&self, runtime_id: &str) -> serde_json::Value {
        self.tasks()["tasks"]
            .as_array()
            .expect("tasks is an array")
            .iter()
            .find(|row| row["runtimeId"] == runtime_id)
            .unwrap_or_else(|| panic!("{runtime_id} is not in the inventory"))
            .clone()
    }

    /// Poll the inventory until `row` satisfies `done`, or fail with what it last looked like.
    fn until(
        &self,
        runtime_id: &str,
        what: &str,
        done: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let deadline = Instant::now() + DEADLINE;
        let mut last = self.row(runtime_id);
        while Instant::now() < deadline {
            if done(&last) {
                return last;
            }
            std::thread::sleep(Duration::from_millis(100));
            last = self.row(runtime_id);
        }
        panic!("{runtime_id} never {what}; last seen as {last}");
    }
}

fn write_agent(catalog: &Path, identity: &str, body: &str) {
    let dir = catalog.join("agents").join(HOST).join(identity);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("agent.kdl"), body).unwrap();
}

/// A task that exits the instant it starts, under a budget of one launch. This is the production
/// shape from #204: parking took five seconds there, because a task that dies immediately is
/// relaunched at the folder-watch rate rather than the timer rate.
fn crash_looping_flapper(catalog: &Path) {
    write_agent(
        catalog,
        "flapper",
        &format!(
            r#"agent "flapper" {{
  host "{HOST}"
  restart {{ attempts 1; interval "{}s"; delay "0s"; mode "fail" }}
  exec "work" {{ id "{FLAPPER}"; argv "/bin/sh" "-c" "exit 7" }}
}}
"#,
            INTERVAL.as_secs()
        ),
    );
}

/// The same declaration with its cause fixed — what an operator edits before asking for recovery.
fn repaired_flapper(catalog: &Path) {
    write_agent(
        catalog,
        "flapper",
        &format!(
            r#"agent "flapper" {{
  host "{HOST}"
  restart {{ attempts 1; interval "{}s"; delay "0s"; mode "fail" }}
  exec "work" {{ id "{FLAPPER}"; argv "/bin/sh" "-c" "sleep 600" }}
}}
"#,
            INTERVAL.as_secs()
        ),
    );
}

fn generation(row: &serde_json::Value) -> (serde_json::Value, serde_json::Value) {
    (
        row["runtime"]["pid"].clone(),
        row["runtime"]["generationId"].clone(),
    )
}

#[test]
fn a_real_supervisor_parks_a_crash_looper_and_unpark_recovers_only_that_task() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&catalog).unwrap();
    std::fs::create_dir_all(&state).unwrap();

    crash_looping_flapper(&catalog);
    write_agent(
        &catalog,
        "healthy",
        &format!(
            r#"agent "healthy" {{
  host "{HOST}"
  exec "work" {{ id "{HEALTHY}"; argv "/bin/sh" "-c" "sleep 600" }}
}}
"#
        ),
    );

    let supervisor = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("--catalog")
        .arg(&catalog)
        .args(["up", "--host", HOST, "--interval", "1"])
        .env("XDG_STATE_HOME", &state)
        .env("HOME", &state)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start a real supervisor");
    let fleet = Fleet {
        supervisor,
        catalog: catalog.clone(),
        state,
    };

    // 1 + 2. It crash-loops to a terminal park, and the inventory says so. Before this change the
    // row below read `desiredState: running`, nothing running, `error: null` — a task that should be
    // up, was not up, and had nothing visibly wrong with it.
    let parked = fleet.until(FLAPPER, "parked", |row| !row["parked"].is_null());
    assert_eq!(parked["desiredState"], "running");
    assert!(
        parked["runtime"]["state"] == "exited" || parked["runtime"]["state"] == "absent",
        "unexpected runtime state {}",
        parked["runtime"]["state"]
    );
    assert_eq!(
        parked["parked"]["recovery"],
        serde_json::json!({
            "argv": [
                "st2",
                "--catalog",
                catalog.display().to_string(),
                "unpark",
                FLAPPER,
                "--host",
                HOST
            ]
        }),
        "the fault must carry its own remedy"
    );

    // 3. The healthy peer is the negative control, and its generation is the thing a host-wide
    // restart would have changed.
    let healthy_before = generation(&fleet.row(HEALTHY));
    assert_eq!(fleet.row(HEALTHY)["runtime"]["state"], "running");
    assert!(!healthy_before.0.is_null() && !healthy_before.1.is_null());

    // Fixing the declaration is not itself a recovery. Making a park clear on redeclaration would put
    // the remedy back in the republish-the-agent path that #204 reports as already not working.
    repaired_flapper(&catalog);
    let deadline = Instant::now() + INTERVAL;
    while Instant::now() < deadline {
        assert!(
            !fleet.row(FLAPPER)["parked"].is_null(),
            "editing the declaration released the park on its own"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // 4. The supported targeted recovery.
    let unpark = fleet
        .st2(&["unpark", FLAPPER, "--host", HOST])
        .output()
        .expect("run st2 unpark");
    assert!(unpark.status.success(), "st2 unpark failed: {unpark:?}");

    let recovered = fleet.until(FLAPPER, "recovered", |row| {
        row["parked"].is_null() && row["runtime"]["state"] == "running"
    });
    let recovered_at = Instant::now();
    assert!(!recovered["runtime"]["pid"].is_null());

    // 5. No supervisor-wide restart occurred: the peer is the same process it always was.
    assert_eq!(
        generation(&fleet.row(HEALTHY)),
        healthy_before,
        "the healthy peer was restarted, so recovery was not targeted"
    );

    // 6. And it stays recovered past the policy's own observation window, rather than for one pass.
    while recovered_at.elapsed() < INTERVAL + Duration::from_secs(1) {
        let row = fleet.row(FLAPPER);
        assert_eq!(
            row["runtime"]["state"], "running",
            "the recovered task fell over again"
        );
        assert!(
            row["parked"].is_null(),
            "the recovered task re-parked: {row}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        generation(&fleet.row(HEALTHY)),
        healthy_before,
        "the healthy peer was restarted while the recovered task settled"
    );
}
