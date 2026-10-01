//! Build identity for `st2 --version`, per the shared build-versioning contract
//! (`baseVersion` / `rev` / `dirty` / `sourceKind` / `commitTs` → a stable
//! `machineVersion` and a human `displayVersion`).
//!
//! The stamp is a `CLI_BUILD_STAMP` JSON blob, the same env var + shape the rest
//! of the fleet uses (TS `@overeng/utils/node/cli-version`; the otel-scrape Rust
//! reader). It reaches this crate at compile time via `option_env!`:
//!   - `CLI_BUILD_STAMP` — a **NixStamp** the flake bakes from `self` (the flake
//!     rev, so a hermetic build still knows its identity). Authoritative.
//!   - `ST2_BUILD_STAMP_LOCAL` — a **LocalStamp** `build.rs` derives from `git`
//!     for a plain `cargo build`. A private, second env var so it can never
//!     collide with / override the Nix stamp; the Nix stamp always wins.
//!
//! st2 cannot depend on the private `effect-utils` helper crate (it ships as a
//! self-contained public flake), so this reimplements the contract. The
//! `machineVersion` grammar is kept byte-identical to that reader.

/// NixStamp baked by the flake (`{type:"nix",...}`); authoritative when present.
const NIX_STAMP: Option<&str> = option_env!("CLI_BUILD_STAMP");
/// LocalStamp baked by `build.rs` from git (`{type:"local",...}`).
const LOCAL_STAMP: Option<&str> = option_env!("ST2_BUILD_STAMP_LOCAL");
/// `baseVersion` from Cargo metadata.
const BASE: &str = env!("CARGO_PKG_VERSION");

/// A parsed build stamp in the shared contract.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BuildStamp {
    Nix {
        version: String,
        rev: String,
        commit_ts: i64,
        dirty: bool,
    },
    Local {
        rev: String,
        commit_ts: i64,
        dirty: bool,
    },
}

/// Build identity shared by user-facing versions and persistent receipts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuildIdentity {
    pub(crate) version: String,
    pub(crate) rev: String,
    pub(crate) commit_unix: u64,
    pub(crate) dirty: bool,
    source_kind: SourceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Nix,
    Local,
    Unknown,
}

/// Parse a `CLI_BUILD_STAMP` JSON string. Defensive: any missing/mistyped field
/// yields `None` so a malformed stamp degrades to the `+dev` fallback rather than
/// failing (`--version` must never panic).
fn parse_stamp(raw: &str) -> Option<BuildStamp> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let dirty = value
        .get("dirty")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let commit_ts = value.get("commitTs").and_then(|v| v.as_i64()).unwrap_or(0);
    match value.get("type").and_then(|v| v.as_str())? {
        "nix" => Some(BuildStamp::Nix {
            version: value.get("version")?.as_str()?.to_owned(),
            rev: value.get("rev")?.as_str()?.to_owned(),
            commit_ts,
            dirty,
        }),
        "local" => Some(BuildStamp::Local {
            rev: value.get("rev")?.as_str()?.to_owned(),
            commit_ts,
            dirty,
        }),
        _ => None,
    }
}

/// Resolve the effective stamp: the compile-time NixStamp wins (a hermetic
/// build's own identity), else the compile-time LocalStamp from `build.rs`.
fn resolve(nix: Option<&str>, local: Option<&str>) -> Option<BuildStamp> {
    if let Some(s @ BuildStamp::Nix { .. }) = nix.and_then(parse_stamp) {
        return Some(s);
    }
    local.and_then(parse_stamp)
}

fn identity(base: &str, stamp: Option<&BuildStamp>) -> BuildIdentity {
    match stamp {
        Some(BuildStamp::Nix {
            version,
            rev,
            commit_ts,
            dirty,
        }) => BuildIdentity {
            version: version.clone(),
            rev: rev.clone(),
            commit_unix: (*commit_ts).try_into().unwrap_or(0),
            dirty: *dirty,
            source_kind: SourceKind::Nix,
        },
        Some(BuildStamp::Local {
            rev,
            commit_ts,
            dirty,
        }) => BuildIdentity {
            version: base.to_owned(),
            rev: rev.clone(),
            commit_unix: (*commit_ts).try_into().unwrap_or(0),
            dirty: *dirty,
            source_kind: SourceKind::Local,
        },
        None => BuildIdentity {
            version: base.to_owned(),
            rev: "unknown".to_owned(),
            commit_unix: 0,
            dirty: false,
            source_kind: SourceKind::Unknown,
        },
    }
}

