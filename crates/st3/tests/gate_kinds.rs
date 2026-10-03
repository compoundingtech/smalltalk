#![cfg(target_os = "linux")]
//! `st gate cargo-test` answers the way an exec gate reads it: 0 passes, 1 is not yet, 3 is
//! broken. A stand-in `cargo` plays each way a test target can fare on `origin/main`.
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

fn git(directory: &Path, arguments: &[&str]) {
    let status = st3::test_support::git()
        .args([
            "-c",
            "user.name=Example",
            "-c",
            "user.email=example@example.invalid",
        ])
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .status()
        .unwrap();
    assert!(status.success(), "git {arguments:?}");
}

fn gate(root: &Path, mode: &str, repository: &Path) -> (Option<i32>, String) {
    std::fs::write(root.join("cargo-mode"), mode).unwrap();
    let output = st3::test_support::command(assert_cmd::cargo::cargo_bin!("st3-fixture"))
        .env_clear()
        .env("HOME", root)
        .env(
            "PATH",
            format!(
                "{}:{}",
                root.join("bin").display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("CARGO_MODE_FILE", root.join("cargo-mode"))
        .args([
            "gate",
            "cargo-test",
            "log_diet",
            "--package",
            "st3",
            "--repository",
        ])
        .arg(repository)
        .arg("--worktree")
        .arg(root.join("worktree"))
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr),
    )
}

#[test]
fn cargo_test_waits_for_its_target_and_breaks_on_a_build_this_host_cannot_make() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let origin = root.join("origin.git");
    let work = root.join("work");
    st3::test_support::git()
        .args(["init", "--quiet", "--bare", "--initial-branch=main"])
        .arg(&origin)
        .status()
        .unwrap();
    st3::test_support::git()
        .args(["clone", "--quiet"])
        .arg(&origin)
        .arg(&work)
        .status()
        .unwrap();
    std::fs::write(work.join("README.md"), "An invented crate.\n").unwrap();
    git(&work, &["add", "README.md"]);
    git(&work, &["commit", "--quiet", "-m", "Start"]);
    git(&work, &["push", "--quiet", "origin", "HEAD:main"]);

    std::fs::create_dir_all(root.join("bin")).unwrap();
    let cargo = root.join("bin/cargo");
    std::fs::write(
        &cargo,
        r#"#!/bin/sh
mode=$(cat "$CARGO_MODE_FILE")
case "$*" in
  *--no-run*)
    case "$mode" in
      missing) echo 'error: no test target named `log_diet` in `st3` package' >&2; exit 101 ;;
      linker) echo 'error: linker `mold` not found' >&2; exit 101 ;;
    esac
    echo 'Finished `test` profile' >&2; exit 0 ;;
esac
case "$mode" in
  failing) echo 'test result: FAILED. 1 passed; 1 failed'; exit 101 ;;
esac
echo 'test result: ok. 2 passed'
"#,
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();

    let (code, output) = gate(root, "missing", &work);
    assert_eq!(code, Some(1), "{output}");
    assert!(output.contains("not yet: origin/main"), "{output}");
    assert!(
        output.contains("has no test target `log_diet` in package `st3` yet"),
        "{output}"
    );

    let (code, output) = gate(root, "linker", &work);
    assert_eq!(code, Some(3), "{output}");
    assert!(
        output.contains("broken: test target `log_diet` does not build"),
        "{output}"
    );
    assert!(output.contains("linker `mold` not found"), "{output}");

    let (code, output) = gate(root, "failing", &work);
    assert_eq!(code, Some(1), "{output}");
    assert!(
        output.contains("not yet: test target `log_diet` fails at origin/main"),
        "{output}"
    );

    // A new commit on origin reaches the kept worktree on the next check.
    std::fs::write(work.join("CHANGELOG.md"), "One change.\n").unwrap();
    git(&work, &["add", "CHANGELOG.md"]);
    git(&work, &["commit", "--quiet", "-m", "Change"]);
    git(&work, &["push", "--quiet", "origin", "HEAD:main"]);
    let (code, output) = gate(root, "passing", &work);
    assert_eq!(code, Some(0), "{output}");
    assert!(
        output.contains("pass: test target `log_diet` passes at origin/main"),
        "{output}"
    );
    assert!(root.join("worktree/CHANGELOG.md").exists());

    let (code, output) = gate(root, "passing", root);
    assert_eq!(code, Some(3), "{output}");
    assert!(
        output.contains("is not inside a git repository"),
        "{output}"
    );
}
