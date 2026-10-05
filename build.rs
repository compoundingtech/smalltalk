//! Bake a LocalStamp for `st2 --version` on a plain `cargo build`, per the shared
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

fn main() {
    println!("cargo:rerun-if-env-changed=AGENT_SPEC_REVISION");
    let mut explicit_clean_source = false;
    if let Some(rev) = git(&["rev-parse", "--short", "HEAD"]) {
        let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
        explicit_clean_source = !dirty
            && std::env::var("AGENT_SPEC_REVISION")
                .ok()
                .zip(git(&["rev-parse", "HEAD"]))
                .is_some_and(|(supplied, head)| supplied == head);
        let commit_ts = git(&["log", "-1", "--format=%ct"])
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        // Hand-assembled JSON: the short-sha is hex so no escaping is needed, and
        // this avoids a build-dependency just to serialize three fields.
        let stamp =
            format!(r#"{{"type":"local","rev":"{rev}","commitTs":{commit_ts},"dirty":{dirty}}}"#);
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
    for path in ["HEAD", "refs", "index"] {
        if let Some(path) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}