/// The authoritative identity for this binary, regardless of its build path.
pub(crate) fn build_identity() -> BuildIdentity {
    identity(BASE, resolve(NIX_STAMP, LOCAL_STAMP).as_ref())
}

/// `<version>+<rev>[-dirty]` for a NixStamp. The flake's `rev` may already end in
/// `-dirty` (dirtyShortRev), so the suffix is not doubled.
fn nix_machine(version: &str, rev: &str, dirty: bool) -> String {
    let suffix = if dirty && !rev.ends_with("-dirty") {
        "-dirty"
    } else {
        ""
    };
    format!("{version}+{rev}{suffix}")
}

/// `<base>+local.<rev>[.dirty]` for a LocalStamp.
fn local_machine(base: &str, rev: &str, dirty: bool) -> String {
    let suffix = if dirty { ".dirty" } else { "" };
    format!("{base}+local.{rev}{suffix}")
}

fn machine(identity: &BuildIdentity) -> String {
    match identity.source_kind {
        SourceKind::Nix => nix_machine(&identity.version, &identity.rev, identity.dirty),
        SourceKind::Local => local_machine(&identity.version, &identity.rev, identity.dirty),
        SourceKind::Unknown => format!("{}+dev", identity.version),
    }
}

/// Stable, parseable version for logs/telemetry/exact comparison. No prose, no
/// relative time.
pub fn machine_version() -> String {
    machine(&build_identity())
}

/// Human-facing version for `--version`: the machineVersion plus source-kind and
/// a relative commit time when known. Returns `&'static str` (clap's `version`
/// needs `'static`); computed once, at the first CLI build (i.e. `--version`).
pub fn display_version() -> &'static str {
    static RENDERED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    RENDERED
        .get_or_init(|| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            display(&build_identity(), now)
        })
        .as_str()
}

fn display(identity: &BuildIdentity, now: u64) -> String {
    let machine = machine(identity);
    match identity.source_kind {
        SourceKind::Nix => {
            let when = relative_time(identity.commit_unix, now);
            let mut s = format!("{machine} — committed");
            if let Some(when) = when {
                s.push_str(&format!(" {when}"));
            }
            if identity.dirty {
                s.push_str(", with uncommitted changes");
            }
            s
        }
        SourceKind::Local => {
            let mut detail = identity.rev.clone();
            if let Some(when) = relative_time(identity.commit_unix, now) {
                detail.push_str(&format!(", {when}"));
            }
            if identity.dirty {
                detail.push_str(", dirty");
            }
            format!(
                "{} — running from local source ({detail})",
                identity.version
            )
        }
        SourceKind::Unknown => machine,
    }
}

