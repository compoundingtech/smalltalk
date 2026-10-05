//! Descriptor-relative, no-follow access to configured private-notes catalogs.
//! Unsafe code is confined to converting successful owned POSIX descriptors into `File`.
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Component, Path};

pub(crate) const CARRIER: &str = "private-notes.md";

pub(crate) fn open_directory(path: &Path) -> io::Result<File> {
    if !path.is_absolute() { return Err(io::Error::other("notes catalog must be absolute")); }
    let mut directory = File::open("/")?;
    for component in path.components() {
        match component {
            Component::RootDir => {},
            Component::Normal(name) => {
                directory = open_at(&directory, name.as_bytes(), libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
            },
            _ => return Err(io::Error::other("notes catalog contains an unsafe component")),
        }
    }
    owner(&directory)?;
    Ok(directory)
}

pub(crate) fn child_directory(parent: &File, name: &str) -> io::Result<File> {
    let directory = open_at(parent, name.as_bytes(), libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    owner(&directory)?;
    Ok(directory)
}

pub(crate) fn read_regular(parent: &File, name: &str, max_bytes: usize) -> io::Result<Vec<u8>> {
    let mut file = open_at(parent, name.as_bytes(), libc::O_RDONLY | libc::O_NONBLOCK, 0)?;
    read_file(&mut file, max_bytes)
}

fn read_file(file: &mut File, max_bytes: usize) -> io::Result<Vec<u8>> {
    owner(&file)?;
    if !file.metadata()?.is_file() { return Err(io::Error::other("notes source must be a regular file")); }
    let mut bytes = Vec::new();
    std::io::Read::by_ref(file).take(max_bytes as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes { return Err(io::Error::other("notes source exceeds client limit")); }
    Ok(bytes)
}

pub(crate) struct Snapshot {
    pub bytes: Option<Vec<u8>>,
    pub revision: String,
}

fn file_revision(file: &File, bytes: &[u8]) -> io::Result<String> {
    use sha2::{Digest as _, Sha256};
    let metadata = file.metadata()?;
    let mut digest = Sha256::new();
    for field in [metadata.dev(), metadata.ino(), metadata.mtime() as u64, metadata.mtime_nsec() as u64] {
        digest.update(field.to_be_bytes());
    }
    digest.update(bytes);
    Ok(hex::encode(digest.finalize()))
}

pub(crate) fn read_carrier(parent: &File, max_bytes: usize) -> io::Result<Snapshot> {
    match open_at(parent, CARRIER.as_bytes(), libc::O_RDONLY | libc::O_NONBLOCK, 0) {
        Ok(mut file) => {
            let bytes = read_file(&mut file, max_bytes)?;
            let revision = file_revision(&file, &bytes)?;
            Ok(Snapshot { bytes: Some(bytes), revision })
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            owner(parent)?;
            Ok(Snapshot { bytes: None, revision: "absent".into() })
        },
        Err(error) => Err(error),
    }
}

pub(crate) fn replace(parent: &File, operation: &str, markdown: &[u8], record_expected: impl FnOnce(&str) -> io::Result<()>) -> io::Result<()> {
    // Reprove both carrier and destination directory immediately before the mutation.
    owner(parent)?;
    match open_at(parent, CARRIER.as_bytes(), libc::O_RDONLY | libc::O_NONBLOCK, 0) {
        Ok(file) => {
            owner(&file)?;
            if !file.metadata()?.is_file() { return Err(io::Error::other("notes carrier must be a regular file")); }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {},
        Err(error) => return Err(error),
    }
    let sibling = format!(".private-notes-{operation}.pending");
    let name = cstring(sibling.as_bytes())?;
    // SAFETY: descriptor/name are valid; exact retry unlinks only its own unrenamed sibling.
    unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0); }
    let mut file = open_at(parent, sibling.as_bytes(), libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o600)?;
    let result = (|| {
        file.write_all(markdown)?;
        file.sync_all()?;
        record_expected(&file_revision(&file, markdown)?)?;
        let from = cstring(sibling.as_bytes())?;
        let to = cstring(CARRIER.as_bytes())?;
        // SAFETY: both names are NUL-terminated single components; parent owns a live descriptor.
        if unsafe { libc::renameat(parent.as_raw_fd(), from.as_ptr(), parent.as_raw_fd(), to.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        parent.sync_all()?;
        if read_regular(parent, CARRIER, markdown.len())? != markdown {
            return Err(io::Error::other("notes read-back differs from replacement"));
        }
        Ok(())
    })();
    if result.is_err() {
        let name = cstring(sibling.as_bytes())?;
        // SAFETY: descriptor and name remain valid; cleanup never follows a symlink.
        unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0); }
    }
    result
}

pub(crate) fn owner(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    // SAFETY: getuid has no inputs and no memory safety requirements.
    if metadata.uid() != unsafe { libc::getuid() } || metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "notes carrier or source is not exclusively owned by the local OS owner"));
    }
    Ok(())
}

fn lock_file(path: &Path) -> io::Result<File> {
    let file = std::fs::OpenOptions::new().read(true).write(true).create(true)
        .truncate(false).mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)?;
    owner(&file)?;
    if !file.metadata()?.is_file() { return Err(io::Error::other("notes lock must be a regular file")); }
    Ok(file)
}

pub(crate) fn local_lock(path: &Path) -> io::Result<File> {
    let file = lock_file(path)?;
    file.lock()?;
    Ok(file)
}

pub(crate) fn local_try_lock(path: &Path) -> io::Result<Option<File>> {
    let file = lock_file(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

fn cstring(name: &[u8]) -> io::Result<CString> {
    if name.is_empty() || name.contains(&b'/') || name == b"." || name == b".." {
        return Err(io::Error::other("unsafe notes component"));
    }
    CString::new(name).map_err(|_| io::Error::other("unsafe notes component"))
}

fn open_at(parent: &File, name: &[u8], flags: i32, mode: libc::mode_t) -> io::Result<File> {
    let name = cstring(name)?;
    // SAFETY: name is NUL-terminated and the directory descriptor is borrowed for this call.
    let descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC, mode) };
    if descriptor < 0 { return Err(io::Error::last_os_error()); }
    // SAFETY: openat returned a new descriptor, whose sole ownership is transferred to File.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}
