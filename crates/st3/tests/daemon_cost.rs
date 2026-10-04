//! The cost check: how much SQLite work each daemon request does on a small generated store and on
//! one ten times larger. A request whose work grows with the store fails, however fast the machine
//! that runs it. It counts, and never times, so it gives the same answer on every runner.
//!
//! ```sh
//! TMPDIR=/var/tmp cargo test -p st3 --test integration daemon_cost:: -- --nocapture --test-threads 1
//! ```
//!
//! The counts are the whole process's, so no other test may run beside it.
//!
//! Every statement reports SQLite's own counters when it finishes (`smallclaims::sqlite::work`):
//! virtual machine steps, which each row read, compared or written costs; steps through a table
//! without an index; sorts no index could give; and rows put into an index SQLite built for one
//! statement. A foreign-key check or a trigger counts inside the statement that ran it.
//!
//! Each request runs once to warm caches, then three times; the least of the three counts, so a
//! stray background statement cannot fail it. Its work at the larger scale may be at most
//! [`GROWTH`] times its work at the smaller, after dividing by how much larger its answer grew: a
//! list that answers ten times more rows may read ten times more, and a request that answers the
//! same must read about the same. Small differences under [`SLACK`] steps pass.
//!
//! It covers every route the daemon serves: each is measured or listed in [`NOT_MEASURED`] with
//! the reason, and a new route fails the check until it is one or the other. Beyond the routes it
//! measures replication receive and export as the replication worker calls them, and the deletes
//! of a checkpoint trim, per deleted row.
//!
//! - `ST_COST_SCALES` sets the two generated scales, smaller first. The default is `0.01,0.1`.
//! - `ST_BENCH_DIR` keeps generated stores for the next run, as for `daemon_bench`.
//! - `ST_COST_REPORT` writes the measurements as JSON to that path.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use smallclaims::sqlite::work::{self, SqliteWork};
use st3::api::AppState;
use st3::client::Client;
use st3::store::Store;
use tokio::sync::{Notify, watch};

use crate::daemon_bench::{
    FLEET, NODE, PEER, Subjects, claim_input, fleet_subjects, generated_stores, listed, stub_pty,
};

/// How many times more work a request may do at the larger scale, for the same answer.
const GROWTH: f64 = 3.0;

/// Work differences this small pass whatever their ratio: a few rows more or less.
const SLACK: u64 = 5_000;

/// Requests whose work already grows with the store on main, measured on 2026-10-03 at
/// 341f7ad9, each with the growth it may reach: half again its measured ratio, so it cannot get
/// much worse unnoticed. Each breaks the daemon's rule that no query's cost grows with the whole
/// store and is owed a fix. A fixed one fails the check until it leaves this list.
const KNOWN_GROWTH: &[(&str, f64)] = &[
    // Attention and the mission detail read every person ask (11.8x full-scan steps).
    ("GET /v1/attention", 18.0),
    ("GET /v1/client/missions/{*id}", 18.0),
    // Checkpoint status walks the sealed set (9.8x).
    ("GET /v1/checkpoint/status", 15.0),
    // Device inventory reads every principal claim (9.8x).
    ("GET /v1/client/devices", 15.0),
    // Runtimes read every runtime observation (3.8x for the list, 9.0x for one runtime).
    ("GET /v1/client/runtimes", 6.0),
    ("GET /v1/client/runtimes/{*id}", 14.0),
    // Usage sums every usage claim of the period (6.1x).
    ("GET /v1/client/usage", 9.5),
    ("GET /v1/usage", 9.5),
    // Doctor checks the whole store, as it must (11.4x full-scan steps).
    ("GET /v1/doctor", 17.0),
    // Fleet and replication status count every replica record (9.7x).
    ("GET /v1/internal/fleet/status", 15.0),
    ("GET /v1/replication/status", 15.0),
    // A replica record is found by scanning them all (9.8x).
    ("GET /v1/replication/records/{*record}", 15.0),
    // Outcome history reads every finished step to page them (8.3x).
    ("GET /v1/outcome-history", 12.5),
    // The repair plan's work grows faster than the store (24.7x steps, 64.6x full-scan steps).
    ("GET /v1/repair", 97.0),
    // Reviews scan every claim of their kinds (19.6x steps, 25.0x full-scan steps).
    ("GET /v1/reviews", 38.0),
    // Sessions read every harness observation (11.5x full-scan steps).
    ("GET /v1/sessions", 17.5),
];

