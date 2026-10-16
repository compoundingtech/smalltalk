//! Bake a LocalStamp for a plain `cargo build`, per the shared
//! build-versioning contract. Emitted as `ST_BUILD_STAMP_LOCAL` — a private var
//! distinct from the fleet's `CLI_BUILD_STAMP`, so the flake's authoritative
//! NixStamp can never be overridden by this (see src/version.rs). A hermetic Nix
//! build has no `.git`, so this yields nothing there and the NixStamp is used.
//!
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Tell Cargo which Git files can change the stamp. The directory `refs` is NOT one of them: in a linked worktree
/// `git rev-parse --git-path refs` names the COMMON directory that every worktree of the repository shares, and Cargo
/// rescans a watched directory recursively, so any ref updated by anyone (a fetch, a push, a branch in another worktree)
/// re-ran this script and rebuilt everything that depends on this crate (a whole workspace, ~12 minutes) although this
/// worktree's HEAD had not moved. HEAD, the index and the ONE ref HEAD names are the inputs. While that ref is a loose file
/// only the file is watched; when it exists only in packed-refs (after `git gc` or `git pack-refs`) packed-refs is watched
/// instead, and so is the directory the loose file will appear in, because a new commit creates it there. A path that does
/// not exist is skipped: Cargo treats a missing watched file as always changed.
fn watch_git_inputs() {
    let path_of = |name: &str| git(&["rev-parse", "--git-path", name]);
    let watch = |path: &str| {
        if std::path::Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    };
    for name in ["HEAD", "index"] {
        if let Some(path) = path_of(name) {
            watch(&path);
        }
    }
    let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) else {
        return;
    };
    let Some(loose) = path_of(&branch) else {
        return;
    };
    if std::path::Path::new(&loose).exists() {
        watch(&loose);
        return;
    }
    if let Some(packed) = path_of("packed-refs") {
        watch(&packed);
    }
    if let Some(directory) = std::path::Path::new(&loose).parent().and_then(|p| p.to_str()) {
        watch(directory);
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=AGENT_SPEC_REVISION");
    let mut explicit_clean_source = false;
    if let Some(rev) = git(&["rev-parse", "--short", "HEAD"]) {
        let full_rev = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| rev.clone());
        let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
        explicit_clean_source = !dirty
            && std::env::var("AGENT_SPEC_REVISION")
                .ok()
                .zip(git(&["rev-parse", "HEAD"]))
                .is_some_and(|(supplied, head)| supplied == head);
        let commit_ts = git(&["log", "-1", "--format=%ct"])
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        // Reject an invalid release identity during the build, before emitting a stamp.
        // This is the same local machine-version grammar as st-drivers::version.
        let version = std::env::var("CARGO_PKG_VERSION")
            .expect("Cargo must supply the package version to the build stamp generator");
        let suffix = if dirty { ".dirty" } else { "" };
        assert!(
            format!("{version}+local.{rev}{suffix}").len() <= 128,
            "build machine version exceeds 128 bytes; correct the package version or revision"
        );
        // Hand-assembled JSON: the short-sha is hex so no escaping is needed, and
        // this avoids a build-dependency just to serialize three fields.
        let stamp =
            format!(r#"{{"type":"local","rev":"{rev}","fullRev":"{full_rev}","commitTs":{commit_ts},"dirty":{dirty}}}"#);
        println!("cargo:rustc-env=ST_BUILD_STAMP_LOCAL={stamp}");
    }
    // Performance supplies the real full SHA for a clean checkout. Its value changes
    // with source identity, while fresh index/ref timestamps do not affect this stamp.
    // Keep deriving the stamp above from Git, including its real commit time.
    if explicit_clean_source {
        println!("cargo:rerun-if-changed=src");
        println!("cargo:rerun-if-changed=Cargo.toml");
        return;
    }
    // Rebuild the stamp when HEAD moves or the working tree changes (dirty flag).
    // This build script also stamps st-drivers from its crate directory. Git resolves the
    // same worktree metadata for both callers, including linked-worktree layouts.
    watch_git_inputs();
}
