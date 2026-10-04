#![cfg(unix)]
//! An idle daemon uses almost no CPU.
//!
//! A daemon with its reconciler runs over a graph with missions that wait on a person and
//! mailboxes with a history, while real seat drivers sit idle around it. Nothing changes, so the
//! daemon must stay under a CPU budget and answer only the requests idle seats are known to make.
//! The report names every client and request kind, so a new poll or a pass that runs when nothing
//! changed fails here with the caller that causes it.
//!
//! CPU is this process's own time: the daemon runs in it, the seats run as child processes.
//! Run it under nextest, as CI does, so no other test shares the process:
//!
//! ```sh
//! cargo nextest run -p st3 --test integration idle_budget:: --no-capture
//! ```
//!
//! Inside an st seat, run it with `env -u ST_AGENT` under `systemd-run --user --pipe` or another
//! parent outside the seat: the API binds callers to the agent in their process ancestry.

use std::collections::BTreeMap;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};
use st3::api::AppState;
use st3::model::{ClaimInput, IntentInput, MissionRunRequest};
use st3::reconcile::{Reconciler, RuntimeControl, RuntimeObservation};
use st3::store::Store;
use tokio::sync::{Notify, watch};

const NODE: &str = "idle-node";
/// Idle seats, each a real `st3 driver claude` around a provider that never starts a turn.
const SEATS: usize = 6;
/// Closed messages in each seat's mailbox.
const HISTORY: usize = 60;
/// Missions whose only step waits on a person.
const MISSIONS: usize = 12;
/// Seats start, settle their first observations and reach their steady loop.
const WARMUP: Duration = Duration::from_secs(15);
const WINDOW: Duration = Duration::from_secs(30);

/// The whole daemon, as a share of one core, while nothing changes.
const CPU_BUDGET: f64 = 0.05;
/// Each idle seat's driver pages its mailbox once a second; the page carries its delivery report.
const SEAT_REQUESTS_PER_SECOND: f64 = 1.25;
/// Reconcile passes in the window. A pass runs when something changed or a deadline came due;
/// neither happens here.
const PASS_BUDGET: u64 = 2;

/// A runtime that reports each idle seat as running and starts nothing.
struct IdleRuntime {
    seats: Vec<(String, String)>,
}

