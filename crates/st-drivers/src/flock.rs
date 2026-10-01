//! The file-lock transport: `flock` on a named lock file, released when the guard drops.
//!
//! Every persistent cross-process lock in this tree was the same three system calls — open the lock
//! file, `flock` it, close it — hand-rolled around wildly different protocols, each with its own
//! `unsafe` block and its own `impl Drop`. This module owns those three calls, and deliberately
//! nothing else:
//!
//! * It returns [`std::io::Result`] and never `anyhow`, so every caller keeps the exact context
//!   string its own protocol reports. Wrapping here would flatten eight distinct diagnostics into
//!   one and make the lock file's identity unrecoverable from the message.
//! * `Ok(None)` means contention and nothing else. Every other failure is an `Err`, so a caller
//!   cannot mistake a lock file it may not open for a lock file somebody else is holding.
//! * It does not create directories, does not `fsync`, and does not name lock files. Those are
//!   protocol: the callers legitimately disagree about all three (one creates the parent, one
//!   requires the parent to be `0700` first, one names its lock by suffixing a foreign config
//!   path), and absorbing the disagreement here would hide it rather than resolve it.
//! * [`open`] and [`FileLock::hold`] stay split, because the window *between* the open and the
//!   `flock` is load-bearing: [`crate::catalog_lock`] writes its contention checkpoints there, and
//!   [`crate::resource_profile`] proves the opened lock is a regular file there.
//!
//! The defaults are [`crate::resource_profile`]'s publication lock, the one site that was already
//! correct on every axis, so they are house style rather than invention: `O_RDWR` (a path that
//! cannot be opened for write is not usable as this tree's lock file, and a shared lock is still
//! taken on a writable descriptor), `O_NOFOLLOW` so a symlink planted at the lock path cannot
//! redirect the lock domain to a file its planter also holds, `O_CLOEXEC` — which Rust's
//! `OpenOptions` already sets, named in the call only so a reader does not go looking for it — and
//! creation mode `0600`.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

/// Whether the acquisition wants the reader or the writer side of the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Shared,
    Exclusive,
}

/// Whether an acquisition may queue behind the current holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wait {
    /// Block until the holder releases. Every authoring verb the operator invokes.
    Block,
    /// Report contention as `Ok(None)` instead of queueing. The supervisor's maintenance work uses
    /// this: a reconcile pass that blocked on `st2 catalog apply` would stall every live agent's
    /// reconciliation for the length of the apply.
    Now,
}

/// Whether [`open`] may bring the lock file into existence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Open {
    /// Create the lock file when it is missing. The common case: a lock file's only content is its
    /// inode, so first use creates it and nothing ever removes it — removing it would split the
    /// lock domain while an existing process still holds the old inode open.
    Create,
    /// Refuse with `AlreadyExists` when the lock file is already there. For a lock inside a
    /// directory this caller just built, where a pre-existing lock file means the directory is not
    /// what the caller thinks it is.
    CreateNew,
    /// Never create. `NotFound` then means "this protocol has never run here", which a read-only
    /// path can answer without writing into a tree it only inspects.
    Existing,
}

/// Open the lock file at `path` without taking the lock.
///
/// Split from [`FileLock::hold`] so a caller can inspect or announce the descriptor before it
/// blocks on the lock; see the module doc.
pub(crate) fn open(path: &Path, create: Open) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    match create {
        Open::Create => {
            options.create(true);
        }
        Open::CreateNew => {
            options.create_new(true);
        }
        Open::Existing => {}
    }
    options.open(path)
}

/// A held advisory file lock.
///
/// `flock` locks live on the open file description, so closing the descriptor already releases the
/// lock — which is what makes a crashed holder harmless. The explicit `LOCK_UN` in [`Drop`] pins
/// the release to the guard's scope even if the descriptor outlives it through a `dup`.
#[derive(Debug)]
pub(crate) struct FileLock {
    file: File,
}

