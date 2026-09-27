//! One stage-and-rename publication primitive for the state-plane records.
//!
//! Nine helpers in eight modules each staged a sibling and renamed it over the target, with their
//! own temp-name scheme and their own answer to the parts that actually matter: whether the
//! staging file is created exclusively, what mode it carries, and whether a failed staging is
//! cleaned up. The line count was never the problem — deciding the same security question nine
//! times and getting nine answers was, most visibly as the defect fixed in #502 (a predictable
//! staging name, created non-exclusively, in an agent-writable directory).
//!
//! Three things stay with the caller, deliberately:
//!
//! - **Serialization.** Callers pass bytes. The absorbed helpers serialize three different ways
//!   (`to_vec`, `to_vec` plus a newline, `to_writer` plus a newline); a `json` entry point here
//!   would have to reproduce all three to keep every record byte-identical, so the module would
//!   carry the difference instead of removing it.
//! - **The staging-name prefix.** The grammar is `{prefix}.tmp-{pid}-{counter}` and it is
//!   load-bearing, not decorative: `.status.tmp-` is matched by prefix in six catalog and
//!   publication walkers, `.message.tmp-` in four sent-record walkers, and
//!   `.harness-context.tmp-<pid>-<counter>` is parsed digit by digit by
//!   [`crate::harness_context::is_legacy_staging_name`] under INVARIANTS row 29. A module that
//!   invented its own names would silently turn staged files into durable replicated keys.
//! - **Error context.** Every function returns [`io::Result`], never `anyhow`, so each callsite
//!   keeps the exact context string it already had.
//!
//! Five publishers deliberately do **not** use this module, because each holds a strictly stronger
//! primitive than a path-based stage-and-rename can express:
//! `catalog_transaction::atomic_replace_file` and `agent_publish::{atomic_write_spec,
//! atomic_publish_staged_bundle}` publish through a retained control-directory fd with `EXDEV`
//! fault injection and error classification; `resource_profile::atomic_replace_at` and
//! `event::write_record` use `openat`/`renameat` against a retained directory capability;
//! `codex_app_server::atomic_json` chmods its state directory to `0700` on every write and emits
//! pretty JSON. `pretrust::write_atomic` also stays out for the opposite reason: it publishes
//! files st2 does not own (`~/.claude.json`, `~/.codex/config.toml`), where tightening a foreign
//! config's mode has no argument behind it.

use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// One counter for the whole process: the staging name only has to be unique per write, and the
/// pid already separates processes.
static STAGING_SERIAL: AtomicU64 = AtomicU64::new(0);

/// How far a publication is pushed before it reports success.
///
/// There is no `FsyncFile` variant: no caller wants one, and an in-process test cannot observe an
/// fsync that nothing can be made to fail, so shipping the variant would ship a promise no test
/// keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Durability {
    /// Stage, then rename. A concurrent reader sees the old bytes or the new bytes and never a
    /// partial file; a crash can lose the write entirely. Correct where a lost write reads as
    /// "unknown" rather than as something false.
    Rename,
    /// Stage, fsync the staged file, rename, then fsync the parent directory, so both the bytes
    /// and the directory entry naming them survive a crash. Strict: a parent directory that
    /// cannot be opened for that sync fails the publication.
    FsyncFileAndDir,
}

/// Where the staged sibling lives and what it is called.
///
/// `dir` exists for exactly one caller pair: the `harness-state` record stages beside itself,
/// while `harness-context` stages in the catalog control plane, because a staged name inside the
/// replicated `agents` namespace becomes a durable replicated key (HC-R05).
pub(crate) struct Staging<'a> {
    prefix: &'a str,
    dir: Option<&'a Path>,
}

impl<'a> Staging<'a> {
    /// Stage beside the target, under `{prefix}.tmp-{pid}-{counter}`.
    pub(crate) const fn new(prefix: &'a str) -> Self {
        Self { prefix, dir: None }
    }

    /// Stage in `dir` instead of beside the target. The caller owns the guarantee that `dir` is on
    /// the target's filesystem — a rename across filesystems is `EXDEV`, not an atomic publish.
    pub(crate) const fn in_dir(self, dir: &'a Path) -> Self {
        Self {
            prefix: self.prefix,
            dir: Some(dir),
        }
    }
}

