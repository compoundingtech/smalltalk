//! M2.3 integration: presence status + the agent roster over a discovered catalog. Unit mechanics
//! (state parse, staleness, atomic set/refresh) live in `src/status.rs`; this covers the composition
//! `st2 agents` relies on — enumerate specs, read each agent's status/presentation/inbox, project the
//! roster, and derive `unknown` from staleness.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use st2::agents::roster;
use st2::message::send_to_inbox;
use st2::status::{State, set_state, status_path};

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn agent_kdl(identity: &str, host: &str) -> String {
    format!(
        "agent \"{identity}\" {{\n  identity \"{identity}\"\n  host \"{host}\"\n  \
         type \"service\"\n  resource \"work\" uri=\"issue://example/{identity}\" reason=\"example work item\"\n  \
         pty \"agent\" {{ command \"exec claude boot\" }}\n}}\n"
    )
}

fn presented_agent_kdl(identity: &str, host: &str) -> String {
    agent_kdl(identity, host).replace(
        "  type \"service\"\n",
        "  type \"service\"\n  name \"st2 owner\"\n  description \"Own st2 delivery\"\n",
    )
}

fn retired_agent_kdl(identity: &str, host: &str) -> String {
    agent_kdl(identity, host).replace(
        "  type \"service\"\n",
        "  type \"service\"\n  retired #true\n",
    )
}

fn suspended_agent_kdl(identity: &str, host: &str) -> String {
    agent_kdl(identity, host).replace(
        "  type \"service\"\n",
        "  type \"service\"\n  desired-state \"suspended\" reason=\"Waiting for capacity\"\n",
    )
}

#[test]
fn roster_keeps_presence_separate_from_suspended_desired_state() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "h/worker/agent.kdl",
        &suspended_agent_kdl("worker", "h"),
    );
    set_state(&status_path(&root.join("h/worker")), State::Available).unwrap();

    let rows = roster(root, "h");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, State::Available);
    assert!(!rows[0].retired);
    assert_eq!(rows[0].desired_state, "suspended");
    assert_eq!(
        rows[0].desired_state_reason.as_deref(),
        Some("Waiting for capacity")
    );
}

/// The roster enumerates every catalog agent by bus id (sorted), projects each one's presence, and —
/// with enrich data — its inbox count and last-activity.
#[test]
fn roster_projects_presence_name_and_enrich_across_the_catalog() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "hetz/st2-claude/agent.kdl",
        &presented_agent_kdl("st2-claude", "hetz"),
    );
    write(
        root,
        "hetz/cos-claude/agent.kdl",
        &agent_kdl("cos-claude", "hetz"),
    );
    write(
        root,
        "silber/fabric-claude/agent.kdl",
        &agent_kdl("fabric-claude", "silber"),
    );

    // Presence: st2-claude busy, cos-claude available, fabric-claude unset (→ offline). Presentation
    // metadata and an inbox message belong to st2-claude's declaration.
    set_state(&status_path(&root.join("hetz/st2-claude")), State::Busy).unwrap();
    set_state(
        &status_path(&root.join("hetz/cos-claude")),
        State::Available,
    )
    .unwrap();
    send_to_inbox(
        &st2::message::inbox_dir(&root.join("hetz/st2-claude")),
        "hetz.cos-claude",
        None,
        None,
        &[],
        "hi",
    )
    .unwrap();
    let restored = send_to_inbox(
        &st2::message::inbox_dir(&root.join("hetz/st2-claude")),
        "hetz.cos-claude",
        Some("already handled"),
        None,
        &[],
        "must not count twice",
    )
    .unwrap();
    let archive = st2::message::archive_dir(&root.join("hetz/st2-claude"));
    fs::create_dir_all(&archive).unwrap();
    fs::copy(
        st2::message::inbox_dir(&root.join("hetz/st2-claude")).join(&restored),
        archive.join(&restored),
    )
    .unwrap();

    let rows = roster(root, "hetz");
    // Sorted by bus id, spanning hosts.
    let ids: Vec<&str> = rows.iter().map(|r| r.identity.as_str()).collect();
    assert_eq!(
        ids,
        ["hetz.cos-claude", "hetz.st2-claude", "silber.fabric-claude"]
    );

    let st2c = rows
        .iter()
        .find(|r| r.identity == "hetz.st2-claude")
        .unwrap();
    assert_eq!(st2c.status, State::Busy);
    assert_eq!(st2c.name.as_deref(), Some("st2 owner"));
    assert_eq!(st2c.description.as_deref(), Some("Own st2 delivery"));
    assert!(!st2c.retired);
    assert_eq!(
        st2c.inbox, 1,
        "the same-filename archive receipt suppresses the raw inbox duplicate"
    );
    assert!(
        st2c.last_activity_ms.is_some(),
        "an agent with a status + inbox has activity"
    );

    let cos = rows
        .iter()
        .find(|r| r.identity == "hetz.cos-claude")
        .unwrap();
    assert_eq!(cos.status, State::Available);
    assert_eq!(cos.name, None);
    assert!(!cos.retired);
    assert_eq!(cos.inbox, 0);

    // fabric-claude: no status file, nothing touched → offline, no activity.
    let fab = rows
        .iter()
        .find(|r| r.identity == "silber.fabric-claude")
        .unwrap();
    assert_eq!(fab.status, State::Offline);
    assert!(!fab.retired);
    assert_eq!(fab.last_activity_ms, None);
}