impl FileLock {
    /// Take `mode` on an already-open lock file. `Ok(None)` is contention under [`Wait::Now`] and
    /// nothing else.
    pub(crate) fn hold(file: File, mode: Mode, wait: Wait) -> io::Result<Option<Self>> {
        let operation = match mode {
            Mode::Shared => libc::LOCK_SH,
            Mode::Exclusive => libc::LOCK_EX,
        } | match wait {
            Wait::Block => 0,
            Wait::Now => libc::LOCK_NB,
        };
        // SAFETY: `file` owns a valid descriptor for the duration of this call and the returned
        // guard. flock does not access Rust memory.
        let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
        if result != 0 {
            let error = io::Error::last_os_error();
            // `LOCK_NB` reports a live holder as `EWOULDBLOCK`. That is not a fault: the caller
            // asked to be told instead of queueing.
            if matches!(wait, Wait::Now) && error.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error);
        }
        Ok(Some(Self { file }))
    }

    /// Take `mode`, queueing behind the current holder. Returns the guard directly: [`Wait::Block`]
    /// never sets `LOCK_NB`, so contention is not one of its outcomes.
    pub(crate) fn hold_blocking(file: File, mode: Mode) -> io::Result<Self> {
        match Self::hold(file, mode, Wait::Block)? {
            Some(lock) => Ok(lock),
            None => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "a blocking file lock reported contention",
            )),
        }
    }

    /// The locked descriptor, for a protocol that also has to `fsync` the lock file itself.
    pub(crate) fn file(&self) -> &File {
        &self.file
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor remains valid until after Drop returns.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::{FileLock, Mode, Open, Wait, open};

    #[test]
    fn a_symlinked_lock_path_is_refused_instead_of_locking_its_target() {
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("outside");
        let link = temporary.path().join("lock");
        std::fs::write(&target, "unchanged").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        for create in [Open::Create, Open::Existing] {
            let error = open(&link, create).unwrap_err();
            assert_eq!(
                error.raw_os_error(),
                Some(libc::ELOOP),
                "O_NOFOLLOW must refuse a symlinked lock path, got {error}"
            );
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "unchanged");
    }

    #[test]
    fn contention_under_wait_now_is_ok_none_and_a_dropped_guard_releases() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("lock");

        let held = FileLock::hold_blocking(open(&path, Open::Create).unwrap(), Mode::Exclusive)
            .expect("an uncontended blocking acquisition succeeds");
        // `flock` locks the open file description, not the process, so a second open of the same
        // path contends with the guard above without any second thread.
        let contended = FileLock::hold(
            open(&path, Open::Create).unwrap(),
            Mode::Exclusive,
            Wait::Now,
        )
        .unwrap();
        assert!(contended.is_none(), "a live holder must read as contention");

        drop(held);
        let after = FileLock::hold(
            open(&path, Open::Create).unwrap(),
            Mode::Exclusive,
            Wait::Now,
        )
        .unwrap();
        assert!(after.is_some(), "dropping the guard must release the lock");
    }

    #[test]
    fn a_shared_holder_admits_a_second_reader_but_not_a_writer() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("lock");

        let _reader = FileLock::hold_blocking(open(&path, Open::Create).unwrap(), Mode::Shared)
            .expect("an uncontended shared acquisition succeeds");
        assert!(
            FileLock::hold(open(&path, Open::Create).unwrap(), Mode::Shared, Wait::Now)
                .unwrap()
                .is_some(),
            "a shared lock must not exclude another reader"
        );
        assert!(
            FileLock::hold(
                open(&path, Open::Create).unwrap(),
                Mode::Exclusive,
                Wait::Now
            )
            .unwrap()
            .is_none(),
            "a shared lock must exclude a writer"
        );
    }

    #[test]
    fn a_created_lock_file_is_private_to_its_owner() {
        let temporary = tempfile::tempdir().unwrap();
        for (name, create) in [("created", Open::Create), ("fresh", Open::CreateNew)] {
            let path = temporary.path().join(name);
            drop(open(&path, create).unwrap());
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600,
                "{name} lock file must be created private"
            );
        }
    }

    #[test]
    fn create_new_refuses_an_existing_lock_file_and_existing_refuses_a_missing_one() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("lock");
        assert_eq!(
            open(&path, Open::Existing).unwrap_err().kind(),
            std::io::ErrorKind::NotFound,
            "Open::Existing must not create the lock file"
        );
        assert!(!path.exists(), "a refused open must leave nothing behind");

        drop(open(&path, Open::Create).unwrap());
        assert_eq!(
            open(&path, Open::CreateNew).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        drop(open(&path, Open::Existing).unwrap());
    }
}
