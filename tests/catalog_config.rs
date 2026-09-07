//! `<catalog>/catalog.kdl` — the catalog's declaration of its own session registry.
//!
//! Hermetic: every command runs with `CATALOG`/`ST_ROOT`/`PTY_ROOT` removed and `HOME`/
//! `XDG_STATE_HOME` pointed at temp dirs, so nothing here can resolve a real catalog or registry.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn st2(args: &[&str], home: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(args)
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env_remove("CATALOG")
        .env_remove("ST_ROOT")
        .env_remove("PTY_ROOT")
        .output()
        .unwrap()
}

fn write_agent(catalog: &Path, host: &str, identity: &str) {
    let dir = catalog.join("agents").join(host).join(identity);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("agent.kdl"),
        format!("agent \"{identity}\" {{ host \"{host}\"; command \"true\" }}\n"),
    )
    .unwrap();
}

/// The declared root reaches the bus env every reader resolves from the catalog — no ambient
/// `PTY_ROOT` needed, and no reader left looking under `<catalog>/pty` for sessions that are not
/// there.
#[test]
fn a_declared_pty_root_replaces_the_catalog_default_in_the_bus_env() {
    let tmp = tempfile::tempdir().unwrap();
    let shared = tmp.path().join("shared-registry");
    let declared = tmp.path().join("declared");
    let plain = tmp.path().join("plain");
    write_agent(&declared, "h", "seat");
    write_agent(&plain, "h", "seat");
    fs::write(
        declared.join("catalog.kdl"),
        format!("catalog {{\n  pty-root \"{}\"\n}}\n", shared.display()),
    )
    .unwrap();

    let out = st2(
        &["env", "--catalog", declared.to_str().unwrap()],
        tmp.path(),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = String::from_utf8_lossy(&out.stdout);
    assert!(
        env.contains(&format!("export PTY_ROOT={}\n", shared.display())),
        "declared root missing from the bus env:\n{env}"
    );
    assert!(
        env.contains(&format!(
            "export ST_ROOT={}\n",
            declared.canonicalize().unwrap().display()
        )),
        "the flat native ST_ROOT must be unaffected:\n{env}"
    );

    // Control: a catalog that declares nothing keeps the native `<catalog>/pty`.
    let out = st2(&["env", "--catalog", plain.to_str().unwrap()], tmp.path());
    let env = String::from_utf8_lossy(&out.stdout);
    assert!(
        env.contains(&format!(
            "export PTY_ROOT={}/pty\n",
            plain.canonicalize().unwrap().display()
        )),
        "an undeclared catalog must be unchanged:\n{env}"
    );
}

/// `up`/`down` treat a folder holding exactly one parseable top-level spec as a single-file team.
/// `catalog.kdl` is a top-level `*.kdl`, so this pins that a declaring catalog is still a catalog:
/// `catalog` is not a spec node, and `--materialize-only` is refused on the single-file-spec path.
#[test]
fn declaring_a_root_does_not_turn_the_catalog_into_a_single_file_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    write_agent(&catalog, "h", "seat");
    fs::write(
        catalog.join("catalog.kdl"),
        "catalog { pty-root \"/run/agents/pty\" }\n",
    )
    .unwrap();

    let out = st2(
        &[
            "up",
            "--catalog",
            catalog.to_str().unwrap(),
            "--host",
            "h",
            "--materialize-only",
        ],
        tmp.path(),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "catalog dispatch broke:\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !stderr.contains("single-file specs"),
        "the declaration was mistaken for a team spec:\n{stderr}"
    );
}

#[test]
fn catalog_profile_and_agent_nodes_share_catalog_kdl_without_hiding_the_agent() {
    let catalog = tempfile::tempdir().unwrap();
    fs::write(
        catalog.path().join("catalog.kdl"),
        r#"catalog { pty-root "pty" }
profile "dev.example.goal" { wasm "resolver.wasm"; class "immediate" }
agent "worker" { host "hetz"; command "true" }
"#,
    )
    .unwrap();

    let discovered = st2::discover_strict(catalog.path());
    assert!(discovered.errors.is_empty(), "{:?}", discovered.errors);
    assert!(discovered.warnings.is_empty(), "{:?}", discovered.warnings);
    assert_eq!(discovered.specs.len(), 1);
    assert_eq!(discovered.specs[0].identity, "worker");
    assert_eq!(discovered.specs[0].host.as_deref(), Some("hetz"));
    assert!(
        discovered.declarations[0]
            .parse
            .as_ref()
            .is_some_and(agent_spec::DeclaredParse::is_valid),
        "the shared catalog envelope must retain a valid agent declaration parse"
    );

    let config = st2::catalog::load(catalog.path()).expect("catalog and profile nodes parse");
    assert_eq!(config.pty_root.as_deref(), Some("pty"));
    assert_eq!(config.profiles.len(), 1);
    assert_eq!(config.profiles[0].scheme, "dev.example.goal");
    assert_eq!(config.profiles[0].wasm, "resolver.wasm");
}

/// A mistyped field resolves back to `<catalog>/pty`, which is the split registry this declaration
/// exists to prevent — so it fails the gate instead of degrading quietly.
#[test]
fn a_mistyped_declaration_fails_validate() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    write_agent(&catalog, "h", "seat");
    fs::write(
        catalog.join("catalog.kdl"),
        "catalog { pty_root \"/run/agents/pty\" }\n",
    )
    .unwrap();

    let out = st2(
        &["validate", "--catalog", catalog.to_str().unwrap()],
        tmp.path(),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "validate passed a typo:\n{stdout}");
    assert!(
        stdout.contains("unknown catalog field 'pty_root'"),
        "no issue naming the typo:\n{stdout}"
    );

    // The machine-matchable code a renderer's build gate keys on.
    let out = st2(
        &["validate", "--json", "--catalog", catalog.to_str().unwrap()],
        tmp.path(),
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["issues"][0]["code"], "catalog-config");
    assert_eq!(report["issues"][0]["severity"], "error");
}

/// `archive-after` decides when the supervisor archives a retired seat, so a value it cannot parse
/// must fail the gate rather than silently fall back to the seven-day default.
#[test]
fn a_malformed_archive_after_fails_validate() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = tmp.path().join("catalog");
    write_agent(&catalog, "h", "seat");
    fs::write(
        catalog.join("catalog.kdl"),
        "catalog { archive-after \"7 fortnights\" }\n",
    )
    .unwrap();

    let out = st2(
        &["validate", "--catalog", catalog.to_str().unwrap()],
        tmp.path(),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "validate passed an unparseable grace period:\n{stdout}"
    );
    assert!(
        stdout.contains("archive-after: unknown duration unit"),
        "no issue naming the bad duration:\n{stdout}"
    );
}

#[test]
fn a_declared_archive_after_validates_in_every_accepted_form() {
    for declared in ["\"7d\"", "\"12h\"", "\"0\"", "\"90\""] {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        write_agent(&catalog, "h", "seat");
        fs::write(
            catalog.join("catalog.kdl"),
            format!("catalog {{ pty-root \"/run/agents/pty\"; archive-after {declared} }}\n"),
        )
        .unwrap();

        let out = st2(
            &["validate", "--catalog", catalog.to_str().unwrap()],
            tmp.path(),
        );
        assert!(
            out.status.success(),
            "archive-after {declared} was rejected:\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}
