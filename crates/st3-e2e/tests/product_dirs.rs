use st3_client::{device, discover_unix_endpoint};

mod tests {
    use st3_client::product_dirs::{MIGRATION_ID, RECEIPT_FILE};
    use std::path::PathBuf;

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
                    "tests::xdg_environment_reaches_config_and_state_readers",
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
}
