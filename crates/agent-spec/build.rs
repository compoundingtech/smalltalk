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
        for path in ["HEAD", "refs", "index"] {
            if let Some(path) = git(&["rev-parse", "--git-path", path]) {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
}
