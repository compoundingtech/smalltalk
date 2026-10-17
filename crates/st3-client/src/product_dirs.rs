//! Product directory discovery. Resolution never creates or moves directories.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

// LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-16-state-dirs-fractal — DELETE at contraction — #3032
/// A destination-local receipt: exact migration ID followed by a newline.
/// Only the later migration's independently verified completion may write it.
pub const RECEIPT_FILE: &str = ".fractal-migration-completed";
pub const MIGRATION_ID: &str = "arn:lmig:smalltalk:2026-10-16-state-dirs-fractal";
static WARNED: AtomicBool = AtomicBool::new(false);

/// Locate completed migrated data; unreceipted destinations never become authoritative.
/// With neither directory present, callers still create the legacy directory.
pub fn resolve(old: PathBuf, new: PathBuf) -> PathBuf {
    if !new.exists() {
        return old;
    }
    if !completed(&new) {
        if !WARNED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "st: {} has no completed migration receipt; using {}",
                new.display(), old.display()
            );
        }
        return old;
    }
    new
}

fn completed(directory: &Path) -> bool {
    std::fs::read_to_string(directory.join(RECEIPT_FILE))
        .is_ok_and(|receipt| receipt.strip_suffix('\n') == Some(MIGRATION_ID))
}

pub fn config_dir() -> PathBuf {
    product_dir(&xdg_base("XDG_CONFIG_HOME", ".config"))
}

pub fn state_dir() -> PathBuf {
    product_dir(&xdg_base("XDG_STATE_HOME", ".local/state"))
}

fn product_dir(base: &Path) -> PathBuf {
    resolve(base.join("st3"), base.join("fractal"))
}

pub fn instance_root(home: &Path, name: &str) -> PathBuf {
    resolve(
        home.join(".st-instance").join(name),
        home.join(".fractal-instance").join(name),
    )
}

fn xdg_base(variable: &str, suffix: &str) -> PathBuf {
    base(std::env::var_os("HOME"), std::env::var_os(variable), suffix)
}

