//! `st2 service` CLI wiring + the legacy `st2 ding` alias for `st2 ping`.
//!
//! We deliberately do NOT exercise a real `service install` here: it would write a real
//! `st2.service` systemd-user unit and enable+start it on the test host. So these tests cover the
//! CLI surface and the pre-systemctl guard (a missing catalog fails before any `systemctl` runs);
//! the unit-rendering logic itself is exhaustively covered by `src/service.rs` unit tests.

fn st2() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_st2"))
}

#[test]
fn install_rejects_a_missing_catalog_before_touching_systemd() {
    // canonicalize() is the first thing install() does, so this errors out before any systemctl call.
    let out = st2()
        .args([
            "service",
            "install",
            "/definitely/not/a/real/catalog/st2test",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "should fail on a missing catalog");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("does not exist"),
        "expected a missing-catalog error, got: {err}"
    );
}

#[test]
fn render_unit_uses_invoked_executable_and_residency_without_installing() {
    let sandbox = tempfile::tempdir().unwrap();
    let catalog = sandbox.path().join("catalog");
    let config_home = sandbox.path().join("config");
    std::fs::create_dir(&catalog).unwrap();

    let out = st2()
        .args(["service", "render-unit"])
        .arg(&catalog)
        .args([
            "--residency-idle-after",
            "5m",
            "--residency-warm-capacity",
            "2",
        ])
        .env("XDG_CONFIG_HOME", &config_home)
        .env("PATH", sandbox.path().join("empty-bin"))
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "render-unit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_st2")).unwrap();
    let catalog = catalog.canonicalize().unwrap();
    assert!(
        stdout.contains(&format!(
            "ExecStart={} up --catalog {} --residency-idle-after 300000ms \
             --residency-warm-capacity 2",
            exe.display(),
            catalog.display()
        )),
        "{stdout}"
    );
    assert!(
        !config_home.join("systemd/user/st2.service").exists(),
        "render-unit must not write the systemd unit"
    );
}

#[test]
fn service_exposes_install_render_status_uninstall() {
    let out = st2().args(["service", "--help"]).output().unwrap();
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("install"), "{help}");
    assert!(help.contains("render-unit"), "{help}");
    assert!(help.contains("status"), "{help}");
    assert!(help.contains("uninstall"), "{help}");
}

#[test]
fn install_exposes_a_machine_local_pty_root() {
    let out = st2()
        .args(["service", "install", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--pty-root"), "{help}");
    assert!(help.contains("legacy runner"), "{help}");
}

#[test]
fn ping_is_canonical_and_ding_remains_an_alias() {
    // `st2 ping` is the canonical command and its help describes the inbox watch.
    let help = st2().args(["ping", "--help"]).output().unwrap();
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(
        text.contains("inbox"),
        "ping --help should describe the notification watcher; got: {text}"
    );

    // And a bare `st2 ping` dispatches into the notification handler (past clap). Catalog selection now has
    // an XDG default, so the next required runtime input is the acting identity.
    let state = tempfile::tempdir().unwrap();
    let out = st2()
        .arg("ping")
        .env("XDG_STATE_HOME", state.path())
        .env_remove("CATALOG")
        .env_remove("ST_AGENT")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("acting identity"),
        "ping should reach the notification handler; got: {err}"
    );
    assert!(
        !err.to_lowercase().contains("unrecognized"),
        "ping should be recognized; got: {err}"
    );

    let legacy = st2().args(["ding", "--help"]).output().unwrap();
    assert!(
        legacy.status.success(),
        "legacy ding alias should remain valid"
    );
    let legacy_help = String::from_utf8_lossy(&legacy.stdout);
    assert!(legacy_help.contains("inbox"), "{legacy_help}");
}
