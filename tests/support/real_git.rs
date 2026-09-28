use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

/// Find git for fixtures that deliberately run with a restricted PATH.
pub fn on_path(path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|directory| directory.join("git"))
        .find(|candidate| is_real_git(candidate))
}

fn in_recorder_directory(path: &Path) -> bool {
    path.components()
        .any(|part| matches!(part, Component::Normal(name) if name == "recorder"))
}

fn is_real_git(candidate: &Path) -> bool {
    if in_recorder_directory(candidate) {
        return false;
    }
    let Ok(metadata) = fs::metadata(candidate) else {
        return false;
    };
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return false;
    }
    let Ok(resolved) = fs::canonicalize(candidate) else {
        return false;
    };
    if in_recorder_directory(&resolved) || resolved.file_name() == Some(OsStr::new("st3")) {
        return false;
    }
    fs::read(candidate).is_ok_and(|bytes| {
        !bytes
            .windows(b"st2-recorder-wrapper".len())
            .any(|window| window == b"st2-recorder-wrapper")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn skips_recorder_directories_and_st3_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let recorder = temp.path().join("recorder/bin");
        let disguised = temp.path().join("disguised");
        let real = temp.path().join("real");
        for directory in [&recorder, &disguised, &real] {
            fs::create_dir_all(directory).unwrap();
        }
        let st3 = temp.path().join("st3");
        fs::write(&st3, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&st3, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(recorder.join("git"), "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(recorder.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&st3, disguised.join("git")).unwrap();
        let git = real.join("git");
        fs::write(&git, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([&recorder, &disguised, &real]).unwrap();
        assert_eq!(on_path(&path), Some(git));
    }
}