fn base(
    home: Option<std::ffi::OsString>,
    override_path: Option<std::ffi::OsString>,
    suffix: &str,
) -> PathBuf {
    override_path.map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(home.unwrap_or_else(|| ".".into())).join(suffix)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_environment_reaches_config_and_state_readers() {
        const CHILD: &str = "ST_PRODUCT_DIRS_TEST_CHILD";
        if let Some(root) = std::env::var_os(CHILD) {
            let root = PathBuf::from(root);
            let selected = std::env::var("ST_PRODUCT_DIRS_TEST_SELECTED").unwrap();
            let config_dir = root.join("config").join(&selected);
            let state_dir = root.join("state").join(&selected);
            assert_eq!(st3::config::Config::default_path(), config_dir.join("config.toml"));
            assert_eq!(st3::config::Config::default().state_dir, state_dir);
            assert_eq!(
                crate::discover_unix_endpoint(None).unwrap(),
                st3::config::Config::default().socket
            );
            assert_eq!(
                crate::device::profile_path().unwrap(),
                config_dir.join("stui-devices.json")
            );
            let path = st3::config::Config::default_path();
            st3::config::Config::default().initialize_install_config(&path).unwrap();
            assert_eq!(st3::config::Config::load(None).unwrap().state_dir, state_dir);
            assert_eq!(
                crate::discover_unix_endpoint(None).unwrap(),
                st3::config::Config::default().socket
            );
            return;
        }
        for receipted in [false, true] {
            let root = tempfile::tempdir().unwrap();
            for path in ["config/fractal", "config/st3", "state/fractal", "state/st3", "home"] {
                std::fs::create_dir_all(root.path().join(path)).unwrap();
            }
            if receipted {
                for path in ["config/fractal", "state/fractal"] {
                    std::fs::write(
                        root.path().join(path).join(RECEIPT_FILE),
                        format!("{MIGRATION_ID}\n"),
                    ).unwrap();
                }
            }
            let (selected, ignored) = if receipted { ("fractal", "st3") } else { ("st3", "fractal") };
            std::fs::write(
                root.path().join("config").join(ignored).join("config.toml"),
                "socket = '/wrong-daemon.sock'\n",
            ).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "product_dirs::tests::xdg_environment_reaches_config_and_state_readers",
                    "--nocapture",
                ])
                .env(CHILD, root.path())
                .env("ST_PRODUCT_DIRS_TEST_SELECTED", selected)
                .env("HOME", root.path().join("home"))
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_STATE_HOME", root.path().join("state"))
                .env_remove("ST_INSTANCE")
                .env_remove("XDG_RUNTIME_DIR")
                .output()
                .unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            assert!(root.path().join("config").join(selected).join("config.toml").exists());
        }
    }

    #[test]
    fn existing_directory_matrix_and_receipts() {
        for (old_exists, new_exists, receipt, expected_new) in [
            (false, false, None, false),
            (true, false, None, false),
            (false, true, None, false),
            (true, true, None, false),
            (true, true, Some("incomplete\n"), false),
            (true, true, Some("another-migration\n"), false),
            (true, true, Some(MIGRATION_ID), false),
        ] {
            let root = tempfile::tempdir().unwrap();
            let old = root.path().join("st3");
            let new = root.path().join("fractal");
            if old_exists {
                std::fs::create_dir(&old).unwrap();
            }
            if new_exists {
                std::fs::create_dir(&new).unwrap();
            }
            if let Some(receipt) = receipt {
                std::fs::write(new.join(RECEIPT_FILE), receipt).unwrap();
            }
            assert_eq!(
                resolve(old.clone(), new.clone()),
                if expected_new { new } else { old }
            );
        }
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("st3");
        let new = root.path().join("fractal");
        std::fs::create_dir(&old).unwrap();
        std::fs::create_dir(&new).unwrap();
        std::fs::write(new.join(RECEIPT_FILE), format!("{MIGRATION_ID}\n")).unwrap();
        assert_eq!(resolve(old, new.clone()), new);
    }

    #[test]
    fn instance_resolution_is_per_name() {
        let home = tempfile::tempdir().unwrap();
        let old = home.path().join(".st-instance/try");
        let new = home.path().join(".fractal-instance/try");
        assert_eq!(instance_root(home.path(), "try"), old);
        std::fs::create_dir_all(&old).unwrap();
        assert_eq!(instance_root(home.path(), "try"), old);
        std::fs::create_dir_all(&new).unwrap();
        assert_eq!(instance_root(home.path(), "try"), old);
        std::fs::write(new.join(RECEIPT_FILE), format!("{MIGRATION_ID}\n")).unwrap();
        assert_eq!(instance_root(home.path(), "try"), new);
        assert_eq!(
            instance_root(home.path(), "other"),
            home.path().join(".st-instance/other")
        );
        std::fs::remove_dir_all(&old).unwrap();
        assert_eq!(instance_root(home.path(), "try"), new);
        std::fs::remove_file(new.join(RECEIPT_FILE)).unwrap();
        assert_eq!(instance_root(home.path(), "try"), old);
    }

    #[test]
    fn xdg_overrides_define_both_candidates_without_environment_mutation() {
        let home = tempfile::tempdir().unwrap();
        for suffix in [".config", ".local/state"] {
            let default = base(Some(home.path().into()), None, suffix);
            assert_eq!(default, home.path().join(suffix));
            let override_path = home.path().join(format!("override-{}", suffix.replace('/', "-")));
            let selected = base(Some(home.path().into()), Some(override_path.clone().into()), suffix);
            assert_eq!(selected, override_path);
            assert_eq!(product_dir(&selected), selected.join("st3"));
            std::fs::create_dir_all(selected.join("fractal")).unwrap();
            assert_eq!(product_dir(&selected), selected.join("st3"));
            std::fs::write(
                selected.join("fractal").join(RECEIPT_FILE),
                format!("{MIGRATION_ID}\n"),
            ).unwrap();
            assert_eq!(product_dir(&selected), selected.join("fractal"));
            assert!(!default.exists());
        }
    }
}
// LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-16-state-dirs-fractal