/// Routes the check does not measure, and why. Keep this list short: a route here can grow with
/// the store unnoticed.
const NOT_MEASURED: &[(&str, &str)] = &[
    // Streams and terminals: they stay open, or need a live `pty`.
    (
        "GET /v1/mailbox",
        "requires an authenticated native driver; the load test models seat waits with event long-polls",
    ),
    (
        "GET /v1/client/conversations/{id}/stream",
        "a WebSocket stream",
    ),
    ("GET /v1/client/collections/stream", "a WebSocket stream"),
    (
        "GET /v1/client/terminals/{id}/stream",
        "a live terminal stream",
    ),
    (
        "GET /v1/client/terminals/{id}/raw-stream",
        "a live terminal stream",
    ),
    (
        "POST /v1/client/terminals/{id}/raw-attachments",
        "attaches to a live terminal",
    ),
    (
        "GET /v1/client/terminals/{id}/screen",
        "reads a live terminal",
    ),
    (
        "POST /v1/sessions/input/{*subject}",
        "types into a live terminal",
    ),
    ("GET /v1/sessions/logs/{*subject}", "reads a live terminal"),
    (
        "GET /v1/sessions/screen/{*subject}",
        "reads a live terminal",
    ),
    (
        "POST /v1/sessions/attach/{*subject}",
        "attaches to a live terminal",
    ),
    (
        "POST /v1/sessions/{subject}/attach",
        "attaches to a live terminal",
    ),
    (
        "GET /v1/sessions/local-terminal/{*subject}",
        "attaches to a live terminal",
    ),
    (
        "GET /v1/sessions/terminal/{*subject}",
        "attaches to a live terminal",
    ),
    (
        "POST /v1/sessions/{subject}/signal",
        "signals a live process",
    ),
    (
        "POST /v1/sessions/{subject}/context/clear",
        "types into a live terminal",
    ),
    // Pairing and fleet membership change who may talk to the daemon; their tests cover them.
    ("POST /v1/client/pairings", "pairs a device"),
    ("POST /v1/client/pairings/{id}/complete", "pairs a device"),
    ("POST /v1/internal/fleet/invites", "fleet membership"),
    ("POST /v1/internal/fleet/invites/revoke", "fleet membership"),
    ("POST /v1/internal/fleet/redeem", "fleet membership"),
    ("POST /v1/internal/fleet/remove", "fleet membership"),
    ("POST /v1/internal/fleet/leave/begin", "fleet membership"),
    ("POST /v1/internal/fleet/leave/cancel", "fleet membership"),
    ("POST /v1/internal/fleet/leave/claim", "fleet membership"),
    (
        "POST /v1/internal/fleet/endpoints",
        "publishes this node's endpoints",
    ),
    (
        "POST /v1/internal/replication-wake",
        "wakes the replication worker, which is not running",
    ),
    (
        "POST /v1/internal/replication/peer-failure",
        "records a transport failure; no store read",
    ),
    // Checkpoints: the trim is measured directly, per deleted row.
    (
        "POST /v1/checkpoint/plan",
        "plans a checkpoint over the whole sealed set, by design",
    ),
    ("POST /v1/checkpoint/excuse", "checkpoint agreement"),
    ("POST /v1/checkpoint/resume", "checkpoint agreement"),
    (
        "POST /v1/internal/replication/checkpoint",
        "pages a trimmed checkpoint's manifest",
    ),
    (
        "POST /v1/internal/replication/checkpoint-adopt",
        "adopts a peer's checkpoint",
    ),
    (
        "POST /v1/internal/replication/heal/answer",
        "a heal walks the store's differences",
    ),
    (
        "POST /v1/internal/replication/heal/next",
        "a heal walks the store's differences",
    ),
    // Planning sessions and evals start harnesses.
    ("POST /v1/launches", "starts a planning harness"),
    ("POST /v1/launches/{id}/submit", "planning session"),
    (
        "POST /v1/launches/{id}/variants/{variant}/submit",
        "planning session",
    ),
    ("POST /v1/launches/{id}/preview", "planning session"),
    (
        "POST /v1/launches/{id}/variants/{variant}/preview",
        "planning session",
    ),
    (
        "POST /v1/launches/{id}/variants/{variant}/propose",
        "planning session",
    ),
    ("POST /v1/launches/{id}/approve", "planning session"),
    (
        "POST /v1/launches/{id}/approve-and-launch",
        "planning session",
    ),
    ("POST /v1/launches/{id}/start", "planning session"),
    ("POST /v1/launches/{id}/decisions", "planning session"),
    (
        "POST /v1/launches/{id}/decisions/{decision}/answer",
        "planning session",
    ),
    ("POST /v1/launches/{id}/revise", "planning session"),
    ("POST /v1/launches/{id}/cancel", "planning session"),
    ("POST /v1/evals", "starts an eval's harnesses"),
    (
        "POST /v1/gate-checks",
        "runs a mission's gate commands in a workspace",
    ),
    (
        "GET /v1/gate-checks/{id}",
        "reads a gate check that POST /v1/gate-checks started",
    ),
    // Writes that need a live runtime or a person's approval the generated store lacks.
    ("POST /v1/agents/rename", "renames a declared agent"),
    ("POST /v1/agents/restart", "restarts a live seat"),
    (
        "POST /v1/agents/rollout",
        "requires a running source-fenced native seat",
    ),
    ("POST /v1/agents/start", "starts a seat"),
    ("POST /v1/agents/suspend", "suspends a live seat"),
    ("POST /v1/agents/resume", "resumes a live seat"),
    (
        "POST /v1/agents/native-session",
        "reports a live harness session",
    ),
    (
        "POST /v1/repair/apply",
        "applies an operational repair plan",
    ),
    (
        "POST /v1/replication/repair",
        "repairs one replication record",
    ),
    ("POST /v1/rules/set", "sets an authority rule"),
    (
        "POST /v1/client/actions",
        "client actions; each action's own route is measured",
    ),
    (
        "POST /v1/client/blobs",
        "uploads image bytes, outside the graph",
    ),
    (
        "GET /v1/client/blobs/{id}",
        "image bytes, outside the graph",
    ),
    (
        "GET /v1/client/blobs/{id}/chunk",
        "image bytes, outside the graph",
    ),
    (
        "PUT /v1/client/glasses/{id}",
        "a stui layout, outside the graph",
    ),
    (
        "DELETE /v1/client/glasses/{id}",
        "a stui layout, outside the graph",
    ),
    ("POST /v1/intent/mission", "plans a mission from KDL"),
    ("POST /v1/intent/apply", "applies a planned mission"),
    ("POST /v1/missions/{id}/retire", "retires a mission"),
    ("POST /v1/mission-runs/{run}/revision", "revises a run"),
    (
        "POST /v1/mission-runs/{run}/outcome",
        "sets a run's outcome",
    ),
    (
        "POST /v1/revision-proposals/{proposal}/approve",
        "approves a revision",
    ),
    (
        "POST /v1/revision-proposals/{proposal}/cancel",
        "cancels a revision",
    ),
    ("POST /v1/work/cancel-ask", "cancels a person ask"),
    (
        "POST /v1/work/mission/{*subject}",
        "publishes a step's mission",
    ),
    ("POST /v1/work/wake/{*subject}", "wakes a seat"),
    ("POST /v1/work/retry/{*subject}", "retries a failed step"),
    (
        "POST /v1/work/extend/{*subject}",
        "extends a step's deadline",
    ),
    ("POST /v1/agent-queue-moves", "reorders an agent's queue"),
    ("POST /v1/lane-changes", "changes a merge lane"),
    ("POST /v1/gate-results", "records a gate result"),
    ("POST /v1/reviews/{*subject}", "records a review"),
    (
        "POST /v1/attention",
        "refuses every post: attention is derived from asks",
    ),
    (
        "POST /v1/attention/resolve/{*subject}",
        "resolves attention",
    ),
    (
        "POST /v1/attention/withdraw/{*subject}",
        "withdraws attention",
    ),
    (
        "POST /v1/subscription-requests/{decision}/{request}",
        "decides a held request",
    ),
    ("POST /v1/delivery/hold", "holds message delivery"),
    (
        "POST /v1/mailbox/bind",
        "binds a live seat's mailbox stream",
    ),
    (
        "POST /v1/mailbox/receipts",
        "a bound mailbox stream's receipt; the same lifecycle write is measured as \
         POST /v1/messages/{message_id}/claims",
    ),
];

/// One request the check makes. `{name}` in a path stands for an item of the generated store;
/// see [`Fixture::fill`].
struct Probe {
    /// The route as `api.rs` declares it, with its method.
    route: &'static str,
    path: &'static str,
    body: Option<fn(&Fixture, usize) -> Value>,
    /// The store call the route makes, for a route that only a live seat process may call.
    direct: Option<Direct>,
}

type Direct = fn(&Store, &Fixture, usize) -> Result<Value, String>;

const fn get(route: &'static str, path: &'static str) -> Probe {
    Probe {
        route,
        path,
        body: None,
        direct: None,
    }
}

const fn post(
    route: &'static str,
    path: &'static str,
    body: fn(&Fixture, usize) -> Value,
) -> Probe {
    Probe {
        route,
        path,
        body: Some(body),
        direct: None,
    }
}

const fn direct(route: &'static str, call: Direct) -> Probe {
    Probe {
        route,
        path: "",
        body: None,
        direct: Some(call),
    }
}

