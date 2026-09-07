//! `st2 validate` — the contract-check a second renderer (Johannes's nix → catalog) uses to confirm
//! it hit the spec. Each test builds a minimal catalog exercising one failure mode and asserts the
//! exact issue code + severity; a clean catalog (and our shipped `examples/`) must validate spotless.

use st2::validate::{Report, Severity, validate, validate_for_host};

/// Write a set of `(relative-path, body)` files into a fresh temp catalog.
fn catalog(files: &[(&str, &str)]) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    for (rel, body) in files {
        let p = d.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    d
}

fn has(r: &Report, code: &str, sev: Severity) -> bool {
    r.issues.iter().any(|i| i.code == code && i.severity == sev)
}

// ---- clean cases -----------------------------------------------------------------------------

#[test]
fn a_well_formed_service_catalog_is_clean() {
    let c = catalog(&[(
        "hetz/worker/agent.kdl",
        r#"agent "worker" { host "hetz"; type "service"; pty "agent" { command "claude" } }"#,
    )]);
    let r = validate(c.path());
    assert_eq!(r.errors(), 0, "unexpected issues: {:?}", r.issues);
    assert_eq!(r.warnings(), 0, "unexpected warnings: {:?}", r.issues);
    assert_eq!(r.agents, 1);
}

#[test]
fn compact_agent_catalog_is_clean() {
    let c = catalog(&[(
        "Silber/cos/agent.kdl",
        r#"agent "cos" {
  host "Silber"
  env { ST_AGENT "Silber.cos" }
  command "codex"
  ding
}"#,
    )]);
    let r = validate(c.path());
    assert_eq!(r.errors(), 0, "unexpected issues: {:?}", r.issues);
    assert_eq!(r.warnings(), 0, "unexpected warnings: {:?}", r.issues);
}
#[test]
fn native_session_driver_is_clean_with_explicit_readiness() {
    let c = catalog(&[(
        "h/worker/agent.kdl",
        r#"agent "worker" {
  host "h"
  argv "axe" "agent" "launch"
  session-driver "claude"
  delivery-readiness "credential" account-id="tokengate/shared"
}"#,
    )]);

    let report = validate(c.path());
    assert_eq!(report.errors(), 0, "unexpected issues: {:?}", report.issues);
    assert_eq!(report.agents, 1);
}

#[test]
fn native_delivery_requires_explicit_matching_driver_and_readiness() {
    for (name, declaration, code) in [
        (
            "legacy-deliver",
            r#"agent "legacy-deliver" { host "h"; command "codex"; deliver "app-server" }"#,
            "native-driver-missing",
        ),
        (
            "missing-readiness",
            r#"agent "missing-readiness" { host "h"; argv "axe"; session-driver "codex" }"#,
            "delivery-readiness-missing",
        ),
    ] {
        let c = catalog(&[(&format!("h/{name}/agent.kdl"), declaration)]);
        let report = validate(c.path());
        assert!(
            has(&report, code, Severity::Error),
            "{name}: {:?}",
            report.issues
        );
    }
}

#[test]
fn explicit_and_typed_session_drivers_reject_ding() {
    for (name, body) in [
        (
            "opaque",
            r#"argv "axe" "agent" "launch"; session-driver "claude"; ding"#,
        ),
        ("typed", r#"claude { prompt "Start work." }; ding"#),
    ] {
        let c = catalog(&[(
            &format!("h/{name}/agent.kdl"),
            &format!(r#"agent "{name}" {{ host "h"; {body} }}"#),
        )]);
        let report = validate(c.path());
        assert!(
            has(&report, "parse-error", Severity::Error),
            "{name}: {:?}",
            report.issues
        );
    }
}


#[test]
fn adjacent_non_agent_kdl_is_not_subject_to_agent_shape_policy() {
    let c = catalog(&[
        (
            "hetz/worker/agent.kdl",
            r#"agent "worker" { host "hetz"; command "true" }"#,
        ),
        (
            "themes/layout.kdl",
            r#"layout { pane "sidebar"; pane "main" }"#,
        ),
    ]);

    let report = validate(c.path());
    assert_eq!(report.errors(), 0, "unexpected issues: {:?}", report.issues);
}

#[test]
fn opaque_resource_bindings_are_structurally_valid() {
    let c = catalog(&[(
        "Silber/cos/agent.kdl",
        r#"agent "cos" {
  host "Silber"
  resource "work" uri="vendor+thing://authority/exact%20identity" reason="example vendor work item"
  command "codex"
}"#,
    )]);
    let r = validate(c.path());
    assert_eq!(r.errors(), 0, "unexpected issues: {:?}", r.issues);
    assert_eq!(r.warnings(), 0, "unexpected warnings: {:?}", r.issues);
}

#[test]
fn active_agents_may_share_an_opaque_resource_uri() {
    let c = catalog(&[
        (
            "h/reviewer/agent.kdl",
            r#"agent "reviewer" {
  host "h"
  resource "subject" uri="git-commit://github.com/example/project/0123456789abcdef" reason="reviewed example commit"
  command "true"
}"#,
        ),
        (
            "h/integrator/agent.kdl",
            r#"agent "integrator" {
  host "h"
  supervisor "h.reviewer"
  resource "subject" uri="git-commit://github.com/example/project/0123456789abcdef" reason="reviewed example commit"
  command "true"
}"#,
        ),
    ]);

    let r = validate(c.path());
    assert_eq!(r.errors(), 0, "unexpected issues: {:?}", r.issues);
    assert_eq!(r.agents, 2);
}

