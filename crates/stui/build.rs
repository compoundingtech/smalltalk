//! Build identity for `stui --version` and the footer: the short revision, whether the source
//! had uncommitted changes, and when it was committed and built. Fields follow the shared build
//! versioning contract (baseVersion, rev, dirty, commitTs, buildTs). Outside a git checkout, as in
//! a Nix sandbox, the revision is simply unknown.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    // A new commit, a changed index or edited source changes the identity or the build time.
    println!("cargo:rerun-if-changed=src");
    for path in ["HEAD", "index", "packed-refs"] {
        if let Some(path) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git(&["rev-parse", "--git-path", &reference])
    {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed=STUI_BUILD_REV");
    let rev = std::env::var("STUI_BUILD_REV")
        .ok()
        .or_else(|| git(&["rev-parse", "--short=8", "HEAD"]))
        .unwrap_or_default();
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|status| !status.is_empty());
    let commit = git(&["log", "-1", "--format=%ct"]).unwrap_or_default();
    let built = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    println!("cargo:rustc-env=STUI_REV={rev}");
    println!("cargo:rustc-env=STUI_DIRTY={dirty}");
    println!("cargo:rustc-env=STUI_COMMIT_TS={commit}");
    println!("cargo:rustc-env=STUI_BUILD_TS={built}");
}
