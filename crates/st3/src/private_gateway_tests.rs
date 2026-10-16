use super::*;

#[test]
fn private_state_and_socket_derive_a_private_gateway() {
    let mut config = Config::default();
    let default_gateway = config.client_gateway_socket.clone();
    config.state_dir = "/tmp/private-state".into();
    select_private_gateway(&mut config, true, false).unwrap();
    assert_eq!(
        config.client_gateway_socket,
        PathBuf::from("/tmp/private-state/st3-client.sock")
    );
    config.socket = "/tmp/private-socket/api.sock".into();
    select_private_gateway(&mut config, true, true).unwrap();
    assert_eq!(
        config.client_gateway_socket,
        PathBuf::from("/tmp/private-state/st3-client.sock")
    );
    assert_ne!(config.client_gateway_socket, default_gateway);
    select_private_gateway(&mut config, false, true).unwrap();
    assert_eq!(
        config.client_gateway_socket,
        PathBuf::from("/tmp/private-socket/st3-client.sock")
    );
}

#[test]
fn default_daemon_keeps_its_default_gateway() {
    let mut config = Config::default();
    let gateway = config.client_gateway_socket.clone();
    select_private_gateway(&mut config, false, false).unwrap();
    assert_eq!(config.client_gateway_socket, gateway);
}

#[test]
fn private_socket_in_runtime_dir_refuses_shared_default_gateway() {
    let mut config = Config::default();
    config.socket = config.socket.with_file_name("private-st.sock");
    let error = select_private_gateway(&mut config, false, true).unwrap_err();
    let collision = error.downcast_ref::<PrivateGatewayCollision>().unwrap();
    assert_eq!(collision.derived, Config::default().client_gateway_socket);
    assert!(error.to_string().contains("pass --client-gateway-socket"));
}

#[test]
fn private_state_takes_priority_over_runtime_socket() {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.state_dir = root.path().join("private-state");
    config.socket = config.socket.with_file_name("private-st.sock");
    select_private_gateway(&mut config, true, true).unwrap();
    assert_eq!(
        config.client_gateway_socket,
        config.state_dir.join("st3-client.sock")
    );
    assert!(!config.state_dir.exists());
}

#[test]
fn gateway_comparison_resolves_symlinked_runtime_directory() {
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("runtime");
    fs::create_dir(&runtime).unwrap();
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&runtime, &alias).unwrap();
    assert_eq!(
        gateway_path_for_comparison(&runtime.join("st3-client.sock")).unwrap(),
        gateway_path_for_comparison(&alias.join("st3-client.sock")).unwrap()
    );
    assert!(!runtime.join("st3-client.sock").exists());
}

#[test]
fn gateway_comparison_resolves_ancestor_above_missing_runtime_directory() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    fs::create_dir(&real).unwrap();
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    assert_eq!(
        gateway_path_for_comparison(&real.join("missing/run/st3-client.sock")).unwrap(),
        gateway_path_for_comparison(&alias.join("missing/run/st3-client.sock")).unwrap()
    );
    assert!(!real.join("missing").exists());
}

#[test]
fn gateway_comparison_normalizes_missing_suffix_after_resolving_symlinks() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    fs::create_dir_all(real.join("deep")).unwrap();
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(real.join("deep"), &alias).unwrap();
    let target = real.join("run/st3-client.sock");
    for path in [
        alias.join("missing/../../run/st3-client.sock"),
        alias.join("./missing/.././../run/st3-client.sock"),
        alias.join("missing/../other/../../run/st3-client.sock"),
    ] {
        assert_eq!(
            gateway_path_for_comparison(&path).unwrap(),
            gateway_path_for_comparison(&target).unwrap()
        );
    }
    // After canceling a missing component, resolve an existing symlink before parent traversal.
    std::os::unix::fs::symlink(real.join("deep"), real.join("link")).unwrap();
    assert_eq!(
        gateway_path_for_comparison(&real.join("missing/../link/../run/st3-client.sock"))
            .unwrap(),
        gateway_path_for_comparison(&target).unwrap()
    );
    assert!(!real.join("deep/missing").exists());
    assert!(!real.join("missing").exists());
    assert!(!real.join("run").exists());
}
