//! Capability-style filesystem primitives for the catalog control plane.
//!
//! Moved verbatim out of the parent module: every `openat`/`renameat` call the transaction makes
//! through a retained directory descriptor, so no path is re-resolved between check and use.
//! Named `capability` rather than `fs` because the parent imports `std::fs`.

use std::fs::{self, File, OpenOptions};
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use anyhow::{Context as _, Result};

use super::*;

pub(super) fn openat_nofollow(parent: &File, name: &std::ffi::OsStr) -> Result<File> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let name = CString::new(name.as_bytes()).context("source entry name contains NUL")?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("open retained source entry");
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(super) fn capability_dir_entries(dir: &File) -> Result<Vec<std::ffi::OsString>> {
    let path = retained_dir_path(dir)?;
    let mut names = fs::read_dir(&path)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

pub(crate) fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("open directory {}", path.display()))?
        .sync_all()
        .with_context(|| format!("sync directory {}", path.display()))
}

fn open_dir_nofollow(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

pub(crate) fn open_dir_beneath(catalog: &Path, target: &Path) -> std::io::Result<File> {
    let relative = target.strip_prefix(catalog).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "directory escapes catalog",
        )
    })?;
    let mut current = open_dir_nofollow(catalog)?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "directory has an unsafe component",
            ));
        };
        current = openat_dir_nofollow(&current, name)?;
    }
    Ok(current)
}

pub(crate) fn openat_dir_nofollow(parent: &File, name: &std::ffi::OsStr) -> std::io::Result<File> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    let name = CString::new(name.as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "directory name contains NUL",
        )
    })?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

pub(super) fn control_plane_rename_error(error: std::io::Error) -> anyhow::Error {
    if error.raw_os_error() == Some(libc::EXDEV) {
        anyhow::anyhow!(
            "catalog control and declaration planes must share one filesystem for atomic publication"
        )
    } else {
        error.into()
    }
}

pub(crate) fn persist_tempfile_from_control(
    control: &File,
    catalog: &Path,
    temp: tempfile::NamedTempFile,
    target: &Path,
) -> std::io::Result<()> {
    let source = temp.path();
    let target_parent = open_dir_beneath(
        catalog,
        target.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "target has no parent")
        })?,
    )?;
    let source_name = source.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "temporary file has no name",
        )
    })?;
    let target_name = target.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "target has no name")
    })?;
    renameat(control, source_name, &target_parent, target_name)
}

pub(crate) fn link_tempfile_from_control(
    control: &File,
    catalog: &Path,
    temp: &tempfile::NamedTempFile,
    target: &Path,
) -> std::io::Result<()> {
    let source = temp.path();
    let target_parent = open_dir_beneath(
        catalog,
        target.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "target has no parent")
        })?,
    )?;
    let source_name = source.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "temporary file has no name",
        )
    })?;
    let target_name = target.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "target has no name")
    })?;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    let source_name = CString::new(source_name.as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "source name contains NUL")
    })?;
    let target_name = CString::new(target_name.as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "target name contains NUL")
    })?;
    let result = unsafe {
        libc::linkat(
            control.as_raw_fd(),
            source_name.as_ptr(),
            target_parent.as_raw_fd(),
            target_name.as_ptr(),
            0,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub(crate) fn rename_noreplace_between_dirs(
    control: &File,
    catalog: &Path,
    source: &Path,
    target: &Path,
) -> std::io::Result<()> {
    let target_parent = open_dir_beneath(
        catalog,
        target.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "target has no parent")
        })?,
    )?;
    let source_name = source.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "source has no name")
    })?;
    let target_name = target.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "target has no name")
    })?;
    renameat_noreplace(control, source_name, &target_parent, target_name)
}

pub(super) fn renameat_noreplace(
    source_parent: &File,
    source: &std::ffi::OsStr,
    target_parent: &File,
    target: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let source = CString::new(source.as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "source name contains NUL")
    })?;
    let target = CString::new(target.as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "target name contains NUL")
    })?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let result = unsafe {
        libc::renameat2(
            source_parent.as_raw_fd(),
            source.as_ptr(),
            target_parent.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            source_parent.as_raw_fd(),
            source.as_ptr(),
            target_parent.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    let result = {
        let _ = (source_parent, source, target_parent, target);
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "atomic no-replace directory rename is unsupported on this platform",
        ));
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub(super) fn renameat(
    source_parent: &File,
    source: &std::ffi::OsStr,
    target_parent: &File,
    target: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let source = CString::new(source.as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "source name contains NUL")
    })?;
    let target = CString::new(target.as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "target name contains NUL")
    })?;
    let result = unsafe {
        libc::renameat(
            source_parent.as_raw_fd(),
            source.as_ptr(),
            target_parent.as_raw_fd(),
            target.as_ptr(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub(crate) fn rename_noreplace(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "source contains NUL")
    })?;
    let target = CString::new(target.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "target contains NUL")
    })?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    let result = {
        let _ = (source, target);
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "atomic no-replace directory rename is unsupported on this platform",
        ));
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
