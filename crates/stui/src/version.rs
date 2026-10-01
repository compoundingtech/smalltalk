//! This build's identity, after the shared build versioning contract: `machine` is exact and
//! stable (`0.1.0+36286721`, `-dirty` when the source had uncommitted changes); `display` adds
//! how old it is. A clean build is as old as its commit; a dirty one as old as the build.

const BASE: &str = env!("CARGO_PKG_VERSION");
const REV: &str = env!("STUI_REV");
const DIRTY: &str = env!("STUI_DIRTY");
const COMMIT_TS: &str = env!("STUI_COMMIT_TS");
const BUILD_TS: &str = env!("STUI_BUILD_TS");

pub fn machine() -> String {
    machine_of(BASE, REV, DIRTY == "true")
}

fn machine_of(base: &str, rev: &str, dirty: bool) -> String {
    match (rev.is_empty(), dirty) {
        (true, _) => base.to_owned(),
        (false, false) => format!("{base}+{rev}"),
        (false, true) => format!("{base}+{rev}-dirty"),
    }
}

/// For `--version`: everything known, in words.
pub fn display(now: u64) -> String {
    let mut parts = Vec::new();
    if let Some(age) = age(COMMIT_TS, now) {
        parts.push(format!("committed {age}"));
    }
    if DIRTY == "true" {
        parts.push("with uncommitted changes".to_owned());
    }
    if let Some(age) = age(BUILD_TS, now) {
        parts.push(format!("built {age}"));
    }
    match parts.is_empty() {
        true => machine(),
        false => format!("{} — {}", machine(), parts.join(", ")),
    }
}

/// For the footer: the machine version and one age, `0.1.0+36286721 · 3h ago`.
pub fn short(now: u64) -> String {
    let dirty = DIRTY == "true";
    short_of(
        &machine(),
        age(if dirty { BUILD_TS } else { COMMIT_TS }, now),
        dirty,
    )
}

fn short_of(machine: &str, age: Option<String>, dirty: bool) -> String {
    match age {
        Some(age) if dirty => format!("{machine} · built {age}"),
        Some(age) => format!("{machine} · {age}"),
        None => machine.to_owned(),
    }
}

fn age(timestamp: &str, now: u64) -> Option<String> {
    let then = timestamp.parse::<u64>().ok().filter(|then| *then > 0)?;
    let seconds = now.saturating_sub(then);
    Some(if seconds < 60 {
        "just now".to_owned()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86400)
    })
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_name_the_revision_and_say_how_old_the_build_is() {
        assert_eq!(machine_of("0.1.0", "", false), "0.1.0");
        assert_eq!(machine_of("0.1.0", "36286721", false), "0.1.0+36286721");
        assert_eq!(
            machine_of("0.1.0", "36286721", true),
            "0.1.0+36286721-dirty"
        );
        assert_eq!(age("1000", 1000 + 3 * 3600 + 5).as_deref(), Some("3h ago"));
        assert_eq!(age("1000", 1000 + 3 * 86400).as_deref(), Some("3d ago"));
        assert_eq!(age("1000", 1010).as_deref(), Some("just now"));
        assert_eq!(age("", 1010), None);
        assert_eq!(
            short_of("0.1.0+36286721", Some("3h ago".into()), false),
            "0.1.0+36286721 · 3h ago"
        );
        assert_eq!(
            short_of("0.1.0+36286721-dirty", Some("5m ago".into()), true),
            "0.1.0+36286721-dirty · built 5m ago"
        );
        assert!(display(now()).starts_with(&machine()));
    }
}
