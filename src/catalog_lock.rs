//! One catalog-authoring lock shared by declaration publishers and snapshot readers.
//!
//! The lock file is persistent by design. Removing it would split the lock domain when an existing
//! process still has the old inode open. State-plane traffic (messages, context, resources, status)
//! deliberately does not use this lock.

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const CONTROL_DIR: &str = ".st2";
pub const LOCK_FILE: &str = "catalog-authoring.lock";
pub const APPLY_MARKER: &str = "catalog-apply-incomplete";
pub const GENERATION_FILE: &str = "catalog-generation";
pub const GENERATION_INTENT_FILE: &str = "catalog-generation-incomplete";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogReadFence(Option<u64>);

pub fn read_fence(catalog: &Path) -> Result<CatalogReadFence> {
    let first = read_generation(catalog)?;
    ensure_authoring_complete(catalog)?;
    let second = read_generation(catalog)?;
    ensure_authoring_complete(catalog)?;
    anyhow::ensure!(
        first == second,
        "catalog generation changed while sampling declaration state"
    );
    Ok(CatalogReadFence(second))
}

fn advance_generation(control_file: &File) -> Result<()> {
    let current = read_generation_from_control(control_file)?.unwrap_or(0);
    let next = current
        .checked_add(1)
        .context("catalog generation counter exhausted")?;
    let control = crate::catalog_transaction::retained_dir_path(control_file)?;
    let target = control.join(GENERATION_FILE);
    match fs::symlink_metadata(&target) {
        Ok(metadata) => anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "catalog generation is not a real regular file: {}",
            target.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect catalog generation"),
    }
    let mut temp = tempfile::Builder::new()
        .prefix("catalog-generation-")
        .tempfile_in(&control)?;
    use std::io::Write as _;
    writeln!(temp, "{next}")?;
    temp.as_file().sync_all()?;
    temp.persist(&target).map_err(|error| error.error)?;
    control_file.sync_all()?;
    Ok(())
}

fn read_generation(catalog: &Path) -> Result<Option<u64>> {
    let Some((control, _control_path)) = retained_control(catalog)? else {
        return Ok(None);
    };
    read_generation_from_control(&control)
}

pub(crate) fn read_generation_token(catalog: &Path) -> Result<Option<u64>> {
    read_generation(catalog)
}

fn read_generation_from_control(control: &File) -> Result<Option<u64>> {
    let control_path = crate::catalog_transaction::retained_dir_path(control)?;
    let path = control_path.join(GENERATION_FILE);
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("open catalog generation"),
    };
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file(),
        "catalog generation is not a real regular file: {}",
        path.display()
    );
    use std::io::Read as _;
    let mut value = String::new();
    file.read_to_string(&mut value)?;
    let value = value
        .strip_suffix('\n')
        .context("catalog generation is missing its newline terminator")?;
    Ok(Some(value.parse().context("parse catalog generation")?))
}

fn ensure_authoring_complete(catalog: &Path) -> Result<()> {
    let Some((_control, control_path)) = retained_control(catalog)? else {
        return Ok(());
    };
    for (name, message) in [
        (APPLY_MARKER, "catalog apply is incomplete"),
        (
            GENERATION_INTENT_FILE,
            "catalog declaration commit is incomplete",
        ),
    ] {
        let marker = control_path.join(name);
        match fs::symlink_metadata(&marker) {
            Ok(_) => anyhow::bail!("{message}: marker present at {}", marker.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect catalog marker {}", marker.display()));
            }
        }
    }
    Ok(())
}

fn retained_control(catalog: &Path) -> Result<Option<(File, PathBuf)>> {
    let root = crate::catalog_transaction::open_dir_beneath(catalog, catalog)?;
    let file = match crate::catalog_transaction::openat_dir_nofollow(
        &root,
        std::ffi::OsStr::new(CONTROL_DIR),
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("open catalog control directory"),
    };
    let path = crate::catalog_transaction::retained_dir_path(&file)?;
    Ok(Some((file, path)))
}

#[derive(Debug, Clone, Copy)]
enum Mode {
    Shared,
    Exclusive,
}

/// Whether an acquisition may queue behind the current holder.
#[derive(Debug, Clone, Copy)]
enum Wait {
    /// Block until the holder releases. Every authoring verb the operator invokes.
    Block,
    /// Report contention instead of queueing. The supervisor's maintenance work uses this: a
    /// reconcile pass that blocked on `st2 catalog apply` would stall every live agent's
    /// reconciliation for the length of the apply.
    Now,
}

/// An advisory catalog-authoring lock. Dropping the guard releases the kernel lock.
#[derive(Debug)]
pub struct CatalogLock {
    file: File,
    control: File,
    root: File,
}

impl CatalogLock {
    pub fn shared(catalog: &Path) -> Result<Self> {
        Self::blocking(catalog, Mode::Shared, false, true)
    }

    /// Acquire the existing catalog lock without initializing missing control state.
    pub(crate) fn shared_existing(catalog: &Path) -> Result<Self> {
        Self::blocking(catalog, Mode::Shared, false, false)
    }

