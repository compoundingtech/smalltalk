//! Bounded, metadata-only enumeration rooted in one native session directory.
//!
//! Directory handles, rather than canonicalized path strings, are the confinement boundary.
use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path};

pub const MAX_ENTRIES: usize = 10_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub directory: bool,
}

#[derive(Debug)]
pub struct Listing {
    pub entries: Vec<Entry>,
    pub device: u64,
    pub inode: u64,
    pub modified_seconds: i64,
    pub modified_nanoseconds: i64,
    pub changed_seconds: i64,
    pub changed_nanoseconds: i64,
}

#[derive(Debug)]
pub enum Error {
    InvalidDirectory,
    InvalidPrefix,
    ScanLimit,
    Changed,
    Io(io::Error),
}
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self { Self::Io(error) }
}

/// Enumerate a single directory. Hidden entries, symlinks and special files are never returned.
/// The caller owns actor/session fences and pagination; this function never accepts a new root
/// from an HTTP request and never opens or reads an entry's contents.
pub fn list(root: &Path, identity: &st_drivers::harness_inventory::WorkspaceIdentity,
    directory: &str, prefix: &str) -> Result<Listing, Error> {
    if directory.len() > 4096 || Path::new(directory).is_absolute() {
        return Err(Error::InvalidDirectory);
    }
    if prefix.len() > 255 || prefix.contains(['/', '\0']) || prefix.starts_with('.') {
        return Err(Error::InvalidPrefix);
    }
    if !root.is_absolute() { return Err(Error::InvalidDirectory) }
    let mut handle = OpenOptions::new().read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open("/")?;
    for component in root.components() {
        match component {
            Component::RootDir => {},
            Component::Normal(name) => {
                use std::os::unix::ffi::OsStrExt as _;
                let name = CString::new(name.as_bytes()).map_err(|_| Error::InvalidDirectory)?;
                handle = open_directory(&handle, &name)?;
            },
            _ => return Err(Error::InvalidDirectory),
        }
    }
    let root_metadata = handle.metadata()?;
    if identity.device.parse::<u64>().ok() != Some(root_metadata.dev())
        || identity.inode.parse::<u64>().ok() != Some(root_metadata.ino()) {
        return Err(Error::Changed);
    }
    for component in Path::new(directory).components() {
        let Component::Normal(name) = component else { return Err(Error::InvalidDirectory) };
        let Some(name) = name.to_str() else { return Err(Error::InvalidDirectory) };
        if name.starts_with('.') || name.is_empty() { return Err(Error::InvalidDirectory) }
        let name = CString::new(name).map_err(|_| Error::InvalidDirectory)?;
        handle = open_directory(&handle, &name)?;
    }
    let before = handle.metadata()?;
    let mut entries = Vec::new();
    let mut reader = DirectoryReader::new(&handle)?;
    let mut count = 0;
    while let Some(name) = reader.next()? {
        count += 1;
        if count > MAX_ENTRIES { return Err(Error::ScanLimit) }
        let Ok(name) = String::from_utf8(name) else { continue };
        if name.starts_with('.') || !name.starts_with(prefix) { continue }
        // fstatat inspects entry metadata without following symlinks.
        let name_c = CString::new(name.as_str()).map_err(|_| Error::InvalidDirectory)?;
        let mode = entry_mode(&handle, &name_c)?;
        let directory = match mode & libc::S_IFMT {
            libc::S_IFDIR => true,
            libc::S_IFREG => false,
            _ => continue,
        };
        entries.push(Entry { name, directory });
    }
    let after = handle.metadata()?;
    if (before.dev(), before.ino(), before.mtime(), before.mtime_nsec(), before.ctime(), before.ctime_nsec())
        != (after.dev(), after.ino(), after.mtime(), after.mtime_nsec(), after.ctime(), after.ctime_nsec()) {
        return Err(Error::Changed);
    }
    entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok(Listing {
        entries, device: after.dev(), inode: after.ino(),
        modified_seconds: after.mtime(), modified_nanoseconds: after.mtime_nsec(),
        changed_seconds: after.ctime(), changed_nanoseconds: after.ctime_nsec(),
    })
}

