//! Bake a LocalStamp for `st2 --version` on a plain `cargo build`, per the shared
//! build-versioning contract. Emitted as `ST2_BUILD_STAMP_LOCAL` — a private var
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
    if let Some(rev) = git(&["rev-parse", "--short", "HEAD"]) {
        let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
        let commit_ts = git(&["log", "-1", "--format=%ct"])
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        // Hand-assembled JSON: the short-sha is hex so no escaping is needed, and
        // this avoids a build-dependency just to serialize three fields.
        let stamp =
            format!(r#"{{"type":"local","rev":"{rev}","commitTs":{commit_ts},"dirty":{dirty}}}"#);
        println!("cargo:rustc-env=ST2_BUILD_STAMP_LOCAL={stamp}");
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