#[test]
fn duplicate_bus_ids_remain_an_error_when_resources_are_shared() {
    let c = catalog(&[
        (
            "h/one/agent.kdl",
            r#"agent "worker" {
  host "h"
  resource "subject" uri="git-commit://github.com/example/project/0123456789abcdef" reason="reviewed example commit"
  command "true"
}"#,
        ),
        (
            "h/two/agent.kdl",
            r#"agent "worker" {
  host "h"
  resource "subject" uri="git-commit://github.com/example/project/0123456789abcdef" reason="reviewed example commit"
  command "true"
}"#,
        ),
    ]);

    let r = validate(c.path());
    assert!(has(&r, "dup-id", Severity::Error), "{:?}", r.issues);
}

#[test]
fn an_invalid_resource_binding_is_a_parse_error() {
    let c = catalog(&[(
        "Silber/cos/agent.kdl",
        r#"agent "cos" {
  host "Silber"
  resource "work" uri="not-an-absolute-uri"
  command "codex"
}"#,
    )]);
    assert!(has(&validate(c.path()), "parse-error", Severity::Error));
}

#[test]
fn shared_workspace_render_conflict_is_an_error() {
    let c = tempfile::tempdir().unwrap();
    let workspace = c.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(c.path().join("_templates")).unwrap();
    std::fs::write(c.path().join("_templates/a"), "a\n").unwrap();
    std::fs::write(c.path().join("_templates/b"), "b\n").unwrap();
    for (identity, template) in [("a", "a"), ("b", "b")] {
        let path = c.path().join(format!("h/{identity}/agent.kdl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!(
                "agent \"{identity}\" {{ host \"h\"; workspace \"{}\"; command \"true\"; render {{ copy \"_templates/{template}\" \".st2/PERSONA.md\" }} }}",
                workspace.display()
            ),
        )
        .unwrap();
    }

    let r = validate_for_host(c.path(), "h");

    assert!(
        has(&r, "render-owner-conflict", Severity::Error),
        "{:?}",
        r.issues
    );
}

// ---- errors ----------------------------------------------------------------------------------

#[test]
fn a_driver_block_and_deliver_are_two_conflicting_launch_sources() {
    // #399 moved the conflict into the parser: a delivery transport pins its session driver,
    // so `deliver "app-server"` refuses a `claude` driver block at discovery time.
    let c = catalog(&[(
        "h/worker/agent.kdl",
        r#"agent "worker" {
  host "h"
  deliver "app-server"
  claude { prompt "Start work." }
  argv "claude" "Start work."
}"#,
    )]);

    let report = validate(c.path());
    let issue = report
        .issues
        .iter()
        .find(|issue| issue.code == "parse-error")
        .unwrap();
    assert_eq!(issue.severity, Severity::Error);
    assert_eq!(
        issue.message,
        "agent 'worker' delivery transport 'app-server' requires session-driver 'codex', not 'claude'"
    );
    assert_eq!(report.agents, 0, "the declaration never lowers to a spec");
}

#[test]
fn type_batch_is_retired_and_flagged_unknown() {
    // `type = batch` is retired (native `st2 eval` replaces it) — a lingering batch spec is now an
    // unknown type, not a silently-accepted service.
    let c = catalog(&[(
        "demo/eval/agent.kdl",
        r#"agent "eval" { host "demo"; type "batch"; pty "agent" { command "x" } }"#,
    )]);
    assert!(has(&validate(c.path()), "unknown-type", Severity::Error));
}

#[test]
fn unknown_type_is_an_error() {
    let c = catalog(&[(
        "hetz/w/agent.kdl",
        r#"agent "w" { host "hetz"; type "srvice"; pty "agent" { command "x" } }"#,
    )]);
    assert!(has(&validate(c.path()), "unknown-type", Severity::Error));
}