impl RuntimeControl for IdleRuntime {
    fn snapshot_ptys(&self) -> Result<Vec<RuntimeObservation>> {
        Ok(self
            .seats
            .iter()
            .map(|(runtime_id, incarnation)| RuntimeObservation {
                runtime_id: runtime_id.clone(),
                terminal: true,
                status: "running".into(),
                exit_code: None,
                incarnation_id: Some(incarnation.clone()),
            })
            .collect())
    }
    fn observe_exec(&self, _: &str) -> Result<Option<RuntimeObservation>> {
        Ok(None)
    }
    fn start(&self, _: &st3::model::MemberSpec) -> Result<()> {
        Ok(())
    }
    fn stop(&self, _: &str, _: bool, _: Option<&str>) -> Result<()> {
        Ok(())
    }
    fn kill(&self, _: &str, _: bool, _: Option<&str>) -> Result<()> {
        Ok(())
    }
    fn remove(&self, _: &str, _: bool) -> Result<()> {
        Ok(())
    }
    fn screen(&self, _: &str) -> Result<String> {
        Ok(String::new())
    }
    fn send_key(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn read_exec_log(&self, _: &str) -> Result<Option<String>> {
        Ok(None)
    }
}

fn seat(index: usize) -> (String, String) {
    (
        format!("idle-seat-{index}"),
        format!("{}:2026-10-01T09:00:00.000Z", 4000 + index),
    )
}

fn append(store: &Store, subject: &str, kind: &str, actor: Option<&str>, fields: Value) {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: actor.map(Into::into),
            fields: serde_json::from_value::<BTreeMap<String, Value>>(fields).unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

/// Missions that wait on a person, a mailbox history for every seat, and each seat running.
fn seed(store: &Store, workspace: &Path) {
    let mut source = String::from("version 2\n");
    for index in 0..MISSIONS {
        source.push_str(&format!(
            r#"
mission "idle/wait-{index}" state="ready" {{
  goal "Wait for a person."
  completion {{ when "all-steps-exhausted" }}
  step "review" {{
    agentless
    gate "accept" type="human" {{
      reviewer "person/example"
      question "Accept this?"
    }}
  }}
}}
"#
        ));
    }
    let intent = st3::parse_intent(&source, NODE).unwrap();
    let plan = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.clone(),
                source_name: None,
            },
        )
        .unwrap();
    assert!(plan.blockers.is_empty(), "{:?}", plan.blockers);
    store
        .apply(&intent, &plan.subject_tokens, "idle-budget")
        .unwrap();
    for index in 0..MISSIONS {
        let mission = format!("idle/wait-{index}");
        store
            .create_mission_run(&MissionRunRequest {
                mission: mission.clone(),
                revision: None,
                workspace: workspace.display().to_string(),
                requester: Some("person/example".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: format!("idle-run:{mission}"),
            })
            .unwrap();
    }
    for index in 0..SEATS {
        let (runtime_id, incarnation) = seat(index);
        let agent = format!("agent/{runtime_id}");
        for message in 0..HISTORY {
            let subject = format!("message/idle-{index}-{message}");
            append(
                store,
                &subject,
                "message.sent",
                None,
                json!({"from": "person/example", "to": agent, "content": "An earlier note.", "status": "sent"}),
            );
            for status in ["delivered", "read", "closed"] {
                append(
                    store,
                    &subject,
                    &format!("message.{status}"),
                    Some(&agent),
                    json!({"status": status}),
                );
            }
        }
        append(
            store,
            &agent,
            "runtime.observed",
            None,
            json!({
                "runtime_id": runtime_id,
                "incarnation_id": incarnation,
                "status": "running",
                "reachability": "local",
            }),
        );
    }
}

/// A seat driver with the environment the reconciler gives it, its directories in `root`.
fn seat_driver(root: &Path, socket: &Path, index: usize) -> Child {
    let root = root.join(format!("seat-{index}"));
    for directory in ["workspace", "home", "state", "config", "runtime", "drivers"] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
    }
    let (runtime_id, _) = seat(index);
    st3::test_support::command(assert_cmd::cargo::cargo_bin!("st3-fixture"))
        // Its own process group, so stopping the seat also stops the stand-in provider.
        .process_group(0)
        .current_dir(root.join("workspace"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root.join("home"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("ST3_DRIVER_STATE_DIR", root.join("drivers"))
        .env("ST3_DAEMON_WAIT", "0")
        .arg("--endpoint")
        .arg(socket)
        .args(["driver", "claude", "--subject"])
        .arg(format!("agent/{runtime_id}"))
        .args(["--", "sleep", "600"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// CPU time this process has used: the daemon's API, reconciler and writer threads.
fn process_cpu() -> Duration {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: usage points to writable memory the size of a rusage.
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    // SAFETY: getrusage filled it.
    let usage = unsafe { usage.assume_init() };
    let time = |value: libc::timeval| {
        Duration::from_secs(value.tv_sec as u64) + Duration::from_micros(value.tv_usec as u64)
    };
    time(usage.ru_utime) + time(usage.ru_stime)
}

/// Counts by label in one table of the performance report.
fn counts(report: &Value, table: &str, label: impl Fn(&Value) -> String) -> BTreeMap<String, u64> {
    report[table]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| (label(row), row["count"].as_u64().unwrap_or_default()))
        .collect()
}

fn delta(after: &BTreeMap<String, u64>, before: &BTreeMap<String, u64>) -> Vec<(String, u64)> {
    let mut rows = after
        .iter()
        .map(|(label, count)| {
            (
                label.clone(),
                count.saturating_sub(before.get(label).copied().unwrap_or_default()),
            )
        })
        .filter(|(_, count)| *count > 0)
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_idle_daemon_stays_under_its_cpu_and_request_budget() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    // A file-backed store, as a member runs: its reads and writes cost what they cost there.
    let store = Arc::new(Store::open(&root.join("claims.sqlite3"), NODE).unwrap());
    seed(&store, &workspace);

    let notify = Arc::new(Notify::new());
    let event_notify = watch::channel(0_u64).0;
    let state = AppState {
        store: store.clone(),
        notify: notify.clone(),
        event_notify,
        node: NODE.into(),
        state_dir: root.join("daemon"),
        pty_root: root.join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: st3::model::PlannerSpec::default(),
    };
    std::fs::create_dir_all(root.join("daemon")).unwrap();
    let socket = root.join("st3.sock");
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        let _ = st3::api::serve_unix(&server_socket, st3::api::router(state)).await;
    });
    let reconciler = Arc::new(Reconciler::new(
        store.clone(),
        Arc::new(IdleRuntime {
            seats: (0..SEATS).map(seat).collect(),
        }),
        NODE.into(),
        notify,
    ));
    let reconciling = tokio::spawn(reconciler.supervise());
    let started = Instant::now();
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the API never listened"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let mut seats = (0..SEATS)
        .map(|index| seat_driver(root, &socket, index))
        .collect::<Vec<_>>();
    tokio::time::sleep(WARMUP).await;
    for (index, seat) in seats.iter_mut().enumerate() {
        assert!(
            seat.try_wait().unwrap().is_none(),
            "idle seat {index} exited during the warmup"
        );
    }

    let cpu_before = process_cpu();
    let report_before = st3::performance::snapshot();
    let measured = Instant::now();
    tokio::time::sleep(WINDOW).await;
    let elapsed = measured.elapsed().as_secs_f64();
    let cpu = (process_cpu() - cpu_before).as_secs_f64() / elapsed;
    let report_after = st3::performance::snapshot();

    for mut seat in seats {
        let group = i32::try_from(seat.id()).unwrap();
        // SAFETY: the seat runs in its own process group.
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
        let _ = seat.wait();
    }
    reconciling.abort();
    server.abort();

    let client_label = |row: &Value| row["client"].as_str().unwrap_or("?").to_owned();
    let pair_label = |row: &Value| {
        format!(
            "{} {}",
            row["client"].as_str().unwrap_or("?"),
            row["kind"].as_str().unwrap_or("?")
        )
    };
    let kind_label = |row: &Value| row["kind"].as_str().unwrap_or("?").to_owned();
    let clients = delta(
        &counts(&report_after, "clients", client_label),
        &counts(&report_before, "clients", client_label),
    );
    let pairs = delta(
        &counts(&report_after, "client_requests", pair_label),
        &counts(&report_before, "client_requests", pair_label),
    );
    let kinds = delta(
        &counts(&report_after, "requests", kind_label),
        &counts(&report_before, "requests", kind_label),
    );
    let requests = report_after["request_count"].as_u64().unwrap_or_default()
        - report_before["request_count"].as_u64().unwrap_or_default();
    let passes = kinds
        .iter()
        .find(|(kind, _)| kind == "task reconcile-pass")
        .map(|(_, count)| *count)
        .unwrap_or_default();
    let mut summary = format!(
        "idle daemon over {elapsed:.0} s: CPU {:.1}% of one core (budget {:.0}%), {requests} requests \
         ({:.2}/s), {passes} reconcile passes\nby client:\n",
        cpu * 100.0,
        CPU_BUDGET * 100.0,
        requests as f64 / elapsed
    );
    for (client, count) in &clients {
        summary.push_str(&format!("  {count:6}  {client}\n"));
    }
    summary.push_str("by client and kind:\n");
    for (pair, count) in &pairs {
        summary.push_str(&format!("  {count:6}  {pair}\n"));
    }
    let wake_label = |row: &Value| row["cause"].as_str().unwrap_or("?").to_owned();
    let wakes = delta(
        &counts(&report_after, "reconciler_wakes", wake_label),
        &counts(&report_before, "reconciler_wakes", wake_label),
    );
    summary.push_str("reconciler wakes by cause:\n");
    for (cause, count) in &wakes {
        summary.push_str(&format!("  {count:6}  {cause}\n"));
    }
    summary.push_str("requests and tasks by kind:\n");
    for (kind, count) in &kinds {
        summary.push_str(&format!("  {count:6}  {kind}\n"));
    }
    eprintln!("{summary}");

    let request_budget = SEAT_REQUESTS_PER_SECOND * SEATS as f64 * elapsed;
    assert!(
        (requests as f64) <= request_budget,
        "idle seats made {requests} requests, more than the budget of {request_budget:.0}\n{summary}"
    );
    assert!(
        passes <= PASS_BUDGET,
        "the reconciler ran {passes} passes while nothing changed\n{summary}"
    );
    assert!(
        cpu <= CPU_BUDGET,
        "the idle daemon used {:.1}% of a core, more than {:.0}%\n{summary}",
        cpu * 100.0,
        CPU_BUDGET * 100.0
    );
}
