//! Native-resume tests that read external sessions or parse intents; they live here because native_resume sits below the store.

#![allow(unused_imports, dead_code)]

use crate::native_resume::*;
use std::fs;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use sha2::{Digest as _, Sha256};
use std::os::unix::fs::{MetadataExt as _, symlink};
use std::os::fd::AsRawFd as _;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt as _;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

#[test]
fn suspend_preflight_names_selectors_in_typed_launches() {
    for (driver, args, expected) in [
        ("claude", "\"--continue\"", Some("--continue")),
        (
            "claude",
            "\"--resume=native-copper\"",
            Some("--resume=native-copper"),
        ),
        (
            "pi",
            "\"--session\" \"/missing-transcript\"",
            Some("--session"),
        ),
        ("omp", "\"-c\"", Some("-c")),
        ("omp", "\"--no-session\"", Some("--no-session")),
        ("opencode", "\"-s\" \"native-copper\"", Some("-s")),
        ("codex", "\"resume\" \"native-copper\"", Some("resume")),
        ("codex", "\"fork\" \"native-copper\"", Some("fork")),
        ("omp", "\"--model\" \"copper\"", None),
        ("omp", "\"--\" \"--resume\"", None),
        ("codex", "\"--\" \"resume\"", None),
    ] {
        let source = format!(
            "version 2\nagent \"example\" {{ host \"amber\"; workspace \"/workspace\"; harness {driver:?} {{ args {args}; }} }}"
        );
        let member = crate::parse_intent(&source, "amber")
            .unwrap()
            .subjects
            .into_values()
            .find_map(|desired| desired.member)
            .unwrap();
        assert_eq!(
            authored_session_selection(&member).as_deref(),
            expected,
            "{driver}: {args}"
        );
    }
}

#[cfg(unix)]
#[test]
fn authored_transcripts_have_disjoint_inventories() {
    assert_private_authored_inventories(false);
}

#[cfg(unix)]
#[test]
fn authored_transcripts_migrate_legacy_shared_links() {
    assert_private_authored_inventories(true);
}

#[cfg(unix)]
fn assert_private_authored_inventories(migrate: bool) {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let shared = root.path().join("shared");
    fs::create_dir(&shared).unwrap();
    let ids = [
        "5f9a6e16-5e30-4bce-b327-9a8241321bd6",
        "a5e213bc-c28e-42ba-a548-17aaf21ec32f",
    ];
    let transcripts: Vec<_> = ids.iter().enumerate().map(|(index, id)| {
        let path = shared.join(format!("2026-10-0{}_{id}.jsonl", index + 1));
        fs::write(&path, format!("{{\"type\":\"session\",\"id\":\"{id}\",\"timestamp\":\"2026-10-0{}T00:00:00Z\",\"cwd\":\"/example\"}}\n", index + 1)).unwrap();
        fs::create_dir(path.with_extension("")).unwrap();
        fs::write(path.with_extension("").join("artifact"), id).unwrap();
        path
    }).collect();
    let inventories: Vec<_> = (0..2).map(|index| {
        let managed = root.path().join(format!("inventory-{migrate}-{index}"));
        if migrate { symlink(&shared, &managed).unwrap(); }
        let arguments = argv(&["omp", "--resume", transcripts[index].to_str().unwrap()]);
        assert!(pi_family_link_transcript(&arguments, &managed).unwrap());
        assert!(!pi_family_link_transcript(&arguments, &managed).unwrap());
        assert!(!fs::symlink_metadata(&managed).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_dir(&managed).unwrap().count(), 2);
        assert!(pi_family_transcript(&managed, ids[index]).is_some());
        assert!(pi_family_transcript(&managed, ids[1 - index]).is_none());
        let latest = crate::external_sessions::find_managed_omp_transcript(&managed, 0)
            .unwrap().unwrap();
        assert_eq!(latest.native_id, ids[index]);
        assert_eq!(
            fs::read_to_string(managed.join(transcripts[index].file_stem().unwrap()).join("artifact")).unwrap(),
            ids[index],
        );
        managed
    }).collect();
    assert_ne!(fs::canonicalize(&inventories[0]).unwrap(), fs::canonicalize(&inventories[1]).unwrap());
    assert_eq!(fs::read_dir(&shared).unwrap().count(), 4);
    for (index, transcript) in transcripts.iter().enumerate() {
        assert_eq!(pi_family_header_id(transcript).as_deref(), Some(ids[index]));
        assert_eq!(fs::read_to_string(transcript.with_extension("").join("artifact")).unwrap(), ids[index]);
    }
}
