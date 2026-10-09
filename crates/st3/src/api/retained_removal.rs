//! Read one writer-owned local refusal, never infer healthy membership from its absence.
use std::fs::{File, Metadata, OpenOptions};
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::Path;

use crate::model::DoctorCheck;

const MAX_SETTINGS_BYTES: usize = 16 * 1024;
const MAX_IDENTITY_BYTES: usize = 128;
// Count even delimiters in comments/strings. This deliberately conservative refusal bounds
// nesting and dotted-key paths independently of toml's feature-unified recursion guard.
const MAX_STRUCTURE_MARKERS: usize = 64;

#[derive(Debug, PartialEq)]
enum EndedMembership {
    Removed,
    Left,
}

pub(super) fn check(state_dir: &Path, node: &str, fleet_id: Option<&str>) -> DoctorCheck {
    let evidence = fleet_id
        .ok_or("no configured fleet")
        .and_then(|fleet_id| capture(state_dir, node, fleet_id, || {}));
    match evidence {
        Ok(Some((ended, reporter))) => {
            let what = match ended {
                EndedMembership::Removed => "this node was removed from fleet",
                EndedMembership::Left => "this node left fleet",
            };
            DoctorCheck {
                name: "replication".into(),
                status: "fail".into(),
                message: format!(
                    "{what} {}; as {reporter} reported in retained local settings; learned-at unavailable; the current graph is not certified. Run st fleet leave --offline to keep a local-only store that a member can invite again",
                    fleet_id.expect("captured evidence has a configured fleet")
                ),
            }
        }
        result => {
            let reason = match result {
                Ok(None) => "no retained local removal marker",
                Err(reason) => reason,
                Ok(Some(_)) => unreachable!(),
            };
            DoctorCheck {
                name: "replication".into(),
                status: "unknown".into(),
                message: format!(
                    "evidence incomplete: {reason}; the current graph is not certified; this read does not start an audit"
                ),
            }
        }
    }
}

fn open_regular(path: &Path) -> Result<File, &'static str> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| "local fleet settings unavailable")?;
    if !file
        .metadata()
        .map_err(|_| "local fleet settings metadata unavailable")?
        .is_file()
    {
        return Err("local fleet settings are not a regular file");
    }
    Ok(file)
}

fn same_version(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

fn supported_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control)
}

