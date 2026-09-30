//! `st2 pty` — a thin pass-through that runs `pty` with the selected catalog's bus env auto-set, so
//! pty subcommands and the interactive UI work without `eval "$(st2 env <catalog>)"` first. These
//! tests use a `pty` shim on PATH that echoes the env it was handed, proving `--catalog` / `$CATALOG`
//! / the XDG default plus argument pass-through without depending on a real pty.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// A temp dir holding a `pty` shim that prints the bus env + its args.
fn pty_shim() -> tempfile::TempDir {
    let bin = tempfile::tempdir().unwrap();
    let shim = bin.path().join("pty");
    std::fs::write(
        &shim,
        "#!/bin/sh\necho CATALOG=$CATALOG\necho PTY_ROOT=$PTY_ROOT\necho ST_ROOT=$ST_ROOT\necho ARGS=$*\n",
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn path_with(bin: &Path) -> String {
    format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

#[test]
fn pty_defaults_to_the_xdg_catalog_and_passes_args_through() {
    let bin = pty_shim();
    let state = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["pty", "peek", "sess1"])
        .current_dir(elsewhere.path())
        .env("PATH", path_with(bin.path()))
        .env("XDG_STATE_HOME", state.path())
        .env_remove("CATALOG")
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    let catalog = state.path().join("st2/default/catalog");

    assert!(s.contains(&format!("CATALOG={}", catalog.display())), "{s}");
    assert!(
        s.contains(&format!("PTY_ROOT={}/pty", catalog.display())),
        "{s}"
    );
    assert!(s.contains(&format!("ST_ROOT={}", catalog.display())), "{s}");
    assert!(
        s.contains("ARGS=peek sess1"),
        "args not passed through: {s}"
    );
}

#[test]
fn pty_honors_a_preset_catalog_env_over_cwd() {
    let bin = pty_shim();
    let cat = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["pty", "ls"])
        .current_dir(elsewhere.path()) // cwd is NOT the catalog
        .env("PATH", path_with(bin.path()))
        .env("XDG_STATE_HOME", state.path())
        .env("CATALOG", cat.path())
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    let canon = cat.path().canonicalize().unwrap();
    assert!(
        s.contains(&format!("CATALOG={}", canon.display())),
        "preset CATALOG ignored: {s}"
    );
    assert!(s.contains("ARGS=ls"), "{s}");
}

#[test]
fn pty_global_catalog_flag_overrides_the_environment() {
    let bin = pty_shim();
    let selected = tempfile::tempdir().unwrap();
    let ambient = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_st2"))
        .arg("--catalog")
        .arg(selected.path())
        .args(["pty", "attach", "ExampleMac.cos"])
        .env("PATH", path_with(bin.path()))
        .env("CATALOG", ambient.path())
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    let selected = selected.path().canonicalize().unwrap();

    assert!(
        s.contains(&format!("CATALOG={}", selected.display())),
        "{s}"
    );
    assert!(s.contains("ARGS=attach ExampleMac.cos"), "{s}");
}

#[test]
fn pty_passes_hyphen_flags_through() {
    // trailing_var_arg + allow_hyphen_values: pty's own flags must reach pty, not be eaten by st2.
    let bin = pty_shim();
    let state = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["pty", "ls", "--json"])
        .current_dir(elsewhere.path())
        .env("PATH", path_with(bin.path()))
        .env("XDG_STATE_HOME", state.path())
        .env_remove("CATALOG")
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("ARGS=ls --json"),
        "hyphen flag not passed through: {s}"
    );
}

#[test]
fn shell_execs_the_shell_with_the_bus_env() {
    // Use the env-echoing shim as $SHELL: st2 shell must exec it with the catalog env + pass args.
    let bin = pty_shim();
    let shell = bin.path().join("pty");
    let state = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_st2"))
        .args(["shell", "-c", "noop"])
        .current_dir(elsewhere.path())
        .env("SHELL", &shell)
        .env("XDG_STATE_HOME", state.path())
        .env_remove("CATALOG")
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    let catalog = state.path().join("st2/default/catalog");
    assert!(s.contains(&format!("CATALOG={}", catalog.display())), "{s}");
    assert!(
        s.contains(&format!("PTY_ROOT={}/pty", catalog.display())),
        "{s}"
    );
    assert!(
        s.contains("ARGS=-c noop"),
        "shell args not passed through: {s}"
    );
}
