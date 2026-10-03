//! This build's identity, by the shared build-versioning contract: the same stamp and
//! formatting as st2 and st-drivers (`crates/st-drivers/src/version.rs`, compiled here as is),
//! fed by the workspace `build.rs` for a plain `cargo build` and by the flake's
//! `CLI_BUILD_STAMP` for a Nix build.

#[allow(dead_code)]
#[path = "../../st-drivers/src/version.rs"]
mod shared;

pub use shared::display_version;

/// For the footer: the machineVersion and how long ago its commit was, `0.1.0+local.ab12cd3 ·
/// 3 hours ago`.
pub fn short(now: u64) -> String {
    let identity = shared::build_identity();
    match shared::relative_time(identity.commit_unix, now) {
        Some(age) => format!("{} · {age}", shared::machine_version()),
        None => shared::machine_version(),
    }
}

/// This build as the client header names it: `stui 0.1.0+local.ab12cd3`.
pub fn client_name() -> String {
    format!("stui {}", shared::machine_version())
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_footer_names_the_shared_machine_version() {
        assert!(super::short(super::now()).starts_with(&super::shared::machine_version()));
    }
}