// The hook is a private deterministic test seam at the actual post-decode/pre-validation
// boundary. Production passes a no-op; there is no callback registration or background job.
fn capture(
    state_dir: &Path,
    node: &str,
    fleet_id: &str,
    after_decode: impl FnOnce(),
) -> Result<Option<(EndedMembership, String)>, &'static str> {
    let path = crate::fleet::FleetFile::path(state_dir);
    let mut file = open_regular(&path)?;
    let before = file
        .metadata()
        .map_err(|_| "local fleet settings metadata unavailable")?;
    if before.len() > MAX_SETTINGS_BYTES as u64 {
        return Err("local fleet settings exceed the diagnostic byte limit");
    }
    let mut bytes = Vec::with_capacity(MAX_SETTINGS_BYTES + 1);
    (&mut file)
        .take((MAX_SETTINGS_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "local fleet settings read failed")?;
    if bytes.len() > MAX_SETTINGS_BYTES {
        return Err("local fleet settings exceed the diagnostic byte limit");
    }
    if bytes.len() as u64 != before.len() {
        return Err("local fleet settings changed during capture");
    }
    if bytes
        .iter()
        .filter(|&&byte| matches!(byte, b'[' | b'{' | b'.'))
        .take(MAX_STRUCTURE_MARKERS + 1)
        .count()
        > MAX_STRUCTURE_MARKERS
    {
        return Err("local fleet settings exceed the diagnostic syntax limit");
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| "local fleet settings have invalid text")?;
    let settings: crate::fleet::FleetFile =
        toml::from_str(text).map_err(|_| "local fleet settings cannot be decoded")?;
    let id = uuid::Uuid::parse_str(&settings.fleet_id)
        .map_err(|_| "local fleet settings have an invalid fleet identity")?;
    if id.to_string() != settings.fleet_id
        || settings.fleet_id != fleet_id
        || !supported_identity(node)
        || settings.node.as_deref() != Some(node)
    {
        return Err("local fleet settings do not bind this daemon's fleet and node");
    }
    let marker = settings
        .removed
        .map(|removed| {
            if !supported_identity(&removed.reported_by) {
                return Err("local removal reporter is unsupported");
            }
            let ended = match removed.code.as_str() {
                "member-removed" => EndedMembership::Removed,
                "member-left" => EndedMembership::Left,
                _ => return Err("local removal code is unsupported"),
            };
            Ok((ended, removed.reported_by))
        })
        .transpose()?;
    after_decode();
    let after = file
        .metadata()
        .map_err(|_| "local fleet settings metadata unavailable")?;
    let current = open_regular(&path)?
        .metadata()
        .map_err(|_| "local fleet settings metadata unavailable")?;
    if !same_version(&before, &after) || !same_version(&before, &current) {
        return Err("local fleet settings changed during capture");
    }
    // An enduring writer refusal does not expire with time. Its version is captured here,
    // and its absence after leave/rejoin still does not certify any healthy graph invariant.
    Ok(marker)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLEET: &str = "3b241101-e2bb-4255-8caf-4136c566a962";

    fn settings(code: Option<&str>) -> crate::fleet::FleetFile {
        crate::fleet::FleetFile {
            fleet_id: FLEET.into(),
            node: Some("birch".into()),
            removed: code.map(|code| crate::fleet::FleetRemoval {
                code: code.into(),
                reported_by: "alder".into(),
            }),
            ..Default::default()
        }
    }

    fn assert_unknown(root: &Path) {
        let result = check(root, "birch", Some(FLEET));
        assert_eq!(result.status, "unknown", "{result:?}");
        assert!(result.message.contains("evidence incomplete"));
    }

    #[test]
    fn removed_and_left_markers_fail_without_certifying_the_graph_or_age() {
        let root = tempfile::tempdir().unwrap();
        for code in ["member-removed", "member-left"] {
            settings(Some(code)).save(root.path()).unwrap();
            let path = crate::fleet::FleetFile::path(root.path());
            let before = std::fs::read(&path).unwrap();
            let result = check(root.path(), "birch", Some(FLEET));
            assert_eq!(result.status, "fail");
            assert!(result.message.contains(if code == "member-removed" {
                "this node was removed from fleet"
            } else {
                "this node left fleet"
            }));
            assert!(result.message.contains("st fleet leave --offline"));
            assert!(result.message.contains("learned-at unavailable"));
            assert!(result.message.contains("current graph is not certified"));
            assert!(result.message.len() < 512);
            assert_eq!(std::fs::read(&path).unwrap(), before);
            // No notice sidecar or signing key is required or opened by this consumer.
            assert!(!root.path().join("fleet/removal.json").exists());
            assert!(!root.path().join("fleet/node.key").exists());
        }
    }

    #[test]
    fn absent_unsupported_or_mismatched_evidence_stays_unknown() {
        let root = tempfile::tempdir().unwrap();
        assert_unknown(root.path());
        for code in [None, Some("invalid-code")] {
            settings(code).save(root.path()).unwrap();
            assert_unknown(root.path());
        }
        for change in 0..5 {
            let mut file = settings(Some("member-removed"));
            match change {
                0 => file.node = None,
                1 => file.node = Some("different".into()),
                2 => file.fleet_id = uuid::Uuid::nil().to_string(),
                3 => file.removed.as_mut().unwrap().reported_by = "x".repeat(129),
                _ => file.removed.as_mut().unwrap().reported_by = "line\nbreak".into(),
            }
            file.save(root.path()).unwrap();
            assert_unknown(root.path());
        }
        assert_eq!(check(root.path(), "birch", None).status, "unknown");
    }

    #[test]
    fn byte_syntax_utf8_and_decode_limits_refuse_before_healthy_results() {
        let root = tempfile::tempdir().unwrap();
        settings(Some("member-removed")).save(root.path()).unwrap();
        let path = crate::fleet::FleetFile::path(root.path());
        for bytes in [
            vec![b' '; MAX_SETTINGS_BYTES + 1],
            vec![0xff],
            b"not valid TOML".to_vec(),
            format!("value = {}0{}", "[".repeat(100), "]".repeat(100)).into_bytes(),
            format!("{} = 0", ["a"; 100].join(".")).into_bytes(),
        ] {
            std::fs::write(&path, bytes).unwrap();
            assert_unknown(root.path());
        }
        // A full-size source within both budgets is supported, including a comment pad.
        let mut bytes = toml::to_string(&settings(Some("member-removed")))
            .unwrap()
            .into_bytes();
        bytes.extend_from_slice(b"\n#");
        bytes.resize(MAX_SETTINGS_BYTES, b' ');
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(check(root.path(), "birch", Some(FLEET)).status, "fail");
    }

    #[test]
    fn symlinks_directories_and_fifos_do_not_become_evidence() {
        use std::os::unix::ffi::OsStrExt as _;
        let root = tempfile::tempdir().unwrap();
        settings(Some("member-removed")).save(root.path()).unwrap();
        let path = crate::fleet::FleetFile::path(root.path());
        let target = root.path().join("settings.toml");
        std::fs::rename(&path, &target).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert_unknown(root.path());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_unknown(root.path());
        std::fs::remove_dir(&path).unwrap();
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a live NUL-terminated private temporary path.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert_unknown(root.path());
    }

    #[test]
    fn replacement_after_decode_or_in_place_mutation_invalidates_capture() {
        let root = tempfile::tempdir().unwrap();
        settings(Some("member-removed")).save(root.path()).unwrap();
        let replaced = capture(root.path(), "birch", FLEET, || {
            settings(None).save(root.path()).unwrap();
        });
        assert!(replaced.is_err());
        assert_unknown(root.path());
        settings(Some("member-removed")).save(root.path()).unwrap();
        let mutated = capture(root.path(), "birch", FLEET, || {
            use std::io::Write as _;
            OpenOptions::new()
                .append(true)
                .open(crate::fleet::FleetFile::path(root.path()))
                .unwrap()
                .write_all(b"\n# changed after decode\n")
                .unwrap();
        });
        assert!(mutated.is_err());
        let removed = capture(root.path(), "birch", FLEET, || {
            crate::fleet::FleetFile::remove(root.path()).unwrap();
        });
        assert!(removed.is_err());
        assert_unknown(root.path());
    }

    #[test]
    fn replacing_the_parent_after_decode_requires_path_validation() {
        let root = tempfile::tempdir().unwrap();
        settings(Some("member-removed")).save(root.path()).unwrap();
        let path = crate::fleet::FleetFile::path(root.path());
        let before = std::fs::metadata(&path).unwrap();
        let captured = capture(root.path(), "birch", FLEET, || {
            std::fs::rename(root.path().join("fleet"), root.path().join("old-fleet")).unwrap();
            settings(Some("member-removed")).save(root.path()).unwrap();
            // The original file is unchanged: descriptor rechecks alone cannot detect this.
            let original = std::fs::metadata(root.path().join("old-fleet/fleet.toml")).unwrap();
            assert!(same_version(&before, &original));
        });
        assert!(captured.is_err());
        assert_eq!(check(root.path(), "birch", Some(FLEET)).status, "fail");
    }

    #[test]
    fn reopening_after_leave_and_same_name_rejoin_never_reuses_an_old_marker() {
        let root = tempfile::tempdir().unwrap();
        settings(Some("member-removed")).save(root.path()).unwrap();
        for _ in 0..2 {
            assert_eq!(check(root.path(), "birch", Some(FLEET)).status, "fail");
        }
        crate::fleet::FleetFile::remove(root.path()).unwrap();
        assert_unknown(root.path());
        settings(None).save(root.path()).unwrap();
        assert_unknown(root.path());
        settings(Some("member-left")).save(root.path()).unwrap();
        assert_eq!(check(root.path(), "birch", Some(FLEET)).status, "fail");
    }
}