const PROBES: &[Probe] = &[
    get("GET /v1/client/sets", "/v1/client/sets"),
    get(
        "GET /v1/client/sets/{*id}",
        "/v1/client/sets/bench/cost/fixture",
    ),
    post("POST /v1/sets/preview", "/v1/sets/preview", |_, attempt| {
        owned_set_request(&format!("preview-{attempt}"))
    }),
    post("POST /v1/sets/apply", "/v1/sets/apply", |_, attempt| {
        owned_set_request(&format!("apply-{attempt}"))
    }),
    // Posting and registration also require GitHub; count their local writes with invented
    // object IDs. The comment probe includes the default watch declaration.
    direct("POST /v1/github/comment", |store, fixture, attempt| {
        let thread = st3::github_watch::ThreadRef::parse(&format!("acme/garden#{}", attempt + 10))
            .map_err(|error| error.message)?;
        let agent = &fixture.subjects.seats[0];
        let id = attempt as u64 + 100;
        let mut record = store
            .record_github_post(
                agent,
                &thread,
                "comment",
                id,
                &format!(
                    "https://github.com/acme/garden/issues/{}#issuecomment-{id}",
                    thread.number
                ),
                "garden-bot",
            )
            .map_err(|error| error.message)?;
        record["watch"] = store
            .declare_watch(&thread, agent, None)
            .map_err(|error| error.message)?;
        Ok(record)
    }),
    direct("POST /v1/github/own", |store, fixture, attempt| {
        let thread =
            st3::github_watch::ThreadRef::parse("acme/garden#12").map_err(|error| error.message)?;
        let id = attempt as u64 + 200;
        store
            .record_github_post(
                &fixture.subjects.seats[0],
                &thread,
                "comment",
                id,
                &format!("https://github.com/acme/garden/issues/12#issuecomment-{id}"),
                "garden-bot",
            )
            .map_err(|error| error.message)
    }),
    // A watch validates the thread with GitHub first; measure its store work directly so the
    // cost check needs neither network access nor a live seat process. Each attempt declares a
    // new watch, and the ending probe ends a different one rather than measuring a replay.
    direct("POST /v1/github/watch", |store, fixture, attempt| {
        let thread = st3::github_watch::ThreadRef::parse(&format!("acme/garden#{}", attempt + 1))
            .map_err(|error| error.message)?;
        store
            .declare_watch(&thread, &fixture.subjects.seats[0], None)
            .map_err(|error| error.message)
    }),
    direct("GET /v1/github/watches", |store, fixture, _| {
        store
            .watches(Some(&fixture.subjects.seats[0]))
            .map(|views| json!(views))
            .map_err(|error| error.message)
    }),
    direct("POST /v1/github/unwatch", |store, fixture, attempt| {
        let thread = st3::github_watch::ThreadRef::parse(&format!("acme/garden#{}", attempt + 1))
            .map_err(|error| error.message)?;
        store
            .end_watch(&thread.watch(&fixture.subjects.seats[0]), "unwatched", None)
            .map(|ended| json!({"ended": ended}))
            .map_err(|error| error.message)
    }),
    get("GET /v1/health", "/v1/health"),
    get("GET /v1/schema", "/v1/schema"),
    get("GET /v1/client/capabilities", "/v1/client/capabilities"),
    get("GET /v1/client/glasses", "/v1/client/glasses"),
    get(
        "GET /v1/client/glasses/{id}",
        "/v1/client/glasses/0192f3a4-0000-7000-8000-000000000000",
    ),
    get(
        "GET /v1/client/request-latency",
        "/v1/client/request-latency",
    ),
    get(
        "GET /v1/client/documents/content",
        "/v1/client/documents/content?name={document_reference}",
    ),
    get("GET /v1/client/usage", "/v1/client/usage"),
    get(
        "GET /v1/client/subject-definition",
        "/v1/client/subject-definition?subject={seat}",
    ),
    get("GET /v1/client/now", "/v1/client/now"),
    get("GET /v1/client/machines", "/v1/client/machines"),
    get("GET /v1/client/hosts/{*id}", "/v1/client/hosts/local/repositories"),
    get("GET /v1/client/devices", "/v1/client/devices"),
    get("GET /v1/client/attention", "/v1/client/attention"),
    get(
        "GET /v1/client/attention/{*id}",
        "/v1/client/attention/{attention}",
    ),
    get("GET /v1/client/messages", "/v1/client/messages"),
    get(
        "GET /v1/client/messages/{*id}",
        "/v1/client/messages/{message}",
    ),
    get("GET /v1/client/launches", "/v1/client/launches"),
    get(
        "GET /v1/client/launches/{id}",
        "/v1/client/launches/{launch}",
    ),
    get(
        "GET /v1/client/launches/{id}/variants",
        "/v1/client/launches/{launch}/variants",
    ),
    get(
        "GET /v1/client/launches/{id}/variants/{variant}",
        "/v1/client/launches/{launch}/variants/main",
    ),
    get(
        "GET /v1/client/launches/{id}/decisions",
        "/v1/client/launches/{launch}/decisions",
    ),
    get(
        "GET /v1/client/launches/{id}/decisions/{decision}",
        "/v1/client/launches/{launch}/decisions/first",
    ),
    get(
        "GET /v1/client/launches/{id}/approvals",
        "/v1/client/launches/{launch}/approvals",
    ),
    get(
        "GET /v1/client/launches/{id}/approvals/{approval}",
        "/v1/client/launches/{launch}/approvals/first",
    ),
    get("GET /v1/client/work", "/v1/client/work"),
    get("GET /v1/client/work/{*id}", "/v1/client/work/{step}"),
    get("GET /v1/client/agents", "/v1/client/agents"),
    get("GET /v1/client/agents/{*id}", "/v1/client/agents/{agent}"),
    get(
        "GET /v1/client/agent-declarations/{*id}",
        "/v1/client/agent-declarations/{agent}",
    ),
    get(
        "GET /v1/client/agent-queues/{*id}",
        "/v1/client/agent-queues/{agent}",
    ),
    get("GET /v1/client/lanes", "/v1/client/lanes"),
    get("GET /v1/client/lanes/{*id}", "/v1/client/lanes/{lane}"),
    get("GET /v1/client/history", "/v1/client/history"),
    get(
        "GET /v1/client/history/{*id}",
        "/v1/client/history/{history}",
    ),
    get("GET /v1/client/sessions", "/v1/client/sessions"),
    get(
        "GET /v1/client/sessions/{*id}",
        "/v1/client/sessions/{session}",
    ),
    get(
        "GET /v1/client/conversations/{id}/changes",
        "/v1/client/conversations/{session}/changes",
    ),
    get(
        "GET /v1/client/conversations/search",
        "/v1/client/conversations/search?text=invented&limit=20",
    ),
    get("GET /v1/client/missions", "/v1/client/missions"),
    get("GET /v1/client/missions-tree", "/v1/client/missions-tree"),
    get(
        "GET /v1/client/missions/{*id}",
        "/v1/client/missions/{mission}",
    ),
    get("GET /v1/client/resources", "/v1/client/resources"),
    get("GET /v1/client/runtimes", "/v1/client/runtimes"),
    get(
        "GET /v1/client/runtimes/{*id}",
        "/v1/client/runtimes/{runtime}",
    ),
    get("GET /v1/client/observers", "/v1/client/observers"),
    get(
        "GET /v1/client/observers/{*id}",
        "/v1/client/observers/{observer}",
    ),
    get("GET /v1/client/subscriptions", "/v1/client/subscriptions"),
    get(
        "GET /v1/client/subscriptions/{*id}",
        "/v1/client/subscriptions/{subscription}",
    ),
    get("GET /v1/client/terminals", "/v1/client/terminals"),
    get("GET /v1/client/operations", "/v1/client/operations"),
    get(
        "GET /v1/client/operations/{*id}",
        "/v1/client/operations/{operation}",
    ),
    get("GET /v1/client/events", "/v1/client/events"),
    get("GET /v1/missions/{id}", "/v1/missions/{mission_name}"),
    get("GET /v1/launches/{id}", "/v1/launches/{launch}"),
    get(
        "GET /v1/launches/{id}/variants/{left}/compare/{right}",
        "/v1/launches/{launch}/variants/main/compare/other",
    ),
    get("GET /v1/documents", "/v1/documents"),
    get(
        "GET /v1/documents/content",
        "/v1/documents/content?reference={document_reference}",
    ),
    get("GET /v1/rules", "/v1/rules"),
    get("GET /v1/rules/audit", "/v1/rules/audit"),
    get("GET /v1/delivery/hold", "/v1/delivery/hold?subject={seat}"),
    get("GET /v1/claims", "/v1/claims?limit=100"),
    get("GET /v1/claims/by-id/{id}", "/v1/claims/by-id/{claim}"),
    get("GET /v1/usage", "/v1/usage"),
    get("GET /v1/reviews", "/v1/reviews"),
    get("GET /v1/attention", "/v1/attention"),
    get(
        "GET /v1/subscription-requests",
        "/v1/subscription-requests?subscription=subscription/bench/missing",
    ),
    get("GET /v1/messages", "/v1/messages?to={seat}"),
    get(
        "GET /v1/messages/by-key",
        "/v1/messages/by-key?key=cost-fixture-message",
    ),
    get(
        "GET /v1/messages/page",
        "/v1/messages/page?include_closed=false&limit=100&to={seat}",
    ),
    get(
        "GET /v1/messages/read/{*subject}",
        "/v1/messages/read/{message}",
    ),
    get(
        "GET /v1/messages/delivery/{*subject}",
        "/v1/messages/delivery/{message}",
    ),
    get("GET /v1/status", "/v1/status?subject={seat}"),
    get("GET /v1/desired/{*subject}", "/v1/desired/{seat}"),
    get("GET /v1/events", "/v1/events?limit=100"),
    // The route streams JSON Lines rather than the JSON document the HTTP probe expects.
    // Measure the same paged exporter directly, normalizing work by archive bytes.
    direct("GET /v1/backup", |store, _, _| {
        let mut archive = Vec::new();
        store
            .write_backup(&mut archive)
            .map_err(|error| error.to_string())?;
        String::from_utf8(archive)
            .map(Value::String)
            .map_err(|error| error.to_string())
    }),
    get("GET /v1/doctor", "/v1/doctor"),
    get("GET /v1/repair", "/v1/repair"),
    get("GET /v1/replication/status", "/v1/replication/status"),
    get("GET /v1/replication/records", "/v1/replication/records"),
    get(
        "GET /v1/replication/records/{*record}",
        "/v1/replication/records/{record}",
    ),
    get("GET /v1/checkpoint/status", "/v1/checkpoint/status"),
    get(
        "GET /v1/internal/fleet/membership",
        "/v1/internal/fleet/membership",
    ),
    get("GET /v1/internal/fleet/status", "/v1/internal/fleet/status"),
    get(
        "GET /v1/internal/fleet/invites",
        "/v1/internal/fleet/invites",
    ),
    get("GET /v1/evals/{*run}", "/v1/evals/{run}"),
    get(
        "GET /v1/mission-runs",
        "/v1/mission-runs?mission={mission_name}",
    ),
    get(
        "GET /v1/mission-overview",
        "/v1/mission-overview?mission={mission_name}",
    ),
    get(
        "GET /v1/outcome-history",
        "/v1/outcome-history?collection=work&limit=50",
    ),
    get("GET /v1/performance", "/v1/performance"),
    get(
        "GET /v1/mission-runs/{run}/generations",
        "/v1/mission-runs/{run}/generations",
    ),
    get(
        "GET /v1/mission-runs/{run}/revision-proposal",
        "/v1/mission-runs/{run}/revision-proposal",
    ),
    get("GET /v1/mission-runs/{run}", "/v1/mission-runs/{run}"),
    get(
        "GET /v1/run-generations/{generation}",
        "/v1/run-generations/{generation}",
    ),
    get(
        "GET /v1/revision-proposals/{proposal}",
        "/v1/revision-proposals/bench-proposal",
    ),
    get("GET /v1/work", "/v1/work?actor={seat}"),
    get("GET /v1/work-items/{*subject}", "/v1/work-items/{step}"),
    get("GET /v1/lanes", "/v1/lanes"),
    get("GET /v1/lanes/{*lane}", "/v1/lanes/{lane}"),
    get("GET /v1/sessions", "/v1/sessions"),
    get(
        "GET /v1/hosts/{host}/agent-workspace",
        "/v1/hosts/bench-host/agent-workspace?identity=bench/seat-0",
    ),
    // Writes, each with a new idempotency key.
    post("POST /v1/claims", "/v1/claims", |fixture, attempt| {
        let seat = &fixture.subjects.seats[0];
        json!({
            "subject": seat,
            "kind": "harness.observed",
            "actor": seat,
            "fields": {"state": "working", "incarnation_id": format!("seat-0-{attempt}")},
            "evidence": [],
            "idempotency_key": format!("cost-heartbeat-{attempt}"),
        })
    }),
    post("POST /v1/messages", "/v1/messages", |fixture, attempt| {
        json!({
            "idempotency_key": format!("cost-message-{attempt}"),
            "from": fixture.subjects.seats[0],
            "to": fixture.subjects.seats[1],
            "title": "An invented status note",
            "content": "The invented build finished; the invented review can start.",
        })
    }),
    post(
        "POST /v1/messages/{message_id}/claims",
        "/v1/messages/{sent}/claims",
        |fixture, attempt| {
            json!({
                "lifecycle": "delivered",
                "actor": fixture.subjects.seats[1],
                "evidence": [],
                "idempotency_key": format!("cost-delivered-{attempt}"),
            })
        },
    ),
    // The route renews only for a live harness incarnation, which a generated seat has none of;
    // the renewal is the same write.
    direct(
        "POST /v1/work/{action}/{*subject}",
        |store, fixture, attempt| {
            let (agent, step, incarnation) = &fixture.subjects.held[0];
            let request = st3::model::WorkRequest {
                actor: Some(agent.clone()),
                incarnation: Some(incarnation.clone()),
                summary: None,
                reason: None,
                evidence: Vec::new(),
                idempotency_key: format!("cost-renew-{attempt}"),
            };
            store
                .work_action(step, "renew", &request)
                .map(|view| serde_json::to_value(view).unwrap())
                .map_err(|error| error.message)
        },
    ),
    post("POST /v1/work/ask", "/v1/work/ask", |fixture, attempt| {
        json!({
            "person": "person/bench-operator",
            "title": format!("Invented cost question {attempt}"),
            "reason": "An invented decision needs a person.",
            "actor": fixture.items["asker"],
            "new_run": format!("cost-question-{attempt}"),
            "idempotency_key": format!("cost-ask-{attempt}"),
        })
    }),
    post("POST /v1/work/done", "/v1/work/done", |fixture, attempt| {
        json!({
            "subject": fixture.asks.get(attempt).cloned().unwrap_or_default(),
            "actor": "person/bench-operator",
            "summary": "The invented decision is made",
            "evidence": [],
            "idempotency_key": format!("cost-done-{attempt}"),
        })
    }),
    post("POST /v1/documents", "/v1/documents", |_, attempt| {
        json!({
            "name": format!("doc/bench/cost/report-{attempt}"),
            "bytes": format!("# Invented report, revision {attempt}\n\nAn invented finding.\n").into_bytes(),
            "idempotency_key": format!("cost-document-{attempt}"),
        })
    }),
    post(
        "POST /v1/diagnostics/harness",
        "/v1/diagnostics/harness",
        |fixture, attempt| {
            json!({
                "actor": fixture.subjects.seats[0],
                "code": "invented-diagnostic",
                "reason": format!("An invented diagnostic {attempt}"),
                "severity": "warning",
                "status": "open",
                "idempotency_key": format!("cost-diagnostic-{attempt}"),
            })
        },
    ),
    // The route binds its caller to a live seat process, which a test has none of.
    direct("POST /v1/harness-events", |store, fixture, attempt| {
        let seat = &fixture.subjects.seats[0];
        let mut claim = claim_input(
            "harness.timeline",
            &format!("cost-harness-event-{attempt}"),
            attempt,
            "",
        );
        claim.subject = seat.clone();
        claim.actor = Some(seat.clone());
        claim
            .fields
            .insert("sequence".into(), json!(attempt as u64 + 1));
        claim
            .fields
            .insert("incarnation_id".into(), json!(SEAT_RUNTIME));
        let publication = st3::harness_events::Publication {
            runtime_incarnation: SEAT_RUNTIME.into(),
            sequence: attempt as u64 + 1,
            claim,
        };
        store
            .append_harness_event(&publication)
            .map(|(record, _)| serde_json::to_value(record).unwrap())
            .map_err(|error| error.message)
    }),
    // Replication, as the worker calls it: an in-step peer asks for a summary, and each side
    // answers the other's summary with what it lacks, here a few new claims.
    post(
        "POST /v1/internal/replication/export",
        "/v1/internal/replication/export",
        |fixture, _| {
            json!({
                "fleet_id": FLEET,
                "inventory": fixture.peer_inventory,
                "summary_only": false,
            })
        },
    ),
    post(
        "POST /v1/internal/replication/receive",
        "/v1/internal/replication/receive",
        |fixture, _| {
            json!({
                "peer": PEER,
                "fleet_id": FLEET,
                "exchange": fixture.peer_exchange,
                "round_trip_ms": 5,
            })
        },
    ),
    post(
        "POST /v1/internal/replication/checkpoint-need",
        "/v1/internal/replication/checkpoint-need",
        |_, _| json!({}),
    ),
];

