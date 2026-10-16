use std::fs;
use std::io::{Cursor, Read as _};
use std::path::{Component, Path};

use anyhow::{Context as _, Result};
use tar::{Archive, Builder, EntryType, Header};
use walkdir::WalkDir;

pub fn archive_eval(root: &Path) -> Result<Vec<u8>> {
    anyhow::ensure!(root.is_dir(), "eval {} is not a directory", root.display());
    let mut files = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if entry.path() == root {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "eval contains symbolic link {}",
            entry.path().display()
        );
        anyhow::ensure!(
            metadata.is_dir() || metadata.is_file(),
            "eval contains special file {}",
            entry.path().display()
        );
        if metadata.is_file() {
            let relative = entry.path().strip_prefix(root)?.to_path_buf();
            files.push((relative, entry.path().to_path_buf(), metadata));
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let mut bytes = Vec::new();
    {
        let mut builder = Builder::new(&mut bytes);
        for (relative, path, metadata) in files {
            let data = fs::read(&path)?;
            let mut header = Header::new_gnu();
            header.set_entry_type(EntryType::Regular);
            header.set_size(data.len() as u64);
            header.set_mode(mode(&metadata));
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            header.set_cksum();
            builder.append_data(&mut header, relative, Cursor::new(data))?;
        }
        builder.finish()?;
    }
    Ok(bytes)
}

pub fn hydrate_eval(bytes: &[u8], destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    let mut archive = Archive::new(Cursor::new(bytes));
    for entry in archive.entries()? {
        let mut entry = entry?;
        anyhow::ensure!(
            entry.header().entry_type().is_file(),
            "eval archive contains a non-file"
        );
        let path = entry.path()?.into_owned();
        validate_relative(&path)?;
        let mapped = map_git(path.as_path());
        let output = destination.join(mapped);
        anyhow::ensure!(
            output.starts_with(destination),
            "eval archive path escapes its workspace"
        );
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        fs::write(&output, data).with_context(|| format!("write {}", output.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = entry.header().mode()? & 0o777;
            fs::set_permissions(&output, fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}

fn validate_relative(path: &Path) -> Result<()> {
    anyhow::ensure!(
        !path.is_absolute(),
        "eval archive contains an absolute path"
    );
    anyhow::ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "eval archive contains an unsafe path {}",
        path.display()
    );
    Ok(())
}

fn map_git(path: &Path) -> std::path::PathBuf {
    path.components()
        .map(|component| match component {
            Component::Normal(name) if name == "_git" => std::ffi::OsStr::new(".git"),
            Component::Normal(name) => name,
            _ => unreachable!("validated path has only normal components"),
        })
        .collect()
}

#[cfg(unix)]
fn mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn mode(_metadata: &fs::Metadata) -> u32 {
    0o644
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_is_deterministic_and_maps_git() {
        let source = tempfile::tempdir().unwrap();
        fs::create_dir(source.path().join("_git")).unwrap();
        fs::write(source.path().join("_git/config"), "test").unwrap();
        fs::write(source.path().join("eval.kdl"), "version 2\n").unwrap();
        let one = archive_eval(source.path()).unwrap();
        let two = archive_eval(source.path()).unwrap();
        assert_eq!(one, two);
        let output = tempfile::tempdir().unwrap();
        hydrate_eval(&one, output.path()).unwrap();
        assert_eq!(
            fs::read_to_string(output.path().join(".git/config")).unwrap(),
            "test"
        );
    }
}