#[test]
fn roster_json_and_human_output_distinguish_retirement_from_presence() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "h/live/agent.kdl", &agent_kdl("live", "h"));
    write(
        root,
        "h/retired/agent.kdl",
        &retired_agent_kdl("retired", "h"),
    );
    set_state(&status_path(&root.join("h/live")), State::Available).unwrap();
    set_state(&status_path(&root.join("h/retired")), State::Busy).unwrap();

    let rows = roster(root, "h");
    assert!(
        !rows
            .iter()
            .find(|row| row.identity == "h.live")
            .unwrap()
            .retired
    );
    let retired = rows.iter().find(|row| row.identity == "h.retired").unwrap();
    assert!(retired.retired);
    assert_eq!(
        retired.status,
        State::Busy,
        "retirement is declaration state, not a replacement presence value"
    );

    let json = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("agents")
        .arg(root)
        .args(["--host", "h", "--json", "--enrich"])
        .output()
        .unwrap();
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(rows[0]["identity"], "h.live");
    assert_eq!(rows[0]["retired"], false);
    assert_eq!(
        rows[0]["resources"],
        serde_json::json!([{
            "name": "work",
            "uri": "issue://example/live",
            "reason": "example work item",
            "resync": "unsupported"
        }])
    );
    assert_eq!(rows[1]["identity"], "h.retired");
    assert_eq!(rows[1]["status"], "busy");
    assert_eq!(rows[1]["retired"], true);

    let selected = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("agents")
        .arg(root)
        .args(["--host", "h", "--identity", "h.live", "--json"])
        .output()
        .unwrap();
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    let selected: serde_json::Value = serde_json::from_slice(&selected.stdout).unwrap();
    assert_eq!(selected.as_array().unwrap().len(), 1);
    assert_eq!(selected[0]["identity"], "h.live");

    let absent = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("agents")
        .arg(root)
        .args(["--host", "h", "--identity", "h.missing", "--json"])
        .output()
        .unwrap();
    assert!(!absent.status.success());
    assert!(
        String::from_utf8_lossy(&absent.stderr)
            .contains("expected exactly one Agent Spec with identity `h.missing`, found 0")
    );

    let filtered_out = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("agents")
        .arg(root)
        .args([
            "--host",
            "h",
            "--identity",
            "h.live",
            "--status",
            "busy",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(!filtered_out.status.success());
    assert!(
        String::from_utf8_lossy(&filtered_out.stderr)
            .contains("Agent Spec `h.live` does not match status `busy`")
    );

    let human = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("agents")
        .arg(root)
        .args(["--host", "h"])
        .output()
        .unwrap();
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    assert_eq!(
        String::from_utf8(human.stdout).unwrap(),
        "h.live\tavailable\tobs:-\tctx:-\t\t\nh.retired\tbusy\tobs:-\tctx:-\t\t\t[retired]\n"
    );
}

/// DELTA-003 (R24/R25): the roster appends the immutable agent ID, the effective mutable address,
/// and the routable bus address without disturbing `identity`, which stays the positional
/// declaration key and legacy address fallback. A retired subject is non-routable: its
/// `busAddress` is null and every other field is still present.
#[test]
fn roster_json_appends_agent_id_address_and_nullable_bus_address() {
    const EXPLICIT_ID: &str = "0193b8f2-7c31-7a4e-9f11-4c2d6b8a35e7";
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "h/legacy/agent.kdl", &agent_kdl("legacy", "h"));
    write(
        root,
        "h/migrated/agent.kdl",
        &agent_kdl("migrated", "h").replace(
            "  type \"service\"\n",
            &format!(
                "  type \"service\"\n  id \"{EXPLICIT_ID}\"\n  address \"delivery-lead\"\n"
            ),
        ),
    );
    write(
        root,
        "h/retired/agent.kdl",
        &retired_agent_kdl("retired", "h"),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("agents")
        .arg(root)
        .args(["--host", "h", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let rows = rows.as_array().unwrap();
    let find = |identity: &str| {
        rows.iter()
            .find(|row| row["identity"] == identity)
            .unwrap_or_else(|| panic!("no roster row for {identity}"))
            .clone()
    };

    // A legacy declaration: the ID migration will freeze is exactly its bus identity, and the
    // effective address is its positional identity.
    let legacy = find("h.legacy");
    assert_eq!(legacy["id"], "h.legacy");
    assert_eq!(legacy["address"], "legacy");
    assert_eq!(legacy["busAddress"], "h.legacy");

    // An explicitly migrated declaration: all three new fields reflect the declared values and
    // `identity` is untouched — it is not the agent ID.
    let migrated = find("h.migrated");
    assert_eq!(migrated["identity"], "h.migrated");
    assert_eq!(migrated["id"], EXPLICIT_ID);
    assert_eq!(migrated["address"], "delivery-lead");
    assert_eq!(migrated["busAddress"], "h.delivery-lead");

    // A retired subject releases its address: `busAddress` is null, nothing else goes missing.
    let retired = find("h.retired");
    assert_eq!(retired["busAddress"], serde_json::Value::Null);
    assert_eq!(retired["id"], "h.retired");
    assert_eq!(retired["address"], "retired");
    assert_eq!(retired["retired"], true);
    assert_eq!(retired["status"], "offline");
    assert_eq!(retired["desiredState"], "retired");
    for field in [
        "identity",
        "status",
        "name",
        "description",
        "retired",
        "resources",
        "desiredState",
        "desiredStateReason",
        "observedState",
        "driverDiagnostic",
        "context",
        "id",
        "address",
        "busAddress",
    ] {
        assert!(
            retired.get(field).is_some(),
            "retired row lost `{field}`: {retired}"
        );
    }
}

#[test]
fn exact_identity_rejects_duplicates_before_status_filtering() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "declarations/one/agent.kdl",
        &agent_kdl("worker", "h"),
    );
    write(
        root,
        "declarations/two/agent.kdl",
        &agent_kdl("worker", "h"),
    );
    set_state(&status_path(&root.join("declarations/one")), State::Busy).unwrap();
    set_state(
        &status_path(&root.join("declarations/two")),
        State::Available,
    )
    .unwrap();

    let selected = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("agents")
        .arg(root)
        .args([
            "--host",
            "h",
            "--identity",
            "h.worker",
            "--status",
            "busy",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(!selected.status.success());
    assert!(
        String::from_utf8_lossy(&selected.stderr)
            .contains("expected exactly one Agent Spec with identity `h.worker`, found 2")
    );
}

#[test]
fn exact_identity_rejects_incomplete_catalog_discovery() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "valid/worker.kdl", &agent_kdl("worker", "h"));
    write(root, "h/worker/agent.kdl", "this is malformed KDL {");

    let selected = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("agents")
        .arg(root)
        .args(["--host", "h", "--identity", "h.worker", "--json"])
        .output()
        .unwrap();
    assert!(!selected.status.success());
    let stderr = String::from_utf8_lossy(&selected.stderr);
    assert!(
        stderr.contains("cannot select an exact Agent Spec while catalog discovery has 1 error")
    );
    assert!(stderr.contains("h/worker/agent.kdl"));

    let ordinary = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("agents")
        .arg(root)
        .args(["--host", "h", "--json"])
        .output()
        .unwrap();
    assert!(ordinary.status.success());
    let rows: serde_json::Value = serde_json::from_slice(&ordinary.stdout).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn exact_identity_rejects_incomplete_catalog_traversal() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let unreadable = root.join("unreadable");
    write(root, "valid/worker.kdl", &agent_kdl("worker", "h"));
    write(root, "unreadable/hidden.kdl", &agent_kdl("worker", "h"));
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&unreadable).is_ok() {
        // Root or CAP_DAC_OVERRIDE can bypass mode 000, so this environment cannot induce the
        // traversal failure the test is intended to exercise.
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }

    let selected = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("agents")
        .arg(root)
        .args(["--host", "h", "--identity", "h.worker", "--json"])
        .output()
        .unwrap();

    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!selected.status.success());
    let stderr = String::from_utf8_lossy(&selected.stderr);
    assert!(
        stderr.contains("cannot select an exact Agent Spec while catalog discovery has 1 error")
    );
    assert!(stderr.contains("unreadable"));
    assert!(stderr.contains("catalog directory traversal failed"));
}

