//! Optional Linux proof with two private API daemons, a real PTY and the phone's TS/native code.
use super::*;
use crate::model::ClaimInput;
use smallclaims::sync::{FleetContext, peer_router};
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use tokio::net::TcpListener;

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Terminal(st_runtime::PtyRuntime);
impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.0.stop("owner-proof");
        let _ = self.0.remove("owner-proof");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires FABRIC_BIN, OWNER_BRIDGE_BIN, Node and pty on an isolated build host"]
async fn terminal_route_phone_client_isolated_owner_pair() -> Result<()> {
    let fabric = std::env::var("FABRIC_BIN")?;
    let bridge = std::env::var("OWNER_BRIDGE_BIN")?;
    let version = Command::new(&fabric).arg("--version").output()?;
    assert_eq!(String::from_utf8(version.stdout)?.trim(), "0.2.41+84c3208");
    let root = tempfile::Builder::new()
        .prefix("ios-owner-pair-")
        .tempdir()?;
    let make_state = |name: &str| -> Result<crate::api::AppState> {
        let path = root.path().join(name);
        fs::create_dir(&path)?;
        Ok(crate::api::AppState {
            store: Arc::new(Store::open(&path.join("graph.db"), name)?),
            notify: Arc::new(tokio::sync::Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: name.into(),
            state_dir: path.clone(),
            pty_root: path.join("pty"),
            pty_binary: PathBuf::from("pty"),
            fleet_id: None,
            configured_peers: vec![],
            client_relay: None,
            native_session_home: None,
            planner_default: Default::default(),
        })
    };
    let mut gateway = make_state("gateway-demo")?;
    let mut owner = make_state("owner-demo")?;
    let key_file = root.path().join("phone.key");
    let identity = Command::new(&bridge)
        .arg(&key_file)
        .arg("--identity")
        .output()?;
    assert!(identity.status.success());
    let phone = String::from_utf8(identity.stdout)?.trim().to_owned();
    let home = root.path().join("fabric");
    fs::create_dir(&home)?;
    let command = |args: &[&str]| -> Result<String> {
        let out = Command::new(&fabric)
            .arg("--home")
            .arg(&home)
            .args(args)
            .output()?;
        anyhow::ensure!(
            out.status.success(),
            "isolated fabric failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(String::from_utf8(out.stdout)?)
    };
    let node = command(&["id"])?.trim().to_owned();
    let fleet = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
    let service = format!("st3/client/{fleet}");
    command(&["add", &phone, "demo-phone", "--inbound-only", "--allow", &service])?;
    let _fabric = Process(
        Command::new(&fabric)
            .arg("--home")
            .arg(&home)
            .args(["up", "--foreground"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let addr: Value = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(out) = command(&["addr"]) {
                break serde_json::from_str::<Value>(&out);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await??;
    let runtime = st_runtime::PtyRuntime::new(owner.pty_root.clone()).with_binary("pty");
    let _terminal = Terminal(runtime.clone());
    let out = Command::new("pty").env("PTY_ROOT", &owner.pty_root).args(["run", "-d", "--force", "--id", "owner-proof", "--tag", "keep=true", "--", "/bin/sh", "-c", "stty -echo; printf 'isolated owner ready'; while IFS= read -r line; do printf '\\r\\naccepted:%s' \"$line\"; done"]).output()?;
    anyhow::ensure!(
        out.status.success(),
        "PTY failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let live = runtime
        .snapshot()?
        .into_iter()
        .find(|item| item.name == "owner-proof")
        .context("running isolated PTY")?;
    let incarnation = format!("{}:{}", live.pid.unwrap(), live.created_at.unwrap());
    owner.store.append_claim(&ClaimInput { subject: "agent/demo/owner-proof".into(), kind: "runtime.observed".into(), actor: None,
        fields: serde_json::from_value(serde_json::json!({"status":"running", "runtime_id":"owner-proof", "terminal":true, "incarnation_id":incarnation}))?,
        evidence: vec![], expected_subject: None, idempotency_key: None })?;
    gateway
        .store
        .import_replication("owner-demo", &owner.store.export_replication(0)?)?;
    // Ordinary local pairing, completed through the public client API, delegates only read scopes.
    let gateway_socket = gateway.state_dir.join("st.sock");
    let (ready, waiting) = tokio::sync::oneshot::channel();
    let socket = gateway_socket.clone();
    let app = crate::api::router(gateway.clone());
    let gateway_task = tokio::spawn(async move {
        crate::api::serve_unix_with_ready(&socket, app, || {
            let _ = ready.send(());
        })
        .await
    });
    waiting.await?;
    let local = st3_client::Client::unix_as(&gateway_socket, "person/demo");
    let challenge = local
        .pairing_begin(&st3_client::PairingBegin {
            api_version: "st3.client.v0".into(),
            device_name: "Isolated owner proof".into(),
            person_id: "person/demo".into(),
            full_control: None,
            scopes: Some(vec!["read.projections".into(), "terminal.read".into()]),
        })
        .await?
        .value;
    let paired = local
        .pairing_complete(
            &challenge.pairing_id,
            &st3_client::PairingComplete {
                api_version: "st3.client.v0".into(),
                code: challenge.code,
                device_public_key: "isolated-read-only-device-key-00000000".into(),
                key_storage: None,
            },
        )
        .await?
        .value;
    assert!(
        !paired
            .scopes
            .iter()
            .any(|scope| scope == "terminal.control")
    );
    owner
        .store
        .import_replication("gateway-demo", &gateway.store.export_replication(0)?)?;
    let member = Arc::new(MemberKey::generate()?.0);
    owner.store.bind_fleet(fleet)?;
    owner.store.pin_fleet_anchor(member.public())?;
    owner.store.set_member_key(Some(member.clone()))?;
    owner
        .store
        .admit_fleet_anchor(fleet, member.public(), "listening")?;
    owner.store.publish_fleet_endpoints(
        "listening",
        &[serde_json::json!({"transport":"fabric", "node":node})],
        "isolated proof",
    )?;
    gateway.store.bind_fleet(fleet)?;
    gateway.store.pin_fleet_anchor(member.public())?;
    // Legacy graph import copies claims, not their sealed member signatures. Transfer the
    // owner's admission and endpoint envelopes through the ordinary verified exchange.
    let exchange = owner.store.export_replication_exchange(
        fleet,
        &smallclaims::replication::ReplicationInventory::default(),
    )?;
    gateway.store.receive_replication_exchange("owner-demo", fleet, &exchange)?;
    gateway.store.validate_replication_backlog()?;
    gateway.store.project_replication_backlog()?;
    let membership = gateway.store.fleet_view_sealed()?;
    anyhow::ensure!(
        membership.members.iter().any(|member| member.name == "owner-demo"
            && member.state == "current"
            && member.endpoints.iter().any(|endpoint| endpoint["node"] == node)),
        "the isolated gateway must see the owner's sealed admission and endpoint: {membership:?}"
    );
    owner.fleet_id = Some(fleet.into());
    gateway.fleet_id = Some(fleet.into());
    let owner_socket = owner.state_dir.join("st.sock");
    let (ready, waiting) = tokio::sync::oneshot::channel();
    let socket = owner_socket.clone();
    let app = crate::api::router(owner.clone());
    let owner_task = tokio::spawn(async move {
        crate::api::serve_unix_with_ready(&socket, app, || {
            let _ = ready.send(());
        })
        .await
    });
    waiting.await?;
    let peer = PeerState::new(
        MainBackend::new(owner_socket),
        "owner-demo".into(),
        FleetAuth::test("fleet-test", &[7; 32]),
        FleetContext::legacy(BTreeSet::from(["gateway-demo".into()])),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let peer_address = listener.local_addr()?;
    let peer_task =
        tokio::spawn(
            async move { axum::serve(listener, peer_router(peer, smalltalk_routes())).await },
        );
    let secret = gateway.state_dir.join("fleet-secret");
    fs::write(&secret, [7; 32])?;
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600))?;
    gateway.client_relay = ClientRelay::from_config(&Config {
        node: "gateway-demo".into(),
        fleet_id: Some("fleet-test".into()),
        shared_secret_file: Some(secret),
        peers: vec![PeerConfig {
            name: "owner-demo".into(),
            url: format!("http://{peer_address}"),
        }],
        ..Default::default()
    })?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let owner_address = listener.local_addr()?;
    let owner_api =
        tokio::spawn(async move { axum::serve(listener, crate::api::fabric_router(owner)).await });
    command(&[
        "expose",
        &service,
        "--tcp",
        &owner_address.to_string(),
        "--ephemeral",
    ])?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let gateway_address = listener.local_addr()?;
    let gateway_api =
        tokio::spawn(
            async move { axum::serve(listener, crate::api::fabric_router(gateway)).await },
        );
    let descriptor = root.path().join("pair.json");
    fs::write(
        &descriptor,
        serde_json::to_vec(
            &serde_json::json!({"kind":"isolated-owner-terminal-pair", "bridge":bridge,
        "key":key_file, "credential":paired.credential, "gateway":format!("http://{gateway_address}"), "node":node,
        "addr":addr, "terminal":"terminal/agent/demo/owner-proof", "incarnation":incarnation}),
        )?,
    )?;
    fs::set_permissions(&descriptor, fs::Permissions::from_mode(0o600))?;
    let repo = Path::new(test_env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut child = tokio::process::Command::new("node")
        .current_dir(&repo)
        .args([
            "--experimental-transform-types",
            "--import",
            "./apps/ios/testHooks.mjs",
            "apps/ios/proofs/owner-terminal.mjs",
        ])
        .arg(&descriptor)
        .kill_on_drop(true)
        .spawn()?;
    let status = tokio::time::timeout(Duration::from_secs(90), child.wait()).await??;
    for task in [gateway_task, owner_task] {
        task.abort();
    }
    peer_task.abort();
    owner_api.abort();
    gateway_api.abort();
    anyhow::ensure!(status.success(), "phone-side owner proof failed");
    Ok(())
}
