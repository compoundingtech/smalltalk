use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!value.is_empty()).then_some(value)
}

fn is_sha(revision: &str) -> bool {
    revision.len() == 40
        && revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=../../Cargo.lock");
    let supplied = std::env::var("AGENT_SPEC_REVISION")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let derive_from_git = supplied.is_none();
    let revision = supplied
        .inspect(|value| {
            assert!(
                is_sha(value) || value.starts_with("nix-dirty."),
                "AGENT_SPEC_REVISION must be a full source SHA or explicit nix-dirty identity"
            );
        })
        .unwrap_or_else(|| {
            let Some(head) = git(&["rev-parse", "HEAD"]) else {
                return "local.unknown".to_owned();
            };
            let dirty = git(&["status", "--porcelain"]).is_some_and(|value| !value.is_empty());
            if dirty {
                format!("local-dirty.{head}")
            } else {
                head
            }
        });
    println!("cargo:rustc-env=AGENT_SPEC_REVISION={revision}");
    // An explicitly supplied source identity is already tracked above. Git metadata
    // is an input only when deriving that identity from this checkout.
    if derive_from_git {
        watch_git_inputs();
    }
}