/// Each write publishes a fresh, fenced member rather than measuring an idempotent retry.
fn owned_set_request(name: &str) -> Value {
    let subject = format!("agent/bench/cost/{name}");
    json!({
        "intent":{"kdl":format!("version 2\nagent \"bench/cost/{name}\" {{ command \"true\" }}\n")},
        "options":{
            "set":format!("bench/cost/{name}"),
            "source":{"repository":"acme/garden","ref":"refs/heads/main","sha":format!("{:040x}",1),"sequence":1},
            "expected_set":"absent",
            "expected_subjects":{subject:[]},
        },
        "actor":"person/bench-operator",
        "idempotency_key":format!("cost-owned-set-{name}"),
    })
}

/// The running runtime of the first seat, whose driver publishes the harness events.
const SEAT_RUNTIME: &str = "cost-seat-0-runtime";

/// The replication summary a peer asks for each exchange; a route of its own above would answer
/// the same request.
const SUMMARY: &str = "POST /v1/internal/replication/export (summary)";

/// What one request did at one scale.
#[derive(Clone, Debug, Default, serde::Serialize)]
struct Cost {
    vm_steps: u64,
    fullscan_steps: u64,
    sorts: u64,
    autoindex_rows: u64,
    statements: u64,
    /// The answer's size in bytes, or the rows a trim deleted.
    answer: u64,
    error: Option<String>,
}