#[test]
fn a_spec_with_no_identity_is_an_error() {
    // Generic `agent.kdl` at the catalog root: no folder to borrow an identity from, none in content.
    let c = catalog(&[("agent.kdl", r#"agent { type "service" }"#)]);
    assert!(has(&validate(c.path()), "no-identity", Severity::Error));
}

#[test]
fn a_malformed_file_is_a_parse_error() {
    let c = catalog(&[("hetz/w/agent.kdl", r#"agent "w" { host "hetz""#)]);
    assert!(has(&validate(c.path()), "parse-error", Severity::Error));
}

#[test]
fn a_relative_path_is_an_error() {
    let c = catalog(&[(
        "hetz/w/agent.kdl",
        r#"agent "w" { host "hetz"; type "service"; workspace "some/rel/dir"; pty "agent" { command "x" } }"#,
    )]);
    assert!(has(&validate(c.path()), "bad-path", Severity::Error));
}

#[test]
fn canonical_relative_workspace_and_task_cwd_are_clean() {
    let c = catalog(&[
        (
            "hetz/w/agent.kdl",
            r#"agent "w" {
  host "hetz"
  workspace ".workspace"
  pty "agent" { cwd ".workspace"; command "x" }
}"#,
        ),
        ("hetz/w/.workspace/.keep", ""),
    ]);
    assert!(!has(&validate(c.path()), "bad-path", Severity::Error));
}

#[test]
fn normalized_relative_workspace_is_clean_but_indeterminate_relative_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    for (identity, workspace) in [
        ("dotted", "./.workspace"),
        ("indeterminate", "$ST2_TEST_UNSET_WORKSPACE"),
    ] {
        let bundle = temp.path().join(format!("agents/host/{identity}"));
        std::fs::create_dir_all(bundle.join(".workspace")).unwrap();
        std::fs::write(
            bundle.join("agent.kdl"),
            format!(
                "agent \"{identity}\" {{\n  host \"host\"\n  workspace \"{workspace}\"\n  argv \"true\"\n}}\n"
            ),
        )
        .unwrap();
    }

    let report = validate(temp.path());
    let bad_paths = report
        .issues
        .iter()
        .filter(|issue| issue.code == "bad-path")
        .collect::<Vec<_>>();
    assert_eq!(bad_paths.len(), 1, "{:#?}", report.issues);
    assert!(
        bad_paths
            .iter()
            .all(|issue| issue.message.contains("unresolved environment variable"))
    );
}

#[test]
fn environment_expanded_canonical_relative_workspace_is_clean() {
    let c = catalog(&[
        (
            "hetz/w/agent.kdl",
            r#"agent "w" { host "hetz"; workspace "$ST2_TEST_WORKSPACE"; argv "true" }"#,
        ),
        ("hetz/w/.workspace/.keep", ""),
    ]);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("--catalog")
        .arg(c.path())
        .args(["validate", "--json"])
        .env("ST2_TEST_WORKSPACE", ".workspace")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn a_missing_catalog_rooted_path_is_an_error() {
    // The renderer's own output — its absence is a real render bug.
    let c = catalog(&[(
        "hetz/w/agent.kdl",
        r#"agent "w" { host "hetz"; type "service"; workspace "$CATALOG/not-emitted"; pty "agent" { command "x" } }"#,
    )]);
    assert!(has(&validate(c.path()), "bad-path", Severity::Error));
}

#[test]
fn a_missing_external_path_is_only_a_warning() {
    // The workspace repo may not be checked out on the host running validate (≠ the run host) — a
    // nix build gate legitimately validates a catalog whose workspace lives elsewhere.
    let c = catalog(&[(
        "hetz/w/agent.kdl",
        r#"agent "w" { host "hetz"; type "service"; workspace "/no/such/dir/xyz123"; pty "agent" { command "x" } }"#,
    )]);
    let r = validate(c.path());
    assert!(has(&r, "bad-path", Severity::Warn));
    assert_eq!(
        r.errors(),
        0,
        "a missing external workspace must not be an error: {:?}",
        r.issues
    );
}

#[test]
fn a_remote_hosts_missing_external_path_is_not_a_local_warning() {
    let c = catalog(&[(
        "hetz/w/agent.kdl",
        r#"agent "w" { host "hetz"; type "service"; workspace "/no/such/dir/xyz123"; pty "agent" { command "x" } }"#,
    )]);
    let r = validate_for_host(c.path(), "Silber");
    assert_eq!(r.errors(), 0, "unexpected errors: {:?}", r.issues);
    assert_eq!(
        r.warnings(),
        0,
        "remote-host filesystem facts must not dirty local strict validation: {:?}",
        r.issues
    );
}

#[test]
fn an_environment_expanded_absolute_path_is_checked_normally() {
    let c = catalog(&[(
        "hetz/w/agent.kdl",
        r#"agent "w" { host "hetz"; type "service"; workspace "$HOME/repo"; pty "agent" { command "x" } }"#,
    )]);
    assert!(!has(&validate(c.path()), "bad-path", Severity::Error));
}

#[test]
fn a_catalog_rooted_path_that_exists_is_clean() {
    let c = catalog(&[
        (
            "hetz/w/agent.kdl",
            r#"agent "w" { host "hetz"; type "service"; workspace "$CATALOG/repo"; pty "agent" { command "x" } }"#,
        ),
        ("repo/.keep", ""),
    ]);
    assert!(!has(&validate(c.path()), "bad-path", Severity::Error));
}

#[test]
fn a_duplicate_bus_id_is_an_error() {
    let c = catalog(&[
        (
            "hetz/one/agent.kdl",
            r#"agent "twin" { host "hetz"; type "service"; pty "agent" { command "a" } }"#,
        ),
        (
            "hetz/two/agent.kdl",
            r#"agent "twin" { host "hetz"; type "service"; pty "agent" { command "b" } }"#,
        ),
    ]);
    assert!(has(&validate(c.path()), "dup-id", Severity::Error));
}

#[test]
fn explicit_identity_and_host_are_path_independent_but_still_unique() {
    let c = catalog(&[(
        "organization/.managed/archive/arbitrary/declaration/agent.kdl",
        r#"agent "stable" { host "pinned"; command "x" }"#,
    )]);
    let r = validate(c.path());
    assert_eq!(r.errors(), 0, "unexpected errors: {:?}", r.issues);
    assert_eq!(r.warnings(), 0, "unexpected warnings: {:?}", r.issues);
    assert_eq!(r.agents, 1);
}

#[test]
fn an_unrendered_service_is_not_runnable() {
    let c = catalog(&[(
        "hetz/w/agent.kdl",
        r#"agent "w" { host "hetz"; type "service" }"#,
    )]);
    assert!(has(&validate(c.path()), "not-runnable", Severity::Error));
}

#[test]
fn fleet_validation_compiles_remote_driver_launches() {
    let c = catalog(&[(
        "Silber/worker/agent.kdl",
        r#"agent "worker" {
  host "Silber"
  workspace "/tmp"
  claude { prompt "boot" }
  delivery-readiness "credential" account-id="tokengate/shared"
}"#,
    )]);
    let found = st2::discover(c.path());
    assert!(!found.specs[0].is_runnable());

    for report in [validate(c.path()), validate_for_host(c.path(), "droppy")] {
        assert_eq!(report.errors(), 0, "unexpected issues: {:?}", report.issues);
        assert_eq!(
            report.warnings(),
            0,
            "unexpected issues: {:?}",
            report.issues
        );
    }
}
#[test]
fn validation_reports_shared_task_compiler_errors() {
    let c = catalog(&[(
        "h/worker/agent.kdl",
        r#"agent "worker" {
  host "h"
  workspace "/tmp"
  command "codex"
  deliver "app-server"
}"#,
    )]);

    assert!(has(
        &validate_for_host(c.path(), "h"),
        "launch-compile-error",
        Severity::Error
    ));
}

#[test]
fn a_generated_ding_sidecar_is_not_authored_runnable_work() {
    let c = catalog(&[("hetz/w/agent.kdl", r#"agent "w" { host "hetz"; ding }"#)]);
    assert!(has(&validate(c.path()), "not-runnable", Severity::Error));
}

#[test]
fn ls_marks_a_generated_ding_only_agent_as_unrendered() {
    let c = catalog(&[("hetz/w/agent.kdl", r#"agent "w" { host "hetz"; ding }"#)]);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("--catalog")
        .arg(c.path())
        .arg("ls")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("[UNRENDERED: no task launch]"),
        "stdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn ls_compiles_driver_launches_before_display() {
    let c = catalog(&[(
        "h/worker/agent.kdl",
        r#"agent "worker" { host "h"; claude { prompt "boot" } }"#,
    )]);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("--catalog")
        .arg(c.path())
        .arg("ls")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("UNRENDERED"), "{stdout}");
    assert!(
        stdout.contains(r#""driver", "claude-session""#)
            && stdout.contains(r#""--", "claude", "boot"]"#),
        "{stdout}"
    );
}

#[test]
fn a_missing_render_source_is_an_error() {
    let workspace = tempfile::tempdir().unwrap();
    let agent = format!(
        r#"agent "w" {{
  host "hetz"
  workspace "{}"
  command "x"
  render {{ copy "_templates/missing.md" "AGENTS.md" }}
}}"#,
        workspace.path().display()
    );
    let c = catalog(&[("hetz/w/agent.kdl", &agent)]);
    assert!(has(&validate(c.path()), "render-error", Severity::Error));
}

#[test]
fn a_non_boolean_render_executable_property_is_an_error() {
    let workspace = tempfile::tempdir().unwrap();
    let agent = format!(
        r#"agent "w" {{
  host "hetz"
  workspace "{}"
  command "x"
  render {{ file "tool" "content" executable="yes" }}
}}"#,
        workspace.path().display()
    );
    let c = catalog(&[("hetz/w/agent.kdl", &agent)]);
    let report = validate(c.path());
    let issue = report
        .issues
        .iter()
        .find(|issue| issue.code == "render-error")
        .expect("invalid executable property must fail render validation");
    assert_eq!(issue.severity, Severity::Error);
    assert!(issue.message.contains("must be a boolean"), "{issue:?}");
}

#[test]
fn a_nameless_task_is_an_error() {
    // A `pty` block with no name is silently dropped by the parser — the task never runs.
    let c = catalog(&[(
        "hetz/w/agent.kdl",
        r#"agent "w" { host "hetz"; type "service"; pty { command "claude" } }"#,
    )]);
    assert!(has(
        &validate(c.path()),
        "unknown-task-kind",
        Severity::Error
    ));
}

#[test]
fn the_future_schedule_preview_is_explicitly_rejected() {
    let c = catalog(&[(
        "hetz/w/agent.kdl",
        r#"agent "w" {
  host "hetz"
  command "x"
  schedule "local-health" {
    every "2h"
    ding "Run the local health check."
  }
}"#,
    )]);
    assert!(has(
        &validate(c.path()),
        "unsupported-schedule",
        Severity::Error
    ));
}

// ---- warnings --------------------------------------------------------------------------------

#[test]
fn a_dangling_supervisor_is_an_error() {
    // #399 made the supervisor chain authoritative: a reference to a missing parent is a
    // structural error, not a warning.
    let c = catalog(&[
        (
            "hetz/base/agent.kdl",
            r#"agent "base" { host "hetz"; command "x" }"#,
        ),
        (
            "hetz/w/agent.kdl",
            r#"agent "w" { host "hetz"; type "service"; supervisor "ghost"; pty "agent" { command "x" } }"#,
        ),
    ]);
    let r = validate(c.path());
    assert!(has(&r, "supervisor-missing", Severity::Error));
    assert_eq!(r.errors(), 1, "unexpected issues: {:?}", r.issues);
}

#[test]
fn a_fully_qualified_supervisor_in_the_catalog_is_clean() {
    let c = catalog(&[
        (
            "Silber/cos/agent.kdl",
            r#"agent "cos" { host "Silber"; command "x" }"#,
        ),
        (
            "hetz/base/agent.kdl",
            r#"agent "base" { host "hetz"; command "x" }"#,
        ),
        (
            "hetz/w/agent.kdl",
            r#"agent "w" { host "hetz"; supervisor "Silber.cos"; command "x" }"#,
        ),
    ]);
    let r = validate(c.path());
    assert_eq!(r.errors(), 0, "unexpected errors: {:?}", r.issues);
    assert_eq!(
        r.warnings(),
        0,
        "runtime-routable qualified supervisors must validate: {:?}",
        r.issues
    );
}

#[test]
fn retired_roots_do_not_hold_the_root_slot() {
    // #402: legacy `retired #true` (and new-style retirement) removes a declaration from the org
    // chart, so root-shaped tombstones must not break the one-root-per-host invariant.
    let c = catalog(&[
        (
            "h/cos/agent.kdl",
            r#"agent "cos" { host "h"; command "x" }"#,
        ),
        (
            "h/old-legacy/agent.kdl",
            r#"agent "old-legacy" { host "h"; retired #true; command "x" }"#,
        ),
        (
            "h/old-explicit/agent.kdl",
            r#"agent "old-explicit" { host "h"; desired-state "retired" reason="Replaced by cos"; command "x" }"#,
        ),
    ]);
    let r = validate(c.path());
    assert_eq!(r.errors(), 0, "unexpected errors: {:?}", r.issues);
}

#[test]
fn a_suspended_root_still_holds_the_root_slot() {
    // Suspension is a pause inside the org, not an exit: a suspended root keeps the host at
    // exactly one root, and a second root-shaped declaration is still an error.
    let suspended_only = catalog(&[(
        "h/root/agent.kdl",
        r#"agent "root" { host "h"; desired-state "suspended" reason="maintenance window"; command "x" }"#,
    )]);
    assert_eq!(
        validate(suspended_only.path()).errors(),
        0,
        "a suspended root alone must satisfy the invariant"
    );

    let with_rival = catalog(&[
        (
            "h/root/agent.kdl",
            r#"agent "root" { host "h"; desired-state "suspended" reason="maintenance window"; command "x" }"#,
        ),
        (
            "h/rival/agent.kdl",
            r#"agent "rival" { host "h"; retired #true; command "x" }"#,
        ),
    ]);
    assert_eq!(
        validate(with_rival.path()).errors(),

        0,
        "a retired tombstone must not rival a suspended root"
    );
}
#[test]
fn an_active_chain_may_not_terminate_at_a_retired_root() {
    // One counted root satisfies root-count, but the worker's tree is headed by a retired
    // tombstone: the active org chart must descend from the counted root (#405 review).
    let c = catalog(&[
        (
            "h/live/agent.kdl",
            r#"agent "live" { host "h"; command "x" }"#,
        ),
        (
            "h/dead/agent.kdl",
            r#"agent "dead" { host "h"; retired #true; command "x" }"#,
        ),
        (
            "h/worker/agent.kdl",
            r#"agent "worker" { host "h"; supervisor "h.dead"; command "x" }"#,
        ),
    ]);
    let r = validate(c.path());
    assert!(
        r.issues.iter().any(|i| i.code == "retired-root"
            && i.severity == Severity::Error
            && i.message.contains("h.worker")
            && i.message.contains("h.dead")),
        "expected retired-root on h.worker, got {:?}",
        r.issues
    );
    assert_eq!(r.errors(), 1, "unexpected issues: {:?}", r.issues);

    // Retired descendants of a retired root stay legal: tombstone trees are outside the
    // org chart and the check only binds active agents.
    let tombstones = catalog(&[
        (
            "h/live/agent.kdl",
            r#"agent "live" { host "h"; command "x" }"#,
        ),
        (
            "h/dead/agent.kdl",
            r#"agent "dead" { host "h"; retired #true; command "x" }"#,
        ),
        (
            "h/ghost-worker/agent.kdl",
            r#"agent "ghost-worker" { host "h"; supervisor "h.dead"; retired #true; command "x" }"#,
        ),
    ]);
    assert_eq!(
        validate(tombstones.path()).errors(),
        0,
        "retired chains must not error: {:?}",
        validate(tombstones.path()).issues
    );
}

#[test]
fn a_host_without_one_counted_root_still_fails() {
    // Both zero shapes remain faults: a tombstone-only host, and a headless host whose only
    // root is retired while workers stay active.
    for (name, files) in [
        (
            "tombstone-only",
            vec![(
                "h/ghost/agent.kdl",
                r#"agent "ghost" { host "h"; retired #true; command "x" }"#,
            )],
        ),
        (
            "headless",
            vec![
                (
                    "h/root/agent.kdl",
                    r#"agent "root" { host "h"; retired #true; command "x" }"#,
                ),
                (
                    "h/worker/agent.kdl",
                    r#"agent "worker" { host "h"; supervisor "h.root"; command "x" }"#,
                ),
            ],
        ),
    ] {
        let c = catalog(&files);
        let r = validate(c.path());
        assert!(
            r.issues.iter().any(|i| i.code == "root-count"
                && i.severity == Severity::Error
                && i.message.ends_with("found 0")),
            "{name}: expected root-count found 0, got {:?}",
            r.issues
        );
    }
}

#[test]
fn an_identity_folder_mismatch_is_a_warning() {
    let c = catalog(&[(
        "hetz/folder-name/agent.kdl",
        r#"agent "content-name" { type "service"; pty "agent" { command "x" } }"#,
    )]);
    assert!(has(&validate(c.path()), "id-path-mismatch", Severity::Warn));
}

#[test]
fn a_host_folder_mismatch_is_a_warning() {
    let c = catalog(&[(
        "folderhost/w/agent.kdl",
        r#"agent { host "confighost"; type "service"; pty "agent" { command "x" } }"#,
    )]);
    assert!(has(
        &validate(c.path()),
        "host-path-mismatch",
        Severity::Warn
    ));
}

#[test]
fn a_dangling_overlay_import_is_a_warning() {
    let d = tempfile::tempdir().unwrap();
    // Workspace with the native overlay shape, but the imported persona file is missing.
    let ws = d.path().join("ws");
    std::fs::create_dir_all(ws.join(".claude/rules")).unwrap();
    std::fs::write(ws.join(".claude/rules/st2.md"), "@../../.st2/PERSONA.md\n").unwrap();
    let agent = format!(
        r#"agent "w" {{ host "hetz"; type "service"; workspace {:?}; pty "agent" {{ command "x" }} }}"#,
        ws.display()
    );
    std::fs::create_dir_all(d.path().join("hetz/w")).unwrap();
    std::fs::write(d.path().join("hetz/w/agent.kdl"), agent).unwrap();
    assert!(has(&validate(d.path()), "dangling-import", Severity::Warn));
}

// ---- CLI: exit codes, --strict, --json, native example ---------------------------------------

fn run_validate(args: &[&std::ffi::OsStr]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("validate")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn cli_exits_zero_on_a_clean_catalog() {
    let c = catalog(&[(
        "hetz/w/agent.kdl",
        r#"agent "w" { host "hetz"; type "service"; pty "agent" { command "x" } }"#,
    )]);
    let out = run_validate(&[c.path().as_os_str()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn cli_host_scope_keeps_remote_paths_out_of_strict_validation() {
    let c = catalog(&[(
        "hetz/w/agent.kdl",
        r#"agent "w" { host "hetz"; workspace "/no/such/dir/xyz123"; command "x" }"#,
    )]);
    let out = run_validate(&[
        c.path().as_os_str(),
        std::ffi::OsStr::new("--host"),
        std::ffi::OsStr::new("Silber"),
        std::ffi::OsStr::new("--strict"),
    ]);
    assert!(
        out.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn cli_exits_nonzero_on_an_error_and_strict_promotes_warnings() {
    // An error catalog exits non-zero without --strict.
    let err = catalog(&[(
        "hetz/w/agent.kdl",
        r#"agent "w" { host "hetz"; type "srvice" }"#,
    )]);
    assert!(!run_validate(&[err.path().as_os_str()]).status.success());

    // A warning-only catalog exits 0 normally, 1 under --strict. Under #399's topology rules a
    // lone agent with a mismatched folder name is still exactly one root, so the folder/identity
    // mismatch is the only finding — and it is a warning.
    let warn = catalog(&[(
        "hetz/folder-name/agent.kdl",
        r#"agent "content-name" { type "service"; pty "agent" { command "x" } }"#,
    )]);
    assert!(run_validate(&[warn.path().as_os_str()]).status.success());
    let strict = std::ffi::OsStr::new("--strict");
    assert!(
        !run_validate(&[warn.path().as_os_str(), strict])
            .status
            .success()
    );
}

#[test]
fn cli_json_is_well_formed() {
    let c = catalog(&[(
        "hetz/w/agent.kdl",
        r#"agent "w" { host "hetz"; type "srvice"; pty "agent" { command "x" } }"#,
    )]);
    let out = run_validate(&[c.path().as_os_str(), std::ffi::OsStr::new("--json")]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(v["schema"], "st2.validate.v2");
    assert_eq!(v["policyProfile"], "st2.core+catalog.v1");
    let revision = v["agentSpecRevision"]
        .as_str()
        .expect("agentSpecRevision string");
    let clean_revision = revision.len() == 40
        && revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    assert!(
        clean_revision
            || revision.starts_with("local-dirty.")
            || revision.starts_with("nix-dirty.")
            || revision == "local.unknown"
    );
    assert_eq!(v["errors"], 1);
    assert_eq!(v["issues"][0]["code"], "unknown-type");
    assert_eq!(v["issues"][0]["severity"], "error");
}

#[test]
fn a_hand_authored_native_catalog_validates_without_errors() {
    let workspace = tempfile::tempdir().unwrap();
    let catalog = tempfile::tempdir().unwrap();
    let declaration = include_str!("../examples/native/agent-codex.kdl")
        .replace("<identity>", "worker")
        .replace("<host>", "h")
        .replace("<workspace>", workspace.path().to_str().unwrap());
    let path = catalog.path().join("agents/h/worker/agent.kdl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, declaration).unwrap();
    std::fs::create_dir_all(catalog.path().join("agents/h/worker/assets")).unwrap();
    std::fs::write(
        catalog.path().join("agents/h/worker/assets/AGENTS.md"),
        "# Worker\n",
    )
    .unwrap();

    let result = validate(catalog.path());
    assert_eq!(
        result.errors(),
        0,
        "native catalog has errors: {:?}",
        result.issues
    );
}

/// A pty task whose session socket path exceeds the portable `sun_path` bound is rejected when the
/// declaration is admitted, not on every reconcile pass forever.
///
/// The identity here is long enough that no resolved pty root can bring it under the limit, so the
/// assertion does not depend on where this catalog happens to live. The byte arithmetic itself,
/// including both sides of the boundary, is covered by
/// `src/run.rs::session_socket_overage_is_derived_from_the_resolved_root`.
#[test]
fn an_unbindable_session_socket_path_is_rejected_at_admission() {
    let long_identity = "a".repeat(200);
    let c = catalog(&[(
        &format!("hetz/{long_identity}/agent.kdl"),
        &format!(r#"agent "{long_identity}" {{ host "hetz"; pty "agent" {{ command "x" }} }}"#),
    )]);

    let r = validate_for_host(c.path(), "hetz");
    assert!(
        has(&r, "socket-path-too-long", Severity::Error),
        "unbindable socket path must fail admission: {:?}",
        r.issues
    );
    let issue = r
        .issues
        .iter()
        .find(|issue| issue.code == "socket-path-too-long")
        .expect("the issue was just asserted");
    assert!(
        issue.message.contains(".sock") && issue.message.contains("exceeding the 104-byte"),
        "the diagnostic must name the resolved path and the overage: {}",
        issue.message
    );
}

/// An exec task binds no session socket, so its id is not subject to the bound.
#[test]
fn a_long_exec_task_id_is_not_a_socket_path_issue() {
    let long_identity = "a".repeat(200);
    let c = catalog(&[(
        &format!("hetz/{long_identity}/agent.kdl"),
        &format!(r#"agent "{long_identity}" {{ host "hetz"; exec "job" {{ command "true" }} }}"#),
    )]);

    let r = validate_for_host(c.path(), "hetz");
    assert!(
        !has(&r, "socket-path-too-long", Severity::Error),
        "an exec task never binds a session socket: {:?}",
        r.issues
    );
}

/// The bound comes from the pty root resolved on the host that would run the task, so a synced
/// multi-host catalog must not fail admission for another machine's task against this host's root.
#[test]
fn another_hosts_long_identity_is_not_judged_against_this_hosts_pty_root() {
    let long_identity = "a".repeat(200);
    let c = catalog(&[(
        &format!("elsewhere/{long_identity}/agent.kdl"),
        &format!(r#"agent "{long_identity}" {{ host "elsewhere"; pty "agent" {{ command "x" }} }}"#),
    )]);

    let r = validate_for_host(c.path(), "hetz");
    assert!(
        !has(&r, "socket-path-too-long", Severity::Error),
        "only the selected host's tasks are judged against its pty root: {:?}",
        r.issues
    );
}

// ---- DELTA-003 agent ID and agent address uniqueness -----------------------------------------

/// IDs are catalog-global, not per-host: two hosts cannot share one explicit `id`.
#[test]
fn two_hosts_may_not_share_one_explicit_agent_id() {
    let c = catalog(&[
        (
            "a/root/agent.kdl",
            r#"agent "root" { host "a"; id "0199b8f4-8d3a-7c21-9a44-6f85b7320ea1"; command "x" }"#,
        ),
        (
            "b/root/agent.kdl",
            r#"agent "root" { host "b"; id "0199b8f4-8d3a-7c21-9a44-6f85b7320ea1"; command "x" }"#,
        ),
    ]);

    let r = validate(c.path());
    let issue = r
        .issues
        .iter()
        .find(|i| i.code == "dup-id")
        .unwrap_or_else(|| panic!("catalog-global id collision must fail: {:?}", r.issues));
    assert_eq!(issue.severity, Severity::Error);
    assert!(
        issue
            .message
            .contains("duplicate agent id '0199b8f4-8d3a-7c21-9a44-6f85b7320ea1' (also declared in")
            && issue.message.contains("agent.kdl"),
        "the diagnostic must name the id and the other declaration: {}",
        issue.message
    );
}

/// A migrated subject's frozen ID is exactly the legacy `<host>.<identity>` bytes, so an explicit
/// `id` claiming another subject's still-unmigrated identity is the same collision.
#[test]
fn an_explicit_id_may_not_claim_another_subjects_legacy_bus_identity() {
    let c = catalog(&[
        (
            "h/root/agent.kdl",
            r#"agent "root" { host "h"; command "x" }"#,
        ),
        (
            "h/impostor/agent.kdl",
            r#"agent "impostor" { host "h"; id "h.root"; supervisor "h.root"; command "x" }"#,
        ),
    ]);

    let r = validate(c.path());
    assert!(
        r.issues.iter().any(|i| i.code == "dup-id"
            && i.severity == Severity::Error
            && i.message.contains("duplicate agent id 'h.root'")),
        "an explicit id colliding with a frozen legacy identity must fail: {:?}",
        r.issues
    );
}

/// ID and address are separate typed namespaces: equal bytes across them do not collide.
#[test]
fn an_agent_id_equal_to_another_agents_address_is_not_a_collision() {
    let c = catalog(&[
        (
            "h/root/agent.kdl",
            r#"agent "root" { host "h"; address "reviewer"; command "x" }"#,
        ),
        (
            "h/worker/agent.kdl",
            r#"agent "worker" { host "h"; id "reviewer"; supervisor "h.root"; command "x" }"#,
        ),
    ]);

    let r = validate(c.path());
    assert_eq!(
        r.errors(),
        0,
        "id and address are separate namespaces: {:?}",
        r.issues
    );
}

/// An effective address is unique inside its resolved logical host.
#[test]
fn two_explicit_addresses_may_not_collide_on_one_host() {
    let c = catalog(&[
        (
            "h/root/agent.kdl",
            r#"agent "root" { host "h"; address "reviewer"; command "x" }"#,
        ),
        (
            "h/second/agent.kdl",
            r#"agent "second" { host "h"; address "reviewer"; supervisor "h.root"; command "x" }"#,
        ),
    ]);

    let r = validate(c.path());
    let issue = r
        .issues
        .iter()
        .find(|i| i.code == "dup-address")
        .unwrap_or_else(|| panic!("host-local address collision must fail: {:?}", r.issues));
    assert_eq!(issue.severity, Severity::Error);
    assert!(
        issue
            .message
            .contains("duplicate agent address 'reviewer' on host 'h' (also declared in")
            && issue.message.contains("agent.kdl"),
        "the diagnostic must name the address, host, and other declaration: {}",
        issue.message
    );
}

/// Addresses are host-local, so the same address on two hosts is legal.
#[test]
fn the_same_address_on_two_hosts_is_legal() {
    let c = catalog(&[
        (
            "a/root/agent.kdl",
            r#"agent "root" { host "a"; address "reviewer"; command "x" }"#,
        ),
        (
            "b/root/agent.kdl",
            r#"agent "root-b" { host "b"; address "reviewer"; command "x" }"#,
        ),
    ]);

    let r = validate(c.path());
    assert_eq!(
        r.errors(),
        0,
        "one address per host is the bound, not per catalog: {:?}",
        r.issues
    );
}

/// An addressless declaration still occupies its positional identity in the address namespace.
#[test]
fn an_explicit_address_may_not_collide_with_an_addressless_identity_fallback() {
    let c = catalog(&[
        (
            "h/worker/agent.kdl",
            r#"agent "worker" { host "h"; command "x" }"#,
        ),
        (
            "h/renamed/agent.kdl",
            r#"agent "renamed" { host "h"; address "worker"; supervisor "h.worker"; command "x" }"#,
        ),
    ]);

    let r = validate(c.path());
    assert!(
        r.issues.iter().any(|i| i.code == "dup-address"
            && i.severity == Severity::Error
            && i.message
                .contains("duplicate agent address 'worker' on host 'h' (also declared in")),
        "an explicit address must collide with an identity fallback: {:?}",
        r.issues
    );
}

/// A retired subject does not resolve, so it releases its address; a suspended one keeps it.
#[test]
fn retirement_releases_an_address_but_suspension_still_occupies_it() {
    let retired = catalog(&[
        (
            "h/worker/agent.kdl",
            r#"agent "worker" { host "h"; desired-state "retired" reason="Replaced by renamed"; command "x" }"#,
        ),
        (
            "h/renamed/agent.kdl",
            r#"agent "renamed" { host "h"; address "worker"; command "x" }"#,
        ),
    ]);
    let r = validate(retired.path());
    assert!(
        !has(&r, "dup-address", Severity::Error),
        "a retired subject must release its address: {:?}",
        r.issues
    );
    assert_eq!(r.errors(), 0, "unexpected errors: {:?}", r.issues);

    let suspended = catalog(&[
        (
            "h/worker/agent.kdl",
            r#"agent "worker" { host "h"; desired-state "suspended" reason="maintenance window"; command "x" }"#,
        ),
        (
            "h/renamed/agent.kdl",
            r#"agent "renamed" { host "h"; address "worker"; supervisor "h.worker"; command "x" }"#,
        ),
    ]);
    let r = validate(suspended.path());
    assert!(
        has(&r, "dup-address", Severity::Error),
        "a suspended subject still occupies its address: {:?}",
        r.issues
    );
}

/// The legacy diagnostic text is a stable automation surface: a catalog with no `id` or `address`
/// anywhere must produce byte-identical messages to before the two fields existed.
#[test]
fn a_legacy_catalog_produces_byte_identical_duplicate_diagnostics() {
    let c = catalog(&[
        (
            "h/one/agent.kdl",
            r#"agent "worker" { host "h"; command "x" }"#,
        ),
        (
            "h/two/agent.kdl",
            r#"agent "worker" { host "h"; command "x" }"#,
        ),
    ]);

    let r = validate(c.path());
    let messages = r
        .issues
        .iter()
        .filter(|i| i.code == "dup-id")
        .map(|i| i.message.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        messages,
        vec!["duplicate agent id 'h.worker' (also declared in h/one/agent.kdl)".to_string()],
        "legacy duplicate-identity prose must not drift: {:?}",
        r.issues
    );
    assert!(
        !has(&r, "dup-address", Severity::Error),
        "one legacy duplicate is one diagnostic, not two: {:?}",
        r.issues
    );
}

/// Both duplicate rules read the same host key, so one physical subject declared twice — once
/// host-less, once with an explicit `host "h"` — is a duplicate ID under `--host h`, not a
/// conflict reported only under the address code.
#[test]
fn a_host_less_and_an_explicit_host_declaration_collide_as_one_duplicate_id() {
    let c = catalog(&[
        (
            "h/worker/agent.kdl",
            r#"agent "worker" { host "h"; command "x" }"#,
        ),
        ("worker.kdl", r#"agent "worker" { command "x" }"#),
    ]);

    let r = validate_for_host(c.path(), "h");
    assert!(
        r.issues.iter().any(|i| i.code == "dup-id"
            && i.severity == Severity::Error
            && i.message.contains("duplicate agent id 'h.worker'")),
        "one resolved subject declared twice must collide on the id key: {:?}",
        r.issues
    );
    assert!(
        !has(&r, "dup-address", Severity::Error),
        "one physical conflict is one diagnostic: {:?}",
        r.issues
    );
}

/// A declaration refused for a duplicate ID still claims its address. Otherwise a third subject
/// could take that address undetected — and `st2 agent address` admits exactly what this rule
/// admits, so the writer's gate would hand out a second claim on one route.
#[test]
fn a_declaration_refused_for_a_duplicate_id_still_claims_its_address() {
    let c = catalog(&[
        ("h/a/agent.kdl", r#"agent "x" { host "h"; command "x" }"#),
        (
            "h/b/agent.kdl",
            r#"agent "x" { host "h"; address "ops"; command "x" }"#,
        ),
        (
            "h/c/agent.kdl",
            r#"agent "c" { host "h"; address "ops"; command "x" }"#,
        ),
    ]);

    let r = validate(c.path());
    assert!(
        has(&r, "dup-id", Severity::Error),
        "the duplicate identity must still be reported: {:?}",
        r.issues
    );
    assert!(
        r.issues.iter().any(|i| i.code == "dup-address"
            && i.severity == Severity::Error
            && i.message
                .contains("duplicate agent address 'ops' on host 'h'")
            && i.message.contains("h/b/agent.kdl")),
        "the address a duplicated-id declaration holds must still collide: {:?}",
        r.issues
    );
}
