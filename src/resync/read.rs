//! Bounded, symlink-refusing reads of a watched carrier.
//!
//! Moved verbatim out of the parent module: the confined open, the hash, and the classification
//! of an open error into carrier state.

use std::path::Path;

use sha2::{Digest as _, Sha256};

use super::*;

pub(super) fn read_state(path: &Path, containment_root: Option<&Path>) -> std::io::Result<CarrierState> {
    match containment_root {
        Some(root) => read_confined(path, root),
        None => read_regular(path),
    }
}

pub(super) fn diagnose_read_error(path: &Path, error: &std::io::Error) {
    eprintln!(
        "st2: resync read for '{}' failed transiently; retrying: {error}",
        path.display()
    );
}

fn hash_reader(mut file: std::fs::File) -> std::io::Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = std::io::Read::read(&mut file, &mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(unix)]
fn classify_open_error(error: std::io::Error) -> std::io::Result<CarrierState> {
    match error.raw_os_error() {
        Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP) => Ok(CarrierState::Missing),
        _ => Err(error),
    }
}

#[cfg(not(unix))]
fn classify_open_error(error: std::io::Error) -> std::io::Result<CarrierState> {
    if error.kind() == std::io::ErrorKind::NotFound {
        Ok(CarrierState::Missing)
    } else {
        Err(error)
    }
}

#[cfg(unix)]
fn read_regular(path: &Path) -> std::io::Result<CarrierState> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) => return classify_open_error(error),
    };
    if !file.metadata()?.file_type().is_file() {
        return Ok(CarrierState::Missing);
    }
    hash_reader(file).map(CarrierState::Present)
}

#[cfg(not(unix))]
fn read_regular(path: &Path) -> std::io::Result<CarrierState> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) => return classify_open_error(error),
    };
    if !file.metadata()?.file_type().is_file() {
        return Ok(CarrierState::Missing);
    }
    hash_reader(file).map(CarrierState::Present)
}

#[cfg(unix)]
fn read_confined(path: &Path, root: &Path) -> std::io::Result<CarrierState> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;

    let invalid_path = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "carrier path is outside its confinement root or has an unsafe component",
        )
    };
    let relative = path.strip_prefix(root).map_err(|_| invalid_path())?;
    let mut components = relative.components().peekable();
    if components.peek().is_none() {
        return Ok(CarrierState::Missing);
    }

    // Open the confinement root component-by-component from the filesystem root. `O_NOFOLLOW`
    // on one full pathname protects only its final component; descriptor-relative traversal
    // protects every ancestor from symlink replacement as well.
    let mut root_components = root.components();
    if root_components.next() != Some(std::path::Component::RootDir) {
        return Err(invalid_path());
    }
    let slash = CString::new("/").map_err(|_| invalid_path())?;
    // SAFETY: `slash` is NUL-terminated and the returned descriptor is checked before ownership.
    let filesystem_root = unsafe {
        libc::open(
            slash.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if filesystem_root < 0 {
        return classify_open_error(std::io::Error::last_os_error());
    }
    // SAFETY: `filesystem_root` is newly owned after the non-negative check.
    let mut directory = unsafe { OwnedFd::from_raw_fd(filesystem_root) };
    for component in root_components {
        let std::path::Component::Normal(name) = component else {
            return Err(invalid_path());
        };
        let name = CString::new(name.as_bytes()).map_err(|_| invalid_path())?;
        let flags = libc::O_RDONLY
            | libc::O_DIRECTORY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK;
        // SAFETY: the live directory descriptor and NUL-terminated component are valid.
        let opened = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if opened < 0 {
            return classify_open_error(std::io::Error::last_os_error());
        }
        // SAFETY: `opened` is newly owned after the non-negative check.
        directory = unsafe { OwnedFd::from_raw_fd(opened) };
    }

    while let Some(component) = components.next() {
        let std::path::Component::Normal(name) = component else {
            return Err(invalid_path());
        };
        let name = CString::new(name.as_bytes()).map_err(|_| invalid_path())?;
        let last = components.peek().is_none();
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if last { 0 } else { libc::O_DIRECTORY };
        // SAFETY: both the live directory descriptor and NUL-terminated component are valid;
        // `O_NOFOLLOW` makes each lookup fail closed if that component is replaced by a symlink.
        let opened = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if opened < 0 {
            return classify_open_error(std::io::Error::last_os_error());
        }
        // SAFETY: `opened` is a newly-owned descriptor after the non-negative check above.
        let opened = unsafe { OwnedFd::from_raw_fd(opened) };
        if last {
            let file = std::fs::File::from(opened);
            if !file.metadata()?.file_type().is_file() {
                return Ok(CarrierState::Missing);
            }
            return hash_reader(file).map(CarrierState::Present);
        }
        directory = opened;
    }
    Ok(CarrierState::Missing)
}

#[cfg(not(unix))]
fn read_confined(_path: &Path, _root: &Path) -> std::io::Result<CarrierState> {
    // No std API can atomically enforce no-follow traversal. Fail closed on unsupported hosts.
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "descriptor-relative no-follow reads are unavailable",
    ))
}