#[test]
fn exact_identity_selects_one_of_multiple_agent_specs_in_one_declaration() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "h/pair.kdl",
        r#"
agent "one" {
  host "h"
  name "First Agent Spec"
  pty "agent" { command "true" }
}
agent "two" {
  host "h"
  name "Second Agent Spec"
  pty "agent" { command "true" }
}
"#,
    );

    let selected = Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("agents")
        .arg(root)
        .args(["--host", "h", "--identity", "h.two", "--json"])
        .output()
        .unwrap();
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    let selected: serde_json::Value = serde_json::from_slice(&selected.stdout).unwrap();
    assert_eq!(selected.as_array().unwrap().len(), 1);
    assert_eq!(selected[0]["identity"], "h.two");
    assert_eq!(selected[0]["name"], "Second Agent Spec");
}

/// A version 1 heartbeat older than the stale window projects as `unknown` in the roster.
#[test]
fn roster_derives_unknown_from_a_stale_version_1_heartbeat() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "hetz/idle/agent.kdl", &agent_kdl("idle", "hetz"));
    let sp = status_path(&root.join("hetz/idle"));
    set_state(&sp, State::Available).unwrap();

    // Fresh → available.
    assert_eq!(roster(root, "hetz")[0].status, State::Available);

    // Backdate the embedded heartbeat past the stale window → unknown.
    let stale_ms = st2::message::now_ms()
        - u64::try_from((st2::status::STATUS_STALE + Duration::from_secs(60)).as_millis()).unwrap();
    fs::write(&sp, format!("available\nv1 {stale_ms}\n")).unwrap();
    assert_eq!(roster(root, "hetz")[0].status, State::Unknown);
}