impl Cost {
    fn least(samples: &[Cost]) -> Cost {
        let least = |field: fn(&Cost) -> u64| samples.iter().map(field).min().unwrap_or(0);
        Cost {
            vm_steps: least(|cost| cost.vm_steps),
            fullscan_steps: least(|cost| cost.fullscan_steps),
            sorts: least(|cost| cost.sorts),
            autoindex_rows: least(|cost| cost.autoindex_rows),
            statements: least(|cost| cost.statements),
            answer: samples.iter().map(|cost| cost.answer).max().unwrap_or(0),
            error: samples.iter().find_map(|cost| cost.error.clone()),
        }
    }

    fn from_work(work: SqliteWork, answer: u64, error: Option<String>) -> Cost {
        Cost {
            vm_steps: work.vm_steps,
            fullscan_steps: work.fullscan_steps,
            sorts: work.sorts,
            autoindex_rows: work.autoindex_rows,
            statements: work.statements,
            answer,
            error,
        }
    }
}

/// Items of the generated store the probes refer to.
struct Fixture {
    subjects: Subjects,
    items: BTreeMap<&'static str, String>,
    /// A message sent for the lifecycle writes.
    sent: String,
    /// Person asks the done probe answers, one per attempt.
    asks: Vec<String>,
    peer_inventory: Value,
    peer_exchange: Value,
}