/// Replace `path` with `bytes`, atomically for readers of `path`.
pub(crate) fn replace(
    path: &Path,
    bytes: &[u8],
    staging: Staging<'_>,
    durability: Durability,
) -> io::Result<()> {
    let parent = parent_of(path)?;
    let staged = prepare(parent, &staging)?;
    // A refused creation returns BEFORE the cleanup scope: the thing occupying a
    // `{prefix}.tmp-{pid}-{counter}` path is not ours, and refusing to follow it must not turn
    // into unlinking it.
    let mut file = create_staging(&staged)?;
    let staged_bytes = (|| -> io::Result<()> {
        file.write_all(bytes)?;
        if durability == Durability::FsyncFileAndDir {
            file.sync_all()?;
        }
        Ok(())
    })();
    drop(file);
    if let Err(error) = staged_bytes.and_then(|()| fs::rename(&staged, path)) {
        // Best-effort, and only for the file this call created.
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    if durability == Durability::FsyncFileAndDir {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// Publish `bytes` at `path` only if nothing holds that name yet, reporting whether this call is
/// the one that created it.
///
/// A hardlink rather than a rename, because the name being taken is the answer the caller wants
/// rather than a failure: both callers use the boolean to tell a replay from a first publication.
/// One durability level, because both callers have one — an fsync arm here would have no caller
/// and therefore no test.
pub(crate) fn create_once(path: &Path, bytes: &[u8], staging: Staging<'_>) -> io::Result<bool> {
    let parent = parent_of(path)?;
    let staged = prepare(parent, &staging)?;
    let mut file = create_staging(&staged)?;
    let staged_bytes = file.write_all(bytes);
    drop(file);
    // A failed staged write is never a duplicate: `Ok(false)` claims somebody else published
    // these bytes, and bytes that were never written are nobody's publication. Only the hardlink
    // may answer that question.
    let created = staged_bytes.and_then(|()| match fs::hard_link(&staged, path) {
        Ok(()) => Ok(true),
        // `hard_link` reports `AlreadyExists` for a taken name, but a target that is already a
        // regular file is the same answer whatever the error says.
        Err(_) if path.is_file() => Ok(false),
        Err(error) => Err(error),
    });
    let _ = fs::remove_file(&staged);
    created
}

/// Create the staging sibling exclusively at `0600`.
///
/// An existing regular file, a directory, or a symlink an agent planted at this path is refused
/// with `AlreadyExists` rather than followed or truncated. State-plane records live in
/// agent-writable directories, so that refusal is the whole security property; `0600` is the other
/// half, because a record readable by anyone who can reach the directory is a record an
/// unprivileged reader can harvest.
pub(crate) fn create_staging(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Make both directories exist and name the staging file. Creating the target's parent is part of
/// every absorbed helper's contract: publication paths are derived, not authored, so the first
/// write to an agent is what creates its directory.
fn prepare(parent: &Path, staging: &Staging<'_>) -> io::Result<PathBuf> {
    fs::create_dir_all(parent)?;
    let dir = staging.dir.unwrap_or(parent);
    if dir != parent {
        fs::create_dir_all(dir)?;
    }
    Ok(dir.join(staging_name(staging.prefix)))
}

/// One staged name, `{prefix}.tmp-{pid}-{counter}`.
///
/// Exposed because two message publications stage their own file for reasons this module does not
/// cover — one renames into a name it has to search for, the other compares bytes on collision —
/// and they must draw from the SAME counter as everything else that stages under `.message.tmp-`
/// in the same directory. Two counters for one prefix would collide, which is precisely the class
/// of failure exclusive creation then reports as an error.
pub(crate) fn staging_name(prefix: &str) -> String {
    format!(
        "{prefix}.tmp-{}-{}",
        std::process::id(),
        STAGING_SERIAL.fetch_add(1, Ordering::Relaxed)
    )
}

/// A bare relative name stages in the current directory, which is where its rename lands too.
fn parent_of(path: &Path) -> io::Result<&Path> {
    match path.parent() {
        Some(parent) if parent.as_os_str().is_empty() => Ok(Path::new(".")),
        Some(parent) => Ok(parent),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "publication path has no parent directory",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    /// The grammar six catalog walkers and `harness_context::is_legacy_staging_name` parse. Two
    /// successive names differ in the counter, not in a clock reading: the helper this replaced in
    /// `context` used `now_ms()`, so two writers in the same millisecond shared a staging path and
    /// the second truncated the first's staged bytes.
    #[test]
    fn a_staging_name_is_prefix_pid_counter_and_never_repeats() {
        let first = staging_name(".status");
        let second = staging_name(".status");
        assert_ne!(first, second);
        for name in [&first, &second] {
            let rest = name
                .strip_prefix(".status.tmp-")
                .expect("the grammar is `{prefix}.tmp-{pid}-{counter}`");
            let (pid, counter) = rest.split_once('-').expect("pid and counter are separated");
            assert_eq!(pid, std::process::id().to_string());
            assert!(!counter.is_empty() && counter.bytes().all(|byte| byte.is_ascii_digit()));
        }
    }

    #[test]
    fn a_replacement_lands_owner_only_and_leaves_no_staged_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/record");
        replace(&path, b"first", Staging::new(".record"), Durability::Rename).unwrap();
        replace(
            &path,
            b"second",
            Staging::new(".record"),
            Durability::FsyncFileAndDir,
        )
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(entries(path.parent().unwrap()), vec!["record".to_owned()]);
    }

    #[test]
    fn a_create_once_publication_keeps_the_first_bytes_and_reports_the_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/record");
        assert!(create_once(&path, b"first", Staging::new(".record")).unwrap());
        assert!(!create_once(&path, b"second", Staging::new(".record")).unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"first");
        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(entries(path.parent().unwrap()), vec!["record".to_owned()]);
    }

    /// The staging directory is a caller argument because one caller pair needs it, so an unusable
    /// one must fail the publication instead of quietly staging beside the record — that fallback
    /// would put a staged name inside the replicated namespace (HC-R05). Proven with a staging
    /// path that is a regular file, which no uid can turn into a directory.
    #[test]
    fn staging_happens_in_the_directory_the_caller_named() {
        let tmp = tempfile::tempdir().unwrap();
        let record = tmp.path().join("agents/host/worker/harness-context");
        let staging = tmp.path().join("control/staging");
        replace(
            &record,
            b"{}\n",
            Staging::new(".harness-context").in_dir(&staging),
            Durability::Rename,
        )
        .unwrap();
        assert_eq!(fs::read(&record).unwrap(), b"{}\n");
        assert!(entries(&staging).is_empty());
        assert_eq!(
            entries(record.parent().unwrap()),
            vec!["harness-context".to_owned()]
        );

        let blocked = tmp.path().join("blocked");
        fs::write(&blocked, b"not a directory").unwrap();
        assert!(
            replace(
                &record,
                b"{}\n",
                Staging::new(".harness-context").in_dir(&blocked),
                Durability::Rename,
            )
            .is_err()
        );
    }

    /// The refusal that makes a staging file in an agent-writable directory safe: an agent can
    /// plant a symlink there, and following it would aim st2's own privilege at a file the agent
    /// cannot write.
    #[test]
    fn a_planted_symlink_at_the_staging_path_is_refused_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("authored");
        fs::write(&victim, b"authored bytes").unwrap();
        let planted = tmp.path().join(".record.tmp-planted");
        symlink(&victim, &planted).unwrap();

        let refused = create_staging(&planted).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&victim).unwrap(), b"authored bytes");
    }

    /// [`Durability::FsyncFileAndDir`] is observable exactly here: a parent directory that cannot
    /// be opened for its sync fails the publication, while [`Durability::Rename`] — which never
    /// opens the directory — succeeds against the same directory. Real only for a non-root uid;
    /// the hermetic gate runs as the sandbox's unprivileged build user, and a local root run skips
    /// the edge instead of asserting what root cannot observe.
    #[test]
    fn a_directory_that_cannot_be_synced_fails_only_the_strict_level() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("agent");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("record");

        // Write and traverse, but not read: staging and renaming still work, opening the directory
        // to sync it does not.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o300)).unwrap();
        let strict = replace(
            &path,
            b"strict",
            Staging::new(".record"),
            Durability::FsyncFileAndDir,
        );
        let lenient = replace(
            &path,
            b"lenient",
            Staging::new(".record"),
            Durability::Rename,
        );
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(
            strict.unwrap_err().kind(),
            io::ErrorKind::PermissionDenied,
            "a directory that cannot be synced must fail a strict publication"
        );
        assert!(lenient.is_ok(), "{lenient:?}");
        assert_eq!(fs::read(&path).unwrap(), b"lenient");
    }
}