/// HC-R14/HC-R07: a real harness-context record on disk joins the roster as a fourth axis, in
/// both JSON forms and in the human column, and it stays readable while the categorical record
/// beside it derives `unknown` — the wedge case the record exists for.
#[test]
fn roster_joins_a_real_context_record_independently_of_observed_state() {
    use st2::harness_context::{Compaction, CompactionTrigger, Harness, Reading, Writer};

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // A harness-context write stages under the catalog control plane and validates canonical
    // `<catalog>/agents/<host>/<identity>` ancestry ("Replicated-path discipline"), so this
    // fixture must be a real catalog rather than a bare host/identity pair.
    fs::create_dir_all(root.join(st2::catalog_lock::CONTROL_DIR)).unwrap();
    write(
        root,
        "agents/hetz/filling/agent.kdl",
        &agent_kdl("filling", "hetz"),
    );
    let agent_dir = root.join("agents/hetz/filling");
    set_state(&status_path(&agent_dir), State::Busy).unwrap();

    // No record: the axis is emitted as `null`, not omitted.
    assert!(roster(root, "hetz")[0].context.is_none());

    let mut writer = Writer::new(&agent_dir, "hetz.filling", Harness::Claude).unwrap();
    writer
        .observe(Reading {
            used_tokens: Some(184_000),
            window_tokens: Some(200_000),
            used_percent: Some(92.0),
            model: Some("claude-opus-5".to_string()),
            cost_usd: Some(3.5),
            rate_limits: st2::harness_context::RateLimits {
                five_hour: Some(100.0),
                seven_day: Some(55.0),
            },
            ..Reading::default()
        })
        .unwrap();
    writer
        .compacted(Compaction::new(CompactionTrigger::Auto))
        .unwrap();

    // The state record is absent, so `observedState` is null — and the numbers are still there.
    let row = &roster(root, "hetz")[0];
    assert!(row.observed.is_none());
    let context = row.context.as_ref().expect("the record joins the roster");
    assert_eq!(context.used_percent, Some(92.0));
    assert_eq!(context.compactions, 1);
    assert!(!context.stale);

    for form in [vec!["--json"], vec!["--json", "--enrich"]] {
        let out = Command::new(env!("CARGO_BIN_EXE_st2"))
            .arg("agents")
            .arg(root)
            .args(["--host", "hetz"])
            .args(&form)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let rows: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(rows[0]["context"]["harness"], "claude");
        assert_eq!(rows[0]["context"]["usedPercent"], 92.0);
        assert_eq!(rows[0]["context"]["usedTokens"], 184_000);
        assert_eq!(rows[0]["context"]["rateLimited"], true);
        assert_eq!(rows[0]["context"]["compactions"], 1);
        assert_eq!(rows[0]["context"]["lastCompactionTrigger"], "auto");
        assert_eq!(rows[0]["context"]["stale"], false);
        // Declared presence and the other axes keep their own meanings.
        assert_eq!(rows[0]["status"], "busy");
        assert_eq!(rows[0]["observedState"], serde_json::Value::Null);
    }

    let human = |root: &Path| {
        let out = Command::new(env!("CARGO_BIN_EXE_st2"))
            .arg("agents")
            .arg(root)
            .args(["--host", "hetz"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    assert_eq!(
        human(root),
        "hetz.filling\tbusy\tobs:-\tctx:92% rate-limited \u{27f3}1\t\t\n"
    );

    // A record whose percent the harness withheld — Claude before its first API response — must
    // not render like no record at all: the producer is watching and honestly does not know.
    Writer::new(&agent_dir, "hetz.filling", Harness::Claude)
        .unwrap()
        .observe(Reading {
            window_tokens: Some(200_000),
            ..Reading::default()
        })
        .unwrap();
    let row = &roster(root, "hetz")[0];
    assert!(row.context.as_ref().unwrap().used_percent.is_none());
    assert_eq!(human(root), "hetz.filling\tbusy\tobs:-\tctx:? \u{27f3}1\t\t\n");

    // …and no record at all still reads `-`.
    fs::remove_file(st2::harness_context::harness_context_path(&agent_dir)).unwrap();
    assert_eq!(human(root), "hetz.filling\tbusy\tobs:-\tctx:-\t\t\n");
}

#[test]
fn roster_uses_version_1_origin_time_for_last_activity() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "hetz/idle/agent.kdl", &agent_kdl("idle", "hetz"));
    let sp = status_path(&root.join("hetz/idle"));
    let heartbeat_ms = st2::message::now_ms() - 1_000;
    fs::write(&sp, format!("available\nv1 {heartbeat_ms}\n")).unwrap();

    let row = &roster(root, "hetz")[0];
    assert_eq!(row.status, State::Available);
    assert_eq!(row.last_activity_ms, Some(heartbeat_ms as f64));
}

#[test]
fn status_cli_writes_and_reads_the_version_1_record() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "h/worker/agent.kdl", &agent_kdl("worker", "h"));
    let before = st2::message::now_ms();

    let set = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["status", "h.worker", "--set", "busy", "--root"])
        .arg(root)
        .args(["--host", "h"])
        .output()
        .unwrap();
    assert!(
        set.status.success(),
        "{}",
        String::from_utf8_lossy(&set.stderr)
    );
    assert_eq!(String::from_utf8(set.stdout).unwrap(), "status: busy\n");

    let after = st2::message::now_ms();
    let raw = fs::read_to_string(status_path(&root.join("h/worker"))).unwrap();
    let lines: Vec<&str> = raw.lines().collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], "busy");
    let timestamp = lines[1]
        .strip_prefix("v1 ")
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!((before..=after).contains(&timestamp));
    assert!(raw.ends_with('\n'));

    let get = Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["status", "h.worker", "--root"])
        .arg(root)
        .args(["--host", "h"])
        .output()
        .unwrap();
    assert!(
        get.status.success(),
        "{}",
        String::from_utf8_lossy(&get.stderr)
    );
    assert_eq!(String::from_utf8(get.stdout).unwrap(), "busy\n");
}
