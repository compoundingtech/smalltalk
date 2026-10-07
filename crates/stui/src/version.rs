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
    client_name_labelled(std::env::var("STUI_CLIENT_LABEL").ok().as_deref())
}

/// A test client names itself apart (`STUI_CLIENT_LABEL=measure`) so a profile can tell its
/// connection from a person's; the label is never identity or authority.
fn client_name_labelled(label: Option<&str>) -> String {
    let label = label
        .map(|label| label.chars().filter(|c| c.is_ascii_graphic()).take(32).collect::<String>())
        .filter(|label| !label.is_empty());
    match label {
        Some(label) => format!("stui {} [{label}]", shared::machine_version()),
        None => format!("stui {}", shared::machine_version()),
    }
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
    fn a_test_client_labels_its_name_and_a_person_does_not() {
        assert!(!super::client_name_labelled(None).contains('['));
        assert!(super::client_name_labelled(Some("measure")).ends_with(" [measure]"));
        assert!(!super::client_name_labelled(Some(" \n")).contains('['));
    }

    #[test]
    fn the_footer_names_the_shared_machine_version() {
        assert!(super::short(super::now()).starts_with(&super::shared::machine_version()));
    }
}