/// `commit_ts` (unix seconds) relative to `now`, coarse-grained. `None` when the
/// timestamp is unknown (0) or in the future.
fn relative_time(commit_ts: u64, now: u64) -> Option<String> {
    if commit_ts == 0 || now < commit_ts {
        return None;
    }
    let secs = now - commit_ts;
    let plural = |n: u64, unit: &str| {
        if n == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{n} {unit}s ago")
        }
    };
    Some(match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => plural(secs / 60, "minute"),
        3600..=86_399 => plural(secs / 3600, "hour"),
        86_400..=2_591_999 => plural(secs / 86_400, "day"),
        2_592_000..=31_535_999 => plural(secs / 2_592_000, "month"),
        _ => plural(secs / 31_536_000, "year"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NIX: &str =
        r#"{"type":"nix","version":"0.1.0","rev":"7d5211e","commitTs":1000000,"dirty":false}"#;
    const NIX_DIRTY: &str =
        r#"{"type":"nix","version":"0.1.0","rev":"7d5211e","commitTs":1000000,"dirty":true}"#;
    const LOCAL: &str = r#"{"type":"local","rev":"abc1234","commitTs":1000000,"dirty":false}"#;

    #[test]
    fn machine_version_grammar_matches_the_contract() {
        assert_eq!(
            machine(&identity("0.1.0", parse_stamp(NIX).as_ref())),
            "0.1.0+7d5211e"
        );
        assert_eq!(
            machine(&identity("0.1.0", parse_stamp(NIX_DIRTY).as_ref())),
            "0.1.0+7d5211e-dirty"
        );
        assert_eq!(
            machine(&identity("0.1.0", parse_stamp(LOCAL).as_ref())),
            "0.1.0+local.abc1234"
        );
        assert_eq!(machine(&identity("0.1.0", None)), "0.1.0+dev");
    }

    #[test]
    fn nix_stamp_wins_over_a_local_stamp() {
        assert_eq!(
            resolve(Some(NIX), Some(LOCAL)),
            Some(BuildStamp::Nix {
                version: "0.1.0".into(),
                rev: "7d5211e".into(),
                commit_ts: 1_000_000,
                dirty: false,
            })
        );
    }

    #[test]
    fn a_local_only_build_uses_the_local_stamp() {
        assert!(matches!(
            resolve(None, Some(LOCAL)),
            Some(BuildStamp::Local { .. })
        ));
    }

    #[test]
    fn a_malformed_or_missing_stamp_degrades_to_dev() {
        assert_eq!(resolve(Some("{ not json"), None), None);
        assert_eq!(resolve(Some(r#"{"type":"other"}"#), None), None);
        assert_eq!(
            machine(&identity("0.1.0", resolve(None, None).as_ref())),
            "0.1.0+dev"
        );
        assert_eq!(
            identity("0.1.0", resolve(None, None).as_ref()),
            BuildIdentity {
                version: "0.1.0".into(),
                rev: "unknown".into(),
                commit_unix: 0,
                dirty: false,
                source_kind: SourceKind::Unknown,
            }
        );
    }

    #[test]
    fn receipts_and_versions_resolve_the_same_nix_and_local_identity() {
        assert_eq!(
            identity("ignored", parse_stamp(NIX).as_ref()),
            BuildIdentity {
                version: "0.1.0".into(),
                rev: "7d5211e".into(),
                commit_unix: 1_000_000,
                dirty: false,
                source_kind: SourceKind::Nix,
            }
        );
        assert_eq!(
            identity("0.1.0", parse_stamp(LOCAL).as_ref()),
            BuildIdentity {
                version: "0.1.0".into(),
                rev: "abc1234".into(),
                commit_unix: 1_000_000,
                dirty: false,
                source_kind: SourceKind::Local,
            }
        );
    }

    #[test]
    fn dirty_rev_from_the_flake_is_not_double_suffixed() {
        let stamp = r#"{"type":"nix","version":"0.1.0","rev":"7d5211e-dirty","dirty":true}"#;
        assert_eq!(
            machine(&identity("0.1.0", parse_stamp(stamp).as_ref())),
            "0.1.0+7d5211e-dirty"
        );
    }

    #[test]
    fn display_reads_source_kind_and_relative_time() {
        // commit at ts 1_000_000, "now" three days later.
        let now = 1_000_000 + 3 * 86_400;
        assert_eq!(
            display(&identity("0.1.0", parse_stamp(NIX).as_ref()), now),
            "0.1.0+7d5211e — committed 3 days ago"
        );
        assert_eq!(
            display(&identity("0.1.0", parse_stamp(NIX_DIRTY).as_ref()), now),
            "0.1.0+7d5211e-dirty — committed 3 days ago, with uncommitted changes"
        );
        assert_eq!(
            display(&identity("0.1.0", parse_stamp(LOCAL).as_ref()), now),
            "0.1.0 — running from local source (abc1234, 3 days ago)"
        );
        assert_eq!(display(&identity("0.1.0", None), now), "0.1.0+dev");
    }

    #[test]
    fn relative_time_is_coarse_and_guards_unknown_or_future() {
        assert_eq!(relative_time(0, 100), None);
        assert_eq!(relative_time(100, 50), None);
        assert_eq!(relative_time(100, 130), Some("just now".to_string()));
        assert_eq!(
            relative_time(100, 100 + 120),
            Some("2 minutes ago".to_string())
        );
        assert_eq!(
            relative_time(100, 100 + 3600),
            Some("1 hour ago".to_string())
        );
        assert_eq!(
            relative_time(100, 100 + 86_400),
            Some("1 day ago".to_string())
        );
    }
}