fn open_directory(parent: &File, name: &CStr) -> io::Result<File> {
    // SAFETY: name is NUL-terminated and parent remains open across openat. Only directory
    // handles are admitted; O_NOFOLLOW refuses every symlink component, including replacements.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(),
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 { return Err(io::Error::last_os_error()) }
    // SAFETY: openat returned a fresh owned descriptor, transferred exactly once.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn entry_mode(parent: &File, name: &CStr) -> io::Result<libc::mode_t> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstatat writes one stat into valid storage; name is NUL-terminated. The no-follow
    // flag guarantees metadata is for the entry itself, even if another process replaces it.
    let result = unsafe { libc::fstatat(parent.as_raw_fd(), name.as_ptr(), metadata.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
    if result < 0 { return Err(io::Error::last_os_error()) }
    // SAFETY: a successful fstatat initialized all fields.
    Ok(unsafe { metadata.assume_init() }.st_mode)
}

struct DirectoryReader(*mut libc::DIR);
impl DirectoryReader {
    fn new(directory: &File) -> io::Result<Self> {
        // openat(".") creates an independent directory offset, unlike dup().
        let name = c".";
        let handle = open_directory(directory, name)?;
        use std::os::fd::IntoRawFd;
        let fd = handle.into_raw_fd();
        // SAFETY: fd is a fresh directory descriptor. fdopendir takes ownership on success.
        let reader = unsafe { libc::fdopendir(fd) };
        if reader.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: fdopendir failed and left ownership with the caller.
            unsafe { libc::close(fd); }
            return Err(error);
        }
        Ok(Self(reader))
    }
    fn next(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            // SAFETY: this DIR is exclusively borrowed and remains open; errno is thread-local.
            unsafe { *errno_location() = 0; }
            // SAFETY: self owns a valid DIR. Returned memory is consumed before the next readdir.
            let entry = unsafe { libc::readdir(self.0) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                return if error.raw_os_error() == Some(0) { Ok(None) } else { Err(error) };
            }
            // SAFETY: POSIX readdir provides a NUL-terminated name in d_name.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if matches!(name.to_bytes(), b"." | b"..") { continue }
            return Ok(Some(name.to_bytes().to_vec()));
        }
    }
}
impl Drop for DirectoryReader {
    fn drop(&mut self) {
        // SAFETY: this object owns the DIR and closes it exactly once.
        unsafe { libc::closedir(self.0); }
    }
}
#[cfg(target_os = "linux")]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: libc returns this thread's valid errno pointer.
    unsafe { libc::__errno_location() }
}
#[cfg(target_os = "macos")]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: libc returns this thread's valid errno pointer.
    unsafe { libc::__error() }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity(root: &Path) -> st_drivers::harness_inventory::WorkspaceIdentity {
        let metadata = std::fs::metadata(root).unwrap();
        st_drivers::harness_inventory::WorkspaceIdentity {
            device: metadata.dev().to_string(), inode: metadata.ino().to_string(),
        }
    }
    #[test]
    fn root_visible_metadata_is_filtered_without_following_external_links() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("alpha.rs"), "visible").unwrap();
        std::fs::write(root.path().join("beta.rs"), "visible").unwrap();
        std::fs::write(root.path().join(".env"), "private").unwrap();
        std::fs::create_dir(root.path().join("alpha-dir")).unwrap();
        std::fs::create_dir(root.path().join(".state")).unwrap();
        std::fs::write(outside.path().join("outside.rs"), "never read").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("external")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("outside.rs"), root.path().join("external.rs")).unwrap();
        assert_eq!(list(root.path(), &identity(root.path()), "", "").unwrap().entries, vec![
            Entry { name: "alpha-dir".into(), directory: true },
            Entry { name: "alpha.rs".into(), directory: false },
            Entry { name: "beta.rs".into(), directory: false },
        ]);
        assert_eq!(list(root.path(), &identity(root.path()), "", "alpha.").unwrap().entries,
            vec![Entry { name: "alpha.rs".into(), directory: false }]);
        assert!(matches!(list(root.path(), &identity(root.path()), "../", ""), Err(Error::InvalidDirectory)));
        assert!(matches!(list(root.path(), &identity(root.path()), "/absolute", ""), Err(Error::InvalidDirectory)));
        assert!(matches!(list(root.path(), &identity(root.path()), ".state", ""), Err(Error::InvalidDirectory)));
        assert!(matches!(list(root.path(), &identity(root.path()), "external", ""), Err(Error::Io(_))));
    }
    #[test]
    fn scan_ceiling_counts_hidden_entries_even_when_filter_matches_nothing() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..=MAX_ENTRIES {
            std::fs::write(root.path().join(format!(".hidden-{index}")), "").unwrap();
        }
        assert!(matches!(list(root.path(), &identity(root.path()), "", "missing"), Err(Error::ScanLimit)));
    }
    #[test]
    fn replaced_root_or_symlink_ancestor_cannot_substitute_an_outside_workspace() {
        let base = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let parent = base.path().join("parent");
        let root = parent.join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir(outside.path().join("workspace")).unwrap();
        std::fs::write(outside.path().join("workspace/private"), "never enumerate").unwrap();
        let native = identity(&root);
        std::fs::rename(&parent, base.path().join("original")).unwrap();
        std::os::unix::fs::symlink(outside.path(), &parent).unwrap();
        assert!(matches!(list(&root, &native, "", ""), Err(Error::Io(_))));
        std::fs::remove_file(&parent).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        assert!(matches!(list(&root, &native, "", ""), Err(Error::Changed)));
    }
}
