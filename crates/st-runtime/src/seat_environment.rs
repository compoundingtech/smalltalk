//! Private, restartable PTY environment overlays. Values never enter launcher argv.

use std::collections::BTreeMap;
use std::fs::{DirBuilder, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

// The PTY persists this wrapper, not the environment values, for manual restart.
pub(crate) const RESTORE_ENVIRONMENT: &str = ". \"$1\" || exit; shift; exec \"$@\"";

/// Replace a seat's overlay on its next launch; retain it while PTY can restart the seat.
/// `identity` is a digest, never a path supplied by the seat.
pub(crate) fn write_environment(
    directory: &Path,
    identity: &str,
    environment: &BTreeMap<String, String>,
) -> Result<PathBuf> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .context("create private seat environment directory")?;
    let metadata = std::fs::symlink_metadata(directory)?;
    anyhow::ensure!(
        metadata.is_dir() && metadata.mode() & 0o777 == 0o700
            && metadata.uid() == unsafe { libc::geteuid() },
        "seat environment directory must be owned by this user with mode 0700"
    );
    let path = directory.join(format!("{identity}.env"));
    let temporary = directory.join(format!(
        ".{identity}.{}-{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .context("create private seat environment file")?;
    let result = (|| {
        for (key, value) in environment {
            // PTY always supplies its own session identity, overriding explicit overlays.
            if key == "PTY_SESSION" {
                continue;
            }
            anyhow::ensure!(
                !key.is_empty()
                    && key.bytes().enumerate().all(|(index, byte)| {
                        byte == b'_' || byte.is_ascii_alphabetic()
                            || (index > 0 && byte.is_ascii_digit())
                    })
                    && !value.contains('\0'),
                "seat environment contains a key or value unsupported by the shell"
            );
            // Single quotes are the only shell metacharacter inside a single-quoted value.
            // Closing the quote, quoting one apostrophe, then reopening is lossless even
            // for substitutions, backticks, newlines and trailing whitespace.
            writeln!(file, "export {key}='{}'", value.replace('\'', "'\\''"))?;
        }
        file.flush()?;
        std::fs::rename(&temporary, &path).context("publish private seat environment file")?;
        Ok(path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn private_environment_roundtrips_hostile_values_and_restarts_without_values_in_argv() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("private");
        let value = "$(touch injected) `touch injected` '\" $HOME\nsecond line  ";
        let environment = BTreeMap::from([
            ("ROUNDTRIP".into(), value.into()),
            ("EMPTY".into(), String::new()),
            ("PTY_SESSION".into(), "must-not-win".into()),
        ]);
        let path = write_environment(&directory, "seat", &environment).unwrap();
        assert_eq!(std::fs::metadata(&directory).unwrap().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        let mut command = Command::new("sh");
        command.args(["-c", RESTORE_ENVIRONMENT, "st seat"])
            .arg(&path)
            .args(["sh", "-c", "printf '%s\\0%s\\0%s' \"$ROUNDTRIP\" \"$EMPTY\" \"$PTY_SESSION\""])
            .current_dir(root.path())
            .env("PTY_SESSION", "actual-session");
        assert!(command.get_args().all(|argument| !argument.to_string_lossy().contains(value)));
        for _ in 0..2 {
            let output = command.output().unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, format!("{value}\0\0actual-session").as_bytes());
            assert!(!root.path().join("injected").exists());
        }
        write_environment(&directory, "seat", &BTreeMap::from([("ROUNDTRIP".into(), "replacement".into())])).unwrap();
        let output = command.output().unwrap();
        assert_eq!(output.stdout, b"replacement\0\0actual-session");
        assert_eq!(std::fs::read_dir(directory).unwrap().count(), 1);
    }

    #[test]
    fn refuses_shell_injection_in_keys_without_retaining_partial_files() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("private");
        assert!(write_environment(&directory, "seat", &BTreeMap::from([
            ("KEY; touch injected".into(), "unused".into()),
        ])).is_err());
        assert_eq!(std::fs::read_dir(directory).unwrap().count(), 0);
    }
}