    pub fn exclusive(catalog: &Path) -> Result<Self> {
        Self::blocking(catalog, Mode::Exclusive, false, true)
    }

    /// Acquire the exclusive lock only if it is free right now.
    ///
    /// `Ok(None)` means another holder has it, and the caller must skip its work rather than queue.
    pub fn try_exclusive(catalog: &Path) -> Result<Option<Self>> {
        Self::acquire(catalog, Mode::Exclusive, false, true, Wait::Now)
    }

    /// The whole-catalog transaction is the only operation allowed to inspect and recover an
    /// incomplete apply. Every other declaration reader/writer must keep using `shared` or
    /// `exclusive`, which fail closed while the marker exists.
    pub(crate) fn exclusive_for_catalog_apply(catalog: &Path) -> Result<Self> {
        Self::blocking(catalog, Mode::Exclusive, true, true)
    }

    /// A blocking acquisition returns the guard or an error — never contention.
    fn blocking(
        catalog: &Path,
        mode: Mode,
        allow_incomplete_apply: bool,
        initialize: bool,
    ) -> Result<Self> {
        Self::acquire(
            catalog,
            mode,
            allow_incomplete_apply,
            initialize,
            Wait::Block,
        )?
        .context("blocking catalog-authoring acquisition reported contention")
    }

    fn acquire(
        catalog: &Path,
        mode: Mode,
        allow_incomplete_apply: bool,
        initialize: bool,
        wait: Wait,
    ) -> Result<Option<Self>> {
        let catalog = catalog
            .canonicalize()
            .with_context(|| format!("canonicalize catalog root {}", catalog.display()))?;
        let metadata = fs::symlink_metadata(&catalog)
            .with_context(|| format!("read catalog root {}", catalog.display()))?;
        anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "catalog root is not a real directory: {}",
            catalog.display()
        );
        let root = crate::catalog_transaction::open_dir_beneath(&catalog, &catalog)
            .with_context(|| format!("open catalog root capability {}", catalog.display()))?;

        let control = catalog.join(CONTROL_DIR);
        let control_branch = match fs::symlink_metadata(&control) {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "catalog control path is not a real directory: {}",
                    control.display()
                );
                "observed"
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                anyhow::ensure!(
                    initialize,
                    "catalog control directory is absent: {}",
                    control.display()
                );
                test_control_creation_checkpoint();
                let branch = match fs::create_dir(&control) {
                    Ok(()) => {
                        test_control_created_checkpoint();
                        "created"
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let metadata = fs::symlink_metadata(&control).with_context(|| {
                            format!("re-read catalog control dir {}", control.display())
                        })?;
                        anyhow::ensure!(
                            metadata.is_dir() && !metadata.file_type().is_symlink(),
                            "catalog control path is not a real directory: {}",
                            control.display()
                        );
                        "raced"
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("create catalog control dir {}", control.display())
                        });
                    }
                };
                branch
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read catalog control dir {}", control.display()));
            }
        };
        // The control directory is the durability root for the persistent lock, apply stage, and
        // incomplete marker. Persist its catalog-parent entry before any caller can publish
        // declaration leaves. This is unconditional after observing a real control dir: its creator
        // may have crashed after mkdir but before its own parent fsync.
        root.sync_all()
            .with_context(|| format!("sync catalog root {}", catalog.display()))?;
        #[cfg(debug_assertions)]
        if let Ok(path) = std::env::var("ST2_TEST_CATALOG_CONTROL_BRANCH") {
            let _ = fs::write(path, control_branch);
        }

        let control_file = crate::catalog_transaction::openat_dir_nofollow(
            &root,
            std::ffi::OsStr::new(CONTROL_DIR),
        )
        .context("catalog control directory disappeared while acquiring its lock")?;
        let control = crate::catalog_transaction::retained_dir_path(&control_file)?;
        let path = control.join(LOCK_FILE);
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        if initialize {
            options.create(true);
        }
        let file = options
            .open(&path)
            .with_context(|| format!("open catalog authoring lock {}", path.display()))?;
        let operation = match mode {
            Mode::Shared => libc::LOCK_SH,
            Mode::Exclusive => libc::LOCK_EX,
        } | match wait {
            Wait::Block => 0,
            Wait::Now => libc::LOCK_NB,
        };
        #[cfg(debug_assertions)]
        if let Ok(path) = std::env::var("ST2_TEST_CATALOG_LOCK_ANY_ATTEMPT") {
            let value = match mode {
                Mode::Shared => b"shared".as_slice(),
                Mode::Exclusive => b"exclusive".as_slice(),
            };
            let _ = fs::write(path, value);
        }
        #[cfg(debug_assertions)]
        if matches!(mode, Mode::Exclusive)
            && let Ok(path) = std::env::var("ST2_TEST_CATALOG_LOCK_ATTEMPT")
        {
            let _ = fs::write(path, b"exclusive");
        }
        // SAFETY: `file` owns a valid descriptor for the duration of this call and the returned
        // guard. flock does not access Rust memory.
        let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            // `LOCK_NB` reports a live holder as `EWOULDBLOCK`. That is not a fault: the caller
            // asked to be told instead of queueing.
            if matches!(wait, Wait::Now) && error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error)
                .with_context(|| format!("lock catalog authoring lock {}", path.display()));
        }
        if !allow_incomplete_apply {
            let marker = control.join(APPLY_MARKER);
            match fs::symlink_metadata(&marker) {
                Ok(_) => anyhow::bail!(
                    "catalog apply is incomplete: marker present at {}",
                    marker.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("inspect catalog apply marker"),
            }
        }
        if matches!(mode, Mode::Shared) {
            let intent = control.join(GENERATION_INTENT_FILE);
            match fs::symlink_metadata(&intent) {
                Ok(_) => anyhow::bail!(
                    "catalog declaration commit is incomplete: marker present at {}",
                    intent.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("inspect catalog generation intent"),
            }
        }
        let lock = Self {
            file,
            control: control_file,
            root,
        };
        if matches!(mode, Mode::Exclusive) {
            lock.recover_generation_intent()?;
            test_lock_held_checkpoint();
        }
        Ok(Some(lock))
    }

    pub(crate) fn advance_generation(&self) -> Result<()> {
        advance_generation(&self.control)
    }

    pub(crate) fn generation(&self) -> Result<Option<u64>> {
        read_generation_from_control(&self.control)
    }

    pub(crate) fn root(&self) -> &File {
        &self.root
    }

    pub(crate) fn begin_generation_commit(&self) -> Result<GenerationCommit<'_>> {
        let control = crate::catalog_transaction::retained_dir_path(&self.control)?;
        let intent = control.join(GENERATION_INTENT_FILE);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&intent)
            .context("create catalog generation intent")?;
        use std::io::Write as _;
        file.write_all(b"pending\n")?;
        file.sync_all()?;
        self.control.sync_all()?;
        Ok(GenerationCommit { lock: self })
    }

    pub(crate) fn control(&self) -> &File {
        &self.control
    }

    fn recover_generation_intent(&self) -> Result<()> {
        let control = crate::catalog_transaction::retained_dir_path(&self.control)?;
        let intent = control.join(GENERATION_INTENT_FILE);
        match fs::symlink_metadata(&intent) {
            Ok(metadata) => anyhow::ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "catalog generation intent is not a real regular file"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("inspect catalog generation intent"),
        }
        self.advance_generation()?;
        fs::remove_file(&intent).context("clear recovered catalog generation intent")?;
        self.control.sync_all()?;
        Ok(())
    }
}