impl Fixture {
    fn fill(&self, path: &str) -> String {
        let mut path = path
            .replace("{seat}", &urlencoding::encode(&self.subjects.seats[0]))
            .replace(
                "{sent}",
                &urlencoding::encode(self.sent.trim_start_matches("message/")),
            )
            .replace(
                "{held_step}",
                self.subjects.held.first().map_or("", |(_, step, _)| step),
            );
        for (name, value) in &self.items {
            path = path.replace(&format!("{{{name}}}"), value);
        }
        path
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_request_does_work_that_grows_with_the_store() {
    let scales = std::env::var("ST_COST_SCALES")
        .unwrap_or_else(|_| "0.01,0.1".into())
        .split(',')
        .map(|scale| {
            scale
                .trim()
                .parse::<f64>()
                .expect("ST_COST_SCALES lists numbers")
        })
        .collect::<Vec<_>>();
    assert_eq!(scales.len(), 2, "ST_COST_SCALES names two scales");
    let keep = std::env::var_os("ST_BENCH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/st-bench"));
    std::fs::create_dir_all(&keep).unwrap();

    let started = Instant::now();
    // Both scales generate at once, and only then is anything counted: the counts are the whole
    // process's.
    let stores = tokio::join!(
        generated_stores(&keep, scales[0]),
        generated_stores(&keep, scales[1])
    );
    println!(
        "generated or found in {:.0}s",
        started.elapsed().as_secs_f64()
    );
    let mut measured = Vec::new();
    for (scale, (store, peer)) in scales.iter().zip([stores.0, stores.1]) {
        measured.push(measure(*scale, &store, &peer).await);
        println!(
            "scale {scale}: measured at {:.0}s",
            started.elapsed().as_secs_f64()
        );
    }
    let (small, large) = (&measured[0], &measured[1]);

    let mut failures = Vec::new();
    let mut rows = Vec::new();
    for (name, before) in &small.costs {
        let Some(after) = large.costs.get(name) else {
            continue;
        };
        if let Some(error) = after.error.as_ref().or(before.error.as_ref()) {
            failures.push(format!("{name}: failed: {error}"));
            continue;
        }
        // A list that answers more rows may read more; a request answering the same may not.
        let answered = (after.answer.max(1) as f64 / before.answer.max(1) as f64).max(1.0);
        let grew = |field: fn(&Cost) -> u64| {
            let (before, after) = (field(before), field(after));
            let ratio = after as f64 / before.max(1) as f64 / answered;
            (ratio, ratio > GROWTH && after > before + SLACK)
        };
        let (steps, steps_grew) = grew(|cost| cost.vm_steps);
        let (scans, scans_grew) = grew(|cost| cost.fullscan_steps);
        let summary = format!(
            "{} -> {} VM steps, {} -> {} full-scan steps, answer {} -> {} bytes",
            before.vm_steps,
            after.vm_steps,
            before.fullscan_steps,
            after.fullscan_steps,
            before.answer,
            after.answer
        );
        match KNOWN_GROWTH.iter().find(|(route, _)| route == name) {
            Some((_, limit)) if steps.max(scans) > *limit => failures.push(format!(
                "{name}: work grows more than its known {limit}x: {summary}"
            )),
            Some(_) if !steps_grew && !scans_grew => failures.push(format!(
                "{name}: no longer grows with the store; remove it from KNOWN_GROWTH: {summary}"
            )),
            Some(_) => {}
            None if steps_grew || scans_grew => {
                failures.push(format!("{name}: work grows with the store: {summary}"))
            }
            None => {}
        }
        rows.push((name.clone(), before.clone(), after.clone(), steps, scans));
    }

    println!(
        "\n{:<58} {:>10} {:>10} {:>6} {:>9} {:>9} {:>6}",
        "request", "steps", "steps 10x", "ratio", "scans", "scans 10x", "ratio"
    );
    for (name, before, after, steps, scans) in &rows {
        println!(
            "{:<58} {:>10} {:>10} {:>6.2} {:>9} {:>9} {:>6.2}",
            name,
            before.vm_steps,
            after.vm_steps,
            steps,
            before.fullscan_steps,
            after.fullscan_steps,
            scans
        );
    }
    if let Some(path) = std::env::var_os("ST_COST_REPORT") {
        let report = json!({
            "scales": scales,
            "claims": [small.claims, large.claims],
            "growth_limit": GROWTH,
            "slack": SLACK,
            "small": small.costs,
            "large": large.costs,
            "failures": failures,
        });
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    assert!(
        failures.is_empty(),
        "{} requests do work that grows with the store ({} claims vs {}):\n{}",
        failures.len(),
        small.claims,
        large.claims,
        failures.join("\n")
    );
}

/// Every route `api.rs` declares is measured or listed in `NOT_MEASURED`, and nothing is both.
#[test]
fn the_cost_check_covers_every_route() {
    let declared = declared_routes(include_str!("../src/api.rs"));
    assert!(declared.len() > 100, "found only {} routes", declared.len());
    let measured = PROBES
        .iter()
        .map(|probe| probe.route)
        .collect::<BTreeSet<_>>();
    let skipped = NOT_MEASURED
        .iter()
        .map(|(route, _)| *route)
        .collect::<BTreeSet<_>>();
    let unmeasured_known = KNOWN_GROWTH
        .iter()
        .filter(|(route, _)| !measured.contains(route))
        .collect::<Vec<_>>();
    assert!(
        unmeasured_known.is_empty(),
        "known growth on routes no probe measures: {unmeasured_known:?}"
    );
    let both = measured.intersection(&skipped).collect::<Vec<_>>();
    assert!(
        both.is_empty(),
        "measured and listed as not measured: {both:?}"
    );
    let unknown = measured
        .union(&skipped)
        .filter(|route| !declared.contains(**route))
        .collect::<Vec<_>>();
    assert!(
        unknown.is_empty(),
        "routes api.rs no longer declares: {unknown:?}"
    );
    let uncovered = declared
        .iter()
        .filter(|route| !measured.contains(route.as_str()) && !skipped.contains(route.as_str()))
        .collect::<Vec<_>>();
    assert!(
        uncovered.is_empty(),
        "add a probe to PROBES in tests/daemon_cost.rs, or a reason to NOT_MEASURED, for each \
         of {uncovered:?}"
    );
}

/// `METHOD /path` for every `.route(...)` in the router's source.
fn declared_routes(source: &str) -> BTreeSet<String> {
    let mut routes = BTreeSet::new();
    let router = &source[source
        .find("fn router_for_transport")
        .expect("api.rs builds its router in router_for_transport")..];
    let mut rest = router;
    while let Some(start) = rest.find(".route(") {
        rest = &rest[start + ".route(".len()..];
        // The call's arguments run to its matching parenthesis.
        let mut depth = 1;
        let end = rest
            .char_indices()
            .find_map(|(index, character)| {
                match character {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                (depth == 0).then_some(index)
            })
            .expect("a .route( call closes");
        let arguments = &rest[..end];
        rest = &rest[end..];
        let Some(path) = arguments.split('"').nth(1) else {
            continue;
        };
        let handlers = arguments.splitn(3, '"').nth(2).unwrap_or_default();
        for (method, call) in [
            ("GET", "get("),
            ("POST", "post("),
            ("PUT", "put("),
            ("DELETE", "delete("),
        ] {
            let mut search = handlers;
            while let Some(found) = search.find(call) {
                let before = search[..found].chars().last();
                if before.is_none_or(|character| !character.is_alphanumeric() && character != '_') {
                    routes.insert(format!("{method} {path}"));
                    break;
                }
                search = &search[found + call.len()..];
            }
        }
        if !routes
            .iter()
            .any(|route| route.ends_with(&format!(" {path}")))
        {
            panic!("no method found for route {path}: {arguments}");
        }
    }
    // The router's last section nests test-only routes in its own tests module.
    routes.retain(|route| {
        !route.contains("/v1/client/probes") && !route.contains("/v1/client/earlier")
    });
    routes
}

struct Measured {
    claims: u64,
    costs: BTreeMap<String, Cost>,
}

/// Work counted while `request` runs and until the daemon's own work for it settles.
async fn counted<F, Fut>(request: F) -> Cost
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Value, String>>,
{
    let before = work::total();
    let answer = request().await;
    // Work a request leaves to a background task belongs to it too.
    let mut last = work::total();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let now = work::total();
        if now == last || Instant::now() > deadline {
            break;
        }
        last = now;
    }
    let spent = last - before;
    if std::env::var_os("ST_COST_DEBUG").is_some()
        && let Ok(value) = &answer
    {
        println!(
            "answer: {}",
            value.to_string().chars().take(400).collect::<String>()
        );
    }
    match answer {
        Ok(value) => Cost::from_work(
            spent,
            serde_json::to_vec(&value).unwrap().len() as u64,
            None,
        ),
        Err(error) => Cost::from_work(spent, 0, Some(error)),
    }
}

async fn measure(scale: f64, source: &Path, peer_source: &Path) -> Measured {
    let work_directory = tempfile::tempdir().unwrap();
    let root = work_directory.path();
    let database = root.join("state/claims.sqlite3");
    let peer_database = root.join("peer.sqlite3");
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    for (from, to) in [(source, &database), (peer_source, &peer_database)] {
        for suffix in ["", "-wal"] {
            let from = PathBuf::from(format!("{}{suffix}", from.display()));
            if from.exists() {
                std::fs::copy(&from, format!("{}{suffix}", to.display())).unwrap();
            }
        }
    }
    let store = Arc::new(Store::open(&database, NODE).unwrap());
    store.bind_fleet(FLEET).ok();
    let peer = Arc::new(Store::open(&peer_database, PEER).unwrap());
    for (daemon, name) in [(&store, NODE), (&peer, PEER)] {
        let mut started = claim_input("daemon.started", "cost-owned-set-support", 0, "");
        started.subject = format!("daemon/{name}");
        started.fields = serde_json::from_value(json!({
            "status":"running", "features":{"owned_sets":1},
        }))
        .unwrap();
        daemon.append_claim(&started).unwrap();
    }
    sync(&peer, PEER, &store);
    let claims = store.index().unwrap();

    let socket = root.join("st3.sock");
    let pty = stub_pty(root);
    let state = AppState {
        store: store.clone(),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: NODE.into(),
        state_dir: root.join("state"),
        pty_root: root.join("pty"),
        pty_binary: pty,
        fleet_id: Some(FLEET.into()),
        configured_peers: vec![PEER.into()],
        client_relay: None,
        native_session_home: Some(root.join("home")),
        planner_default: st3::model::PlannerSpec::default(),
    };
    let server_socket = socket.clone();
    let server =
        tokio::spawn(
            async move { st3::api::serve_unix(&server_socket, st3::api::router(state)).await },
        );
    while UnixStream::connect(&socket).is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let client = Client::unix(&socket);
    // Client reads come from a person, as stui and the app make them.
    let person = Client::unix_as(&socket, "person/bench-operator").unwrap();
    let subjects = {
        let store = store.clone();
        tokio::task::spawn_blocking(move || fleet_subjects(&store, 3))
            .await
            .unwrap()
    };
    {
        // The first seat runs, so its driver may publish harness events.
        let seat = subjects.seats[0].clone();
        let mut running = claim_input("runtime.observed", "cost-seat-0-running", 0, "");
        running.subject = seat.clone();
        running.actor = Some(seat);
        running.fields.insert("status".into(), json!("running"));
        running
            .fields
            .insert("incarnation_id".into(), json!(SEAT_RUNTIME));
        store.append_claim(&running).unwrap();
    }
    let mut fixture = fixture(&person, &client, subjects).await;
    client
        .post::<_, Value>("/v1/sets/apply", &owned_set_request("fixture"))
        .await
        .expect("the owned-set read fixture must publish");
    let selected: Value = person
        .get("/v1/client/sets/bench/cost/fixture")
        .await
        .expect("the owned-set detail probe must read a live fixture");
    assert_eq!(selected["receipt"]["source"]["sequence"], 1);

    let mut costs = BTreeMap::new();
    for probe in PROBES {
        let mut samples = Vec::new();
        // The first run warms statement caches and lazily built state; it is not counted.
        for attempt in 0..4 {
            if probe.route.contains("/replication/") {
                prepare_replication(&mut fixture, &store, &peer).await;
            }
            let cost = if let Some(call) = probe.direct {
                let (store, fixture) = (store.clone(), &fixture);
                counted(|| async move {
                    tokio::task::block_in_place(|| call(&store, fixture, attempt))
                })
                .await
            } else {
                let path = fixture.fill(probe.path);
                let body = probe.body.map(|body| body(&fixture, attempt));
                let client = if path.starts_with("/v1/client/") {
                    person.clone()
                } else {
                    client.clone()
                };
                counted(|| async move {
                    let answer = match &body {
                        None => client.get::<Value>(&path).await,
                        Some(body) => client.post::<_, Value>(&path, body).await,
                    };
                    answer.map_err(|error| error.to_string().chars().take(200).collect())
                })
                .await
            };
            if attempt > 0 {
                samples.push(cost);
            }
        }
        if std::env::var_os("ST_COST_DEBUG").is_some() {
            println!("{}: {samples:?}", probe.route);
        }
        let cost = Cost::least(&samples);
        // A missing item answers an error the same way at both scales; only a broken request fails.
        let cost = Cost {
            error: cost
                .error
                .filter(|error| !error.contains("not-found") && !error.contains("404")),
            ..cost
        };
        costs.insert(probe.route.to_owned(), cost);
    }
    // The summary a peer asks for before each exchange, after the daemon wrote a few claims.
    let mut samples = Vec::new();
    for attempt in 0..4 {
        {
            let store = store.clone();
            let round = ROUND.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tokio::task::spawn_blocking(move || write_claims(&store, NODE, round))
                .await
                .unwrap();
        }
        let client = client.clone();
        let cost = counted(|| async move {
            client
                .post::<_, Value>(
                    "/v1/internal/replication/export",
                    &json!({"fleet_id": FLEET, "inventory": {}, "summary_only": true}),
                )
                .await
                .map_err(|error| error.to_string())
        })
        .await;
        if attempt > 0 {
            samples.push(cost);
        }
    }
    costs.insert(SUMMARY.to_owned(), Cost::least(&samples));
    server.abort();

    // Last, as it deletes most of the store: a checkpoint trim, per row it deletes.
    costs.insert(
        "checkpoint trim, per deleted row".into(),
        trim_cost(&store).await,
    );
    println!(
        "scale {scale}: {claims} claims, {} requests measured",
        costs.len()
    );
    Measured { claims, costs }
}

/// Ids from the store's own lists, a sent message, and a person ask per attempt.
async fn fixture(person: &Client, client: &Client, subjects: Subjects) -> Fixture {
    let mut items = BTreeMap::new();
    for (name, list) in [
        ("agent", "/v1/client/agents"),
        ("mission", "/v1/client/missions"),
        ("step", "/v1/client/work"),
        ("message", "/v1/client/messages"),
        ("attention", "/v1/client/attention"),
        ("history", "/v1/client/history"),
        ("session", "/v1/client/sessions"),
        ("runtime", "/v1/client/runtimes"),
        ("observer", "/v1/client/observers"),
        ("subscription", "/v1/client/subscriptions"),
        ("operation", "/v1/client/operations"),
        ("lane", "/v1/client/lanes"),
        ("launch", "/v1/client/launches"),
    ] {
        let first = listed(person, list).await.into_iter().next();
        // Ids go in paths: escape everything a path may not hold, but keep their slashes.
        let first = first.map(|id| urlencoding::encode(&id).replace("%2F", "/"));
        items.insert(
            name,
            first.unwrap_or_else(|| format!("bench-missing-{name}")),
        );
    }
    items.insert("document", "doc/bench/host/report-0".into());
    let hash = client
        .get::<Value>("/v1/documents?name=doc/bench/host/probe")
        .await
        .ok()
        .and_then(|page| first_string(&page, "hash"))
        .unwrap_or_else(|| "bench-missing-hash".into());
    items.insert(
        "document_reference",
        urlencoding::encode(&format!("doc/bench/host/probe@{hash}")).into_owned(),
    );
    items.insert("mission_name", "bench/host-mission-0".into());
    let run = subjects.runs.first().cloned().unwrap_or_default();
    items.insert("run", run.trim_start_matches("mission-run/").to_owned());
    let generation = client
        .get::<Value>(&format!("/v1/mission-runs/{}", items["run"]))
        .await
        .ok()
        .and_then(|view| {
            view.get("generation_id")
                .or_else(|| view.get("generation"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "bench-missing-generation".into());
    items.insert(
        "generation",
        generation.trim_start_matches("run-generation/").to_owned(),
    );
    let claim = client
        .get::<Value>("/v1/claims?limit=1")
        .await
        .ok()
        .and_then(|page| first_string(&page, "id"))
        .unwrap_or_else(|| "bench-missing-claim".into());
    items.insert("claim", claim);
    let record = client
        .get::<Value>("/v1/replication/records")
        .await
        .ok()
        .and_then(|page| first_string(&page, "record").or_else(|| first_string(&page, "id")))
        .unwrap_or_else(|| "bench-missing-record".into());
    items.insert("record", urlencoding::encode(&record).into_owned());

    let sent = client
        .post::<_, st3::model::MessageSendReceipt>(
            "/v1/messages",
            &json!({
                "idempotency_key": "cost-fixture-message",
                "from": subjects.seats[0],
                "to": subjects.seats[1],
                "title": "An invented note to receive",
                "content": "An invented note.",
            }),
        )
        .await
        .expect("the fixture message must send")
        .message
        .subject;
    // A person ask needs an agent with a live declaration: a standing agent the generator did
    // not retire.
    let ask = |actor: String, attempt: usize| {
        let client = client.clone();
        async move {
            client
                .post::<_, Value>(
                    "/v1/work/ask",
                    &json!({
                        "person": "person/bench-operator",
                        "title": format!("Invented question to answer {attempt}"),
                        "reason": "An invented decision needs a person.",
                        "actor": actor,
                        "new_run": format!("cost-answer-{attempt}"),
                        "idempotency_key": format!("cost-answer-ask-{attempt}"),
                    }),
                )
                .await
                .ok()
                .and_then(|view| {
                    view.get("subject")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
        }
    };
    let mut asker = None;
    for standing in (0..500).rev() {
        let actor = format!("agent/bench/host/standing-{standing}");
        if let Some(subject) = ask(actor.clone(), 0).await {
            asker = Some((actor, subject));
            break;
        }
    }
    let (asker, first) = asker.unwrap_or_default();
    let mut asks = vec![first];
    for attempt in 1..4 {
        asks.push(ask(asker.clone(), attempt).await.unwrap_or_default());
    }
    items.insert("asker", asker);
    Fixture {
        subjects,
        items,
        sent,
        asks,
        peer_inventory: Value::Null,
        peer_exchange: Value::Null,
    }
}

fn first_string(page: &Value, field: &str) -> Option<String> {
    let items = page
        .get("value")
        .unwrap_or(page)
        .get("items")
        .or_else(|| page.get("claims"))
        .or_else(|| page.get("records"))
        .and_then(Value::as_array)
        .or_else(|| page.as_array())?;
    items
        .first()?
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Bring the peer in step with the daemon, give each a few claims the other lacks, and run the
/// replication worker's round up to the daemon's part: the daemon's summary, the peer's answer
/// listing the ranges that differ, the daemon's push answering it, and the peer's follow-up
/// carrying its own claims. The probes then make the daemon's push and receive the follow-up.
async fn prepare_replication(fixture: &mut Fixture, store: &Arc<Store>, peer: &Arc<Store>) {
    let (store, peer) = (store.clone(), peer.clone());
    let (inventory, exchange) = tokio::task::spawn_blocking(move || {
        let round = ROUND.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        sync(&store, NODE, &peer);
        sync(&peer, PEER, &store);
        write_claims(&peer, PEER, round);
        write_claims(&store, NODE, round);
        let summary = store.export_replication_summary(FLEET).unwrap();
        let answer = peer
            .export_replication_exchange_answering(FLEET, &summary.inventory, &[])
            .unwrap();
        let push = store
            .export_replication_exchange_answering(FLEET, &answer.inventory, &[])
            .unwrap();
        peer.receive_replication_exchange(NODE, FLEET, &push)
            .unwrap();
        let follow_up = peer
            .export_replication_exchange_answering(FLEET, &push.inventory, &[])
            .unwrap();
        assert!(
            !push.envelopes.is_empty() && !follow_up.envelopes.is_empty(),
            "the exchange carries no claims: {} pushed, {} followed up",
            push.envelopes.len(),
            follow_up.envelopes.len()
        );
        (
            serde_json::to_value(answer.inventory).unwrap(),
            serde_json::to_value(follow_up).unwrap(),
        )
    })
    .await
    .unwrap();
    fixture.peer_inventory = inventory;
    fixture.peer_exchange = exchange;
}

static ROUND: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Three new claims of `origin`'s, each unlike any before it.
fn write_claims(store: &Store, origin: &str, round: usize) {
    for n in 0..3 {
        let index = 1_000_000 + round * 10 + n;
        let key = format!("cost-replication-{origin}-{index}");
        store
            .append_claim(&claim_input("harness.observed", &key, index, ""))
            .unwrap();
    }
}

fn sync(from: &Store, from_name: &str, to: &Store) {
    let exchange = from
        .export_replication_exchange(
            FLEET,
            &to.export_replication_summary(FLEET).unwrap().inventory,
        )
        .unwrap();
    to.receive_replication_exchange(from_name, FLEET, &exchange)
        .unwrap();
    to.validate_replication_backlog().unwrap();
    to.apply_replication_repairs().unwrap();
    to.project_replication_backlog().unwrap();
}

/// A checkpoint trim of everything the checkpoint rules drop, counted per deleted row.
async fn trim_cost(store: &Arc<Store>) -> Cost {
    let store = store.clone();
    tokio::task::spawn_blocking(move || {
        let cut = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            + 1_000;
        let plan = st3::store::plan_drops(&store.checkpoint_sealed_set(cut).unwrap());
        let rows = (plan.claims.len() + plan.envelopes.len()) as u64;
        let before = work::total();
        let mut actions = Vec::new();
        let trimmed = store.trim_checkpoint(
            "checkpoint/cost-check",
            cut,
            &plan.drop_digest,
            &plan.envelopes,
            &plan.claims,
            false,
            &mut actions,
        );
        let spent = work::total() - before;
        // The comparison divides by the answer, here the rows deleted, so it compares the work
        // per row.
        Cost::from_work(spent, rows, trimmed.err().map(|error| error.to_string()))
    })
    .await
    .unwrap()
}