#[cfg(debug_assertions)]
fn test_lock_held_checkpoint() {
    let (Ok(ready), Ok(release)) = (
        std::env::var("ST2_TEST_CATALOG_LOCK_HELD_READY"),
        std::env::var("ST2_TEST_CATALOG_LOCK_HELD_RELEASE"),
    ) else {
        return;
    };
    let _ = fs::write(ready, b"ready");
    while !Path::new(&release).exists() {
        std::thread::yield_now();
    }
}

#[cfg(not(debug_assertions))]
fn test_lock_held_checkpoint() {}

pub(crate) struct GenerationCommit<'a> {
    lock: &'a CatalogLock,
}

impl GenerationCommit<'_> {
    pub(crate) fn commit(self) -> Result<()> {
        #[cfg(debug_assertions)]
        if std::env::var_os("ST2_TEST_GENERATION_FAIL_AFTER_COMMIT").is_some() {
            anyhow::bail!("injected post-commit generation failure");
        }
        self.lock.advance_generation()?;
        let control = crate::catalog_transaction::retained_dir_path(&self.lock.control)?;
        fs::remove_file(control.join(GENERATION_INTENT_FILE))
            .context("clear catalog generation intent")?;
        self.lock.control.sync_all()?;
        Ok(())
    }
}

#[cfg(debug_assertions)]
fn test_control_creation_checkpoint() {
    use std::time::{Duration, Instant};

    let (Ok(ready), Ok(release)) = (
        std::env::var("ST2_TEST_CATALOG_CONTROL_READY"),
        std::env::var("ST2_TEST_CATALOG_CONTROL_RELEASE"),
    ) else {
        return;
    };
    let _ = fs::write(ready, b"ready");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !Path::new(&release).exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(not(debug_assertions))]
fn test_control_creation_checkpoint() {}

#[cfg(debug_assertions)]
fn test_control_created_checkpoint() {
    use std::time::{Duration, Instant};

    let (Ok(ready), Ok(release)) = (
        std::env::var("ST2_TEST_CATALOG_CONTROL_CREATED_READY"),
        std::env::var("ST2_TEST_CATALOG_CONTROL_CREATED_RELEASE"),
    ) else {
        return;
    };
    let _ = fs::write(ready, b"ready");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !Path::new(&release).exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(not(debug_assertions))]
fn test_control_created_checkpoint() {}

impl Drop for CatalogLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor remains valid until after Drop returns.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub fn lock_path(catalog: &Path) -> PathBuf {
    catalog.join(CONTROL_DIR).join(LOCK_FILE)
}

pub fn apply_marker_path(catalog: &Path) -> PathBuf {
    catalog.join(CONTROL_DIR).join(APPLY_MARKER)
}
