use crate::diagnostics;
use crate::wire::{self, CHUNK, Frame, WINDOW};
use anyhow::{Context, Result, bail};
use iroh::{
    Endpoint, EndpointAddr, SecretKey,
    endpoint::{Connection, ConnectionError, presets},
};
use serde::Deserialize;
use std::{
    net::Ipv4Addr,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, mpsc},
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum PathMode {
    #[default]
    Auto,
    Direct,
    Relay,
}

pub struct Bridge {
    pub url: String,
    pub node: String,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl Bridge {
    #[cfg(test)]
    pub async fn start(key: SecretKey, target: EndpointAddr, service: String) -> Result<Self> {
        Self::start_with_mode(key, target, service, PathMode::Auto).await
    }

    #[cfg(test)]
    pub async fn start_with_mode(
        key: SecretKey,
        target: EndpointAddr,
        service: String,
        mode: PathMode,
    ) -> Result<Self> {
        let endpoint = Self::endpoint(key, mode).await?;
        Self::start_on(endpoint, target, service, "standalone".into(), true).await
    }

    pub async fn endpoint(key: SecretKey, mode: PathMode) -> Result<Endpoint> {
        let builder = Endpoint::builder(presets::N0).secret_key(key);
        let builder = match mode {
            PathMode::Auto => builder,
            PathMode::Direct => builder.clear_relay_transports(),
            PathMode::Relay => builder.clear_ip_transports(),
        };
        Ok(builder.bind().await?)
    }

    /// One fixed target listener. App-managed targets share an endpoint; only the manager may
    /// close that endpoint after its last target stops.
    pub async fn start_on(
        endpoint: Endpoint,
        target: EndpointAddr,
        service: String,
        handle: String,
        close_endpoint: bool,
    ) -> Result<Self> {
        validate_service(&service)?;
        let node = endpoint.id().to_string();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let url = format!("http://{}", listener.local_addr()?);
        let cancel = CancellationToken::new();
        let stopped = cancel.clone();
        let owned_endpoint = endpoint.clone();
        let task = tokio::spawn(async move {
            let slots = std::sync::Arc::new(Semaphore::new(16));
            let mut sessions = JoinSet::new();
            loop {
                tokio::select! {
                    _ = stopped.cancelled() => break,
                    Some(_) = sessions.join_next(), if !sessions.is_empty() => {},
                    accepted = listener.accept() => {
                        let Ok((local, _)) = accepted else { break };
                        // Reject excess local connections instead of queueing requests.
                        let Ok(permit) = slots.clone().try_acquire_owned() else { drop(local); continue };
                        let endpoint = owned_endpoint.clone();
                        let target = target.clone();
                        let service = service.clone();
                        let stopped = stopped.clone();
                        let handle = handle.clone();
                        sessions.spawn(async move {
                            let _permit = permit;
                            let _ = tunnel(endpoint, target, service, local, stopped, &handle).await;
                        });
                    }
                }
            }
            // Every tunnel owns its cleanup; cancel and join before closing the endpoint.
            while sessions.join_next().await.is_some() {}
            if close_endpoint {
                owned_endpoint.close().await;
            }
        });
        Ok(Self {
            url,
            node,
            cancel,
            task,
        })
    }

    pub async fn stop(self) {
        self.cancel.cancel();
        let _ = self.task.await;
    }
}

pub fn validate_service(service: &str) -> Result<()> {
    if service.is_empty()
        || service.len() > 255
        || service.contains(['\0', '\n'])
        || service.starts_with("git/")
    {
        bail!("invalid fabric exposure name");
    }
    Ok(())
}

async fn tunnel(
    endpoint: Endpoint,
    target: EndpointAddr,
    service: String,
    local: TcpStream,
    cancel: CancellationToken,
    handle: &str,
) -> Result<()> {
    let attempt = diagnostics::started(handle);
    let started = Instant::now();
    let connected = tokio::select! {
        _ = cancel.cancelled() => { diagnostics::ended(attempt, "cancelled", None); return Ok(()) },
        connected = tokio::time::timeout(Duration::from_secs(15), endpoint.connect(target, service.as_bytes())) => connected.context("fabric connect timed out").and_then(|r| r.map_err(Into::into)),
    };
    let connection = match connected {
        Ok(c) => c,
        Err(e) => {
            let (refused, code) = e
                .chain()
                .find_map(|error| error.downcast_ref::<ConnectionError>())
                .map_or((false, None), member_close);
            diagnostics::ended(
                attempt,
                if refused { "refused" } else { "connect-failed" },
                code,
            );
            return Err(e);
        }
    };
    diagnostics::connected(attempt, started.elapsed().as_millis() as u64, &connection);
    let pump = pump(&connection, local, cancel.clone());
    tokio::pin!(pump);
    let result = tokio::select! {
        result = &mut pump => result,
        _ = cancel.cancelled() => {
            // Give the peer a Close and final Acks before falling back to forced teardown.
            match tokio::time::timeout(Duration::from_secs(5), &mut pump).await {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!("fabric stop drain timed out")),
            }
        }
    };
    // pump waits for the final send bytes to be acknowledged on graceful completion.
    let (refused, closed_code) = connection
        .close_reason()
        .as_ref()
        .map_or((false, None), member_close);
    let rejected = result
        .as_ref()
        .err()
        .and_then(|error| error.downcast_ref::<Rejected>());
    let (outcome, code) = if refused {
        ("refused", closed_code)
    } else if let Some(Rejected(code)) = rejected {
        ("rejected", code.clone())
    } else if result.is_ok() {
        ("completed", None)
    } else {
        ("transport-ended", closed_code)
    };
    diagnostics::ended(attempt, outcome, code);
    connection.close(0u32.into(), b"phone tunnel ended");
    result
}

async fn pump(connection: &Connection, local: TcpStream, cancel: CancellationToken) -> Result<()> {
    let (mut send, mut recv) = connection.open_bi().await?;
    let mut id = [0u8; 16];
    // iroh's generated signing key supplies cryptographic randomness without another RNG API.
    id.copy_from_slice(&SecretKey::generate().to_bytes()[..16]);
    wire::write(
        &mut send,
        Frame::Hello {
            id,
            next: 0,
            resume: false,
        },
    )
    .await?;
    match tokio::time::timeout(Duration::from_secs(5), wire::read(&mut recv))
        .await
        .context("fabric Hello timed out")??
    {
        Frame::Hello {
            id: found,
            next: 0,
            resume: false,
        } if found == id => {}
        // Keep remote host-local paths out of app errors and logs: only the code.
        Frame::Error(text) => return Err(Rejected(wire::error_code(text.as_bytes())).into()),
        _ => bail!("invalid fabric server Hello"),
    }
    wire::write(&mut send, Frame::Ack(0)).await?;
    let (mut local_read, mut local_write) = local.into_split();
    let (frames_tx, mut frames) = mpsc::channel(2);
    let reader = tokio::spawn(async move {
        loop {
            let frame = wire::read(&mut recv).await;
            let failed = frame.is_err();
            if frames_tx.send(frame).await.is_err() || failed {
                break;
            }
        }
    });
    // Aborting a pump (background/stop) also aborts its frame reader.
    let _reader = AbortOnDrop(reader);
    let mut sent = 0u64;
    let mut acked = 0u64;
    let mut received = 0u64;
    let mut local_closed = false;
    let mut remote_close = None;
    let mut local_write_closed = false;
    let mut buffer = [0u8; CHUNK];
    loop {
        if local_closed && remote_close.is_some_and(|offset| offset <= received) && acked == sent {
            send.finish()?;
            // Unlike connection.close, stopped() waits for QUIC to acknowledge the send bytes.
            // The pinned member may close the connection immediately after completion
            // instead of delivering stream EOF. All application bytes are already Acked.
            if tokio::time::timeout(Duration::from_secs(3), send.stopped())
                .await
                .is_err()
                && connection.close_reason().is_none()
            {
                bail!("fabric final Ack drain timed out");
            }
            return Ok(());
        }
        tokio::select! {
            _ = cancel.cancelled(), if !local_closed => {
                local_closed = true;
                wire::write(&mut send, Frame::Close(sent)).await?;
            }
            read = local_read.read(&mut buffer), if !local_closed && sent - acked < WINDOW => {
                let length = read.unwrap_or(0);
                if length == 0 {
                    local_closed = true;
                    wire::write(&mut send, Frame::Close(sent)).await?;
                } else {
                    wire::write(&mut send, Frame::Data { offset: sent, bytes: buffer[..length].to_vec() }).await?;
                    sent = sent.checked_add(length as u64).context("fabric send offset overflow")?;
                }
            }
            frame = frames.recv() => {
                match frame.context("fabric reader ended")?? {
                    Frame::Ack(next) => acked = acked.max(next.min(sent)),
                    Frame::Close(offset) => remote_close = Some(offset),
                    Frame::Data { offset, bytes } => {
                        if offset > received { bail!("fabric data gap"); }
                        let duplicate = (received - offset).min(bytes.len() as u64) as usize;
                        let new = &bytes[duplicate..];
                        if local_write_closed && !new.is_empty() { bail!("fabric data after Close"); }
                        local_write.write_all(new).await?;
                        received = received.checked_add(new.len() as u64).context("fabric receive offset overflow")?;
                        wire::write(&mut send, Frame::Ack(received)).await?;
                    }
                    Frame::Hello { .. } | Frame::Error(_) => bail!("unexpected fabric frame after Hello"),
                }
                if !local_write_closed && remote_close.is_some_and(|offset| offset <= received) {
                    local_write.shutdown().await?;
                    local_write_closed = true;
                }
            }
        }
    }
}

/// A member's application close of the connection: whether it is a refusal
/// (code 403: trust or a grant said no) and its fabric error code, if it sent one.
fn member_close(error: &ConnectionError) -> (bool, Option<String>) {
    match error {
        ConnectionError::ApplicationClosed(close) => (
            close.error_code == 403u32.into(),
            wire::error_code(&close.reason),
        ),
        _ => (false, None),
    }
}

/// The member answered the Hello with an Error frame: a fault such as
/// `service_unavailable` or `session_limit_reached`, not a refusal.
#[derive(Debug)]
struct Rejected(Option<String>);
impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Some(code) => write!(f, "fabric member rejected the tunnel session ({code})"),
            None => write!(f, "fabric member rejected the tunnel session"),
        }
    }
}
impl std::error::Error for Rejected {}

struct AbortOnDrop<T>(JoinHandle<T>);
impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::EndpointId;
    use std::process::{Child, Command, Stdio};

    struct Daemon(Child);
    impl Drop for Daemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires FABRIC_BIN and an explicit pinned member version"]
    async fn pinned_daemon_grants_flow_control_and_half_close() -> Result<()> {
        let binary = std::env::var("FABRIC_BIN").context("set FABRIC_BIN")?;
        let version = Command::new(&binary).arg("--version").output()?;
        // Inbound-only grants need fabric 0.2.41 or later.
        let expected_version =
            std::env::var("FABRIC_EXPECTED_VERSION").unwrap_or_else(|_| "0.2.41+84c3208".into());
        assert_eq!(String::from_utf8(version.stdout)?.trim(), expected_version);
        let home = tempfile::Builder::new()
            .prefix("fabric-proof-")
            .tempdir_in("/tmp")?;
        let control_home = tempfile::Builder::new()
            .prefix("fabric-proof-control-")
            .tempdir_in("/tmp")?;
        let fabric = |home: &std::path::Path, arguments: &[&str]| -> Result<String> {
            let output = Command::new(&binary)
                .arg("--home")
                .arg(home)
                .args(arguments)
                .output()?;
            if !output.status.success() {
                bail!(
                    "isolated fabric command failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Ok(String::from_utf8(output.stdout)?)
        };
        let command = |arguments: &[&str]| fabric(home.path(), arguments);
        let member: EndpointId = command(&["id"])?.trim().parse()?;
        let phone = SecretKey::generate();
        command(&[
            "add",
            &phone.public().to_string(),
            "demo-phone",
            "--inbound-only",
            "--allow",
            "demo-client/0",
        ])?;
        // An ordinarily granted peer that answers the member's echo probes, so the
        // member's probe loop is seen running while it leaves the phone alone.
        let control = fabric(control_home.path(), &["id"])?.trim().to_string();
        fabric(
            control_home.path(),
            &["add", &member.to_string(), "demo-member", "--allow", "echo"],
        )?;
        command(&["add", &control, "demo-control", "--allow", "demo-client/0"])?;
        let spawn = |home: &std::path::Path, probe_secs: &str| -> Result<Daemon> {
            Ok(Daemon(
                Command::new(&binary)
                    .arg("--home")
                    .arg(home)
                    .args(["up", "--foreground"])
                    .env("FABRIC_PEER_HEALTH_SECS", probe_secs)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()?,
            ))
        };
        let _control_daemon = spawn(control_home.path(), "0")?;
        let _daemon = spawn(home.path(), "1")?;
        let mut addr = None;
        for _ in 0..100 {
            if let Ok(value) = command(&["addr"]) {
                addr = Some(serde_json::from_str::<EndpointAddr>(&value)?);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let addr = addr.context("isolated fabric did not start")?;
        assert_eq!(addr.id, member);
        let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        command(&[
            "expose",
            "demo-client/0",
            "--tcp",
            &target.local_addr()?.to_string(),
            "--ephemeral",
        ])?;
        command(&[
            "expose",
            "demo-denied/0",
            "--tcp",
            &target.local_addr()?.to_string(),
            "--ephemeral",
        ])?;

        // Denied and unknown peers must never reach the exposed target. Each is
        // refused with code 403 and the reason's fabric code.
        for (key, service, code) in [
            (
                phone.clone(),
                "demo-denied/0",
                "peer_not_permitted_for_service",
            ),
            (
                SecretKey::generate(),
                "demo-client/0",
                "peer_not_in_allow_list",
            ),
        ] {
            let bridge = Bridge::start(key, addr.clone(), service.into()).await?;
            let mut local = TcpStream::connect(bridge.url.trim_start_matches("http://")).await?;
            local
                .write_all(b"GET /must-not-arrive HTTP/1.1\r\n\r\n")
                .await?;
            let mut response = Vec::new();
            let _ = tokio::time::timeout(Duration::from_secs(8), local.read_to_end(&mut response))
                .await?;
            assert!(response.is_empty());
            let attempt = settled_attempt().await;
            assert_eq!(attempt["result"], "refused", "{attempt}");
            assert_eq!(attempt["code"], code, "{attempt}");
            eprintln!(
                "{service} as {}: {attempt}",
                if code == "peer_not_in_allow_list" {
                    "an unknown node"
                } else {
                    "the phone"
                }
            );
            assert!(
                tokio::time::timeout(Duration::from_millis(100), target.accept())
                    .await
                    .is_err()
            );
            bridge.stop().await;
        }

        // A granted service the member cannot reach answers the Hello with an
        // Error frame: a fault with its code, not a refusal.
        let gone = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let gone_addr = gone.local_addr()?.to_string();
        drop(gone);
        command(&[
            "expose",
            "demo-client/0",
            "--tcp",
            &gone_addr,
            "--ephemeral",
        ])?;
        let bridge = Bridge::start(phone.clone(), addr.clone(), "demo-client/0".into()).await?;
        let mut local = TcpStream::connect(bridge.url.trim_start_matches("http://")).await?;
        local.write_all(b"GET /gone HTTP/1.1\r\n\r\n").await?;
        let mut response = Vec::new();
        let _ =
            tokio::time::timeout(Duration::from_secs(8), local.read_to_end(&mut response)).await?;
        assert!(response.is_empty());
        let attempt = settled_attempt().await;
        assert_eq!(attempt["result"], "rejected", "{attempt}");
        assert_eq!(attempt["code"], "service_unavailable", "{attempt}");
        eprintln!("unreachable service: {attempt}");
        bridge.stop().await;
        command(&[
            "expose",
            "demo-client/0",
            "--tcp",
            &target.local_addr()?.to_string(),
            "--ephemeral",
        ])?;

        // More than the upstream replay window: omitting Acks would stall this.
        let expected = vec![b'x'; 5 * 1024 * 1024];
        let body = expected.clone();
        let served = tokio::spawn(async move {
            let (mut stream, _) = target.accept().await?;
            let mut request = Vec::new();
            stream.read_to_end(&mut request).await?;
            assert_eq!(request, b"GET /demo HTTP/1.1\r\nConnection: close\r\n\r\n");
            stream.write_all(&body).await?;
            stream.shutdown().await?;
            Ok::<_, anyhow::Error>(())
        });
        let bridge = Bridge::start_with_mode(
            phone.clone(),
            addr.clone(),
            "demo-client/0".into(),
            PathMode::Direct,
        )
        .await?;
        let mut local = TcpStream::connect(bridge.url.trim_start_matches("http://")).await?;
        local
            .write_all(b"GET /demo HTTP/1.1\r\nConnection: close\r\n\r\n")
            .await?;
        local.shutdown().await?;
        let mut actual = Vec::new();
        tokio::time::timeout(Duration::from_secs(20), local.read_to_end(&mut actual)).await??;
        assert_eq!(actual, expected);
        served.await??;
        // Let the final Acks reach the member before stopping the whole endpoint.
        tokio::time::sleep(Duration::from_millis(100)).await;
        bridge.stop().await;

        // The member probed the ordinary peer and never the inbound-only phone.
        // A probe logs one of these events, naming the peer.
        let probe_events = [
            "event=\"peer_health_probe\"",
            "event=\"peer_health_probe_skipped\"",
            "event=\"peer_offline\"",
            "event=\"peer_online\"",
        ];
        let mut probes = Vec::new();
        for _ in 0..60 {
            probes.clear();
            for entry in std::fs::read_dir(home.path().join("logs"))? {
                let log = std::fs::read_to_string(entry?.path())?;
                probes.extend(
                    log.lines()
                        .filter(|line| probe_events.iter().any(|event| line.contains(event)))
                        .map(str::to_owned),
                );
            }
            let control_probes = probes
                .iter()
                .filter(|line| line.contains("peer=demo-control"))
                .count();
            if control_probes >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let control_probes = probes
            .iter()
            .filter(|line| line.contains("peer=demo-control"))
            .count();
        assert!(
            control_probes >= 3,
            "the member probed its ordinary peer {control_probes} times: {probes:?}"
        );
        assert!(
            !probes.iter().any(|line| line.contains("demo-phone")),
            "the member probed the inbound-only phone: {probes:?}"
        );
        let status: serde_json::Value = serde_json::from_str(&command(&["status", "--json"])?)?;
        let phone_status = status["peers"]
            .as_array()
            .context("status has no peers")?
            .iter()
            .find(|peer| peer["name"] == "demo-phone")
            .context("status has no demo-phone")?;
        assert_eq!(phone_status["inbound_only"], true);
        assert!(phone_status["round_trip_micros"].is_null());
        assert!(phone_status["error"].is_null());
        eprintln!(
            "member probe events: {control_probes} of demo-control, 0 of demo-phone; demo-phone status: {phone_status}"
        );

        // Relay-only mode must use a measured relay path even when local IP hints exist.
        let mut relay_addr = None;
        for _ in 0..40 {
            let candidate: EndpointAddr = serde_json::from_str(&command(&["addr"])?)?;
            if candidate.addrs.iter().any(|a| a.is_relay()) {
                relay_addr = Some(candidate);
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let relay_addr = relay_addr.context("isolated fabric did not advertise a relay")?;
        let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        command(&[
            "expose",
            "demo-client/0",
            "--tcp",
            &target.local_addr()?.to_string(),
            "--ephemeral",
        ])?;
        let served = tokio::spawn(async move {
            let (mut stream, _) = target.accept().await?;
            let mut request = Vec::new();
            stream.read_to_end(&mut request).await?;
            assert_eq!(request, b"relay probe");
            stream.write_all(b"relay reply").await?;
            stream.shutdown().await?;
            Ok::<_, anyhow::Error>(())
        });
        let bridge = Bridge::start_with_mode(
            phone.clone(),
            relay_addr,
            "demo-client/0".into(),
            PathMode::Relay,
        )
        .await?;
        let mut local = TcpStream::connect(bridge.url.trim_start_matches("http://")).await?;
        local.write_all(b"relay probe").await?;
        local.shutdown().await?;
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(30), local.read_to_end(&mut response)).await??;
        assert_eq!(response, b"relay reply");
        served.await??;
        assert_eq!(
            diagnostics::snapshot()["attempts"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["selectedPath"],
            "relay"
        );
        bridge.stop().await;

        // Explicit stop half-closes an idle local request so the member can reap it.
        let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        command(&[
            "expose",
            "demo-client/0",
            "--tcp",
            &target.local_addr()?.to_string(),
            "--ephemeral",
        ])?;
        let (accepted, ready) = tokio::sync::oneshot::channel();
        let served = tokio::spawn(async move {
            let (mut stream, _) = target.accept().await?;
            let mut prefix = [0u8; 12];
            stream.read_exact(&mut prefix).await?;
            assert_eq!(&prefix, b"idle request");
            let _ = accepted.send(());
            let mut request = Vec::new();
            stream.read_to_end(&mut request).await?;
            assert!(request.is_empty());
            stream.shutdown().await?;
            Ok::<_, anyhow::Error>(())
        });
        let bridge = Bridge::start(phone.clone(), addr.clone(), "demo-client/0".into()).await?;
        let mut local = TcpStream::connect(bridge.url.trim_start_matches("http://")).await?;
        local.write_all(b"idle request").await?;
        tokio::time::timeout(Duration::from_secs(8), ready).await??;
        tokio::time::timeout(Duration::from_secs(8), bridge.stop()).await?;
        served.await??;

        // Removing the grant ends an attached session: the member closes it with
        // code 403, the refusal's code and `(access revoked)`.
        let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        command(&[
            "expose",
            "demo-client/0",
            "--tcp",
            &target.local_addr()?.to_string(),
            "--ephemeral",
        ])?;
        let (accepted, ready) = tokio::sync::oneshot::channel();
        let served = tokio::spawn(async move {
            let (mut stream, _) = target.accept().await?;
            let mut prefix = [0u8; 15];
            stream.read_exact(&mut prefix).await?;
            let _ = accepted.send(());
            let mut rest = Vec::new();
            let _ = stream.read_to_end(&mut rest).await;
            Ok::<_, anyhow::Error>(())
        });
        let bridge = Bridge::start(phone.clone(), addr, "demo-client/0".into()).await?;
        let mut local = TcpStream::connect(bridge.url.trim_start_matches("http://")).await?;
        local.write_all(b"held by a grant").await?;
        tokio::time::timeout(Duration::from_secs(8), ready).await??;
        command(&[
            "add",
            &phone.public().to_string(),
            "demo-phone",
            "--inbound-only",
            "--allow",
            "demo-denied/0",
        ])?;
        command(&["reload-peers"])?;
        let mut response = Vec::new();
        let _ =
            tokio::time::timeout(Duration::from_secs(8), local.read_to_end(&mut response)).await?;
        let attempt = settled_attempt().await;
        assert_eq!(attempt["result"], "refused", "{attempt}");
        assert_eq!(
            attempt["code"], "peer_not_permitted_for_service",
            "{attempt}"
        );
        eprintln!("grant removed while attached: {attempt}");
        bridge.stop().await;
        let _ = tokio::time::timeout(Duration::from_secs(8), served).await;
        Ok(())
    }

    /// Against a real isolated st member started by `proof-member.mjs`: the
    /// paired-only gateway through an inbound-only grant, st's own refusal of an
    /// unpaired request, and fabric's typed refusals, Error frame and revocation
    /// as they arrive on the wire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires ST_FABRIC_PROOF_MEMBER, a proof-member.mjs descriptor"]
    async fn real_member_typed_errors() -> Result<()> {
        let descriptor =
            std::env::var("ST_FABRIC_PROOF_MEMBER").context("set ST_FABRIC_PROOF_MEMBER")?;
        let member: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(descriptor)?)?;
        let text = |key: &str| {
            member[key]
                .as_str()
                .map(str::to_owned)
                .context("descriptor field")
        };
        let (binary, home, gateway, service) = (
            text("fabric")?,
            text("home")?,
            text("gateway")?,
            text("service")?,
        );
        assert!(
            home.starts_with("/tmp/st-fabric-proof-"),
            "not an isolated proof member"
        );
        let fabric = |arguments: &[&str]| -> Result<String> {
            let output = Command::new(&binary)
                .arg("--home")
                .arg(&home)
                .args(arguments)
                .output()?;
            if !output.status.success() {
                bail!(
                    "isolated fabric command failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Ok(String::from_utf8(output.stdout)?)
        };
        eprintln!("member fabric {}", fabric(&["--version"])?.trim());
        let addr: EndpointAddr = serde_json::from_value(member["addr"].clone())?;
        let phone = SecretKey::generate();
        let grant = |allow: &str| {
            fabric(&[
                "add",
                &phone.public().to_string(),
                "demo-phone",
                "--inbound-only",
                "--allow",
                allow,
            ])
            .and_then(|_| fabric(&["reload-peers"]))
        };
        fabric(&[
            "expose",
            "demo-denied/0",
            "--socket",
            &gateway,
            "--ephemeral",
        ])?;
        fabric(&[
            "expose",
            "demo-gone/0",
            "--socket",
            "/tmp/st-fabric-proof-missing.sock",
            "--ephemeral",
        ])?;
        grant(&format!("{service},demo-gone/0"))?;

        // The st gateway through the bridge: capabilities, then st refusing an
        // unpaired request for a protected route.
        let bridge = Bridge::start(phone.clone(), addr.clone(), service.clone()).await?;
        for path in ["/v1/client/capabilities", "/v1/client/agents"] {
            let mut local = TcpStream::connect(bridge.url.trim_start_matches("http://")).await?;
            local
                .write_all(
                    format!("GET {path} HTTP/1.1\r\nHost: member\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .await?;
            let mut response = Vec::new();
            tokio::time::timeout(Duration::from_secs(20), local.read_to_end(&mut response))
                .await??;
            let response = String::from_utf8_lossy(&response);
            let status = response.lines().next().unwrap_or_default().to_owned();
            let body = response.split("\r\n\r\n").nth(1).unwrap_or_default();
            eprintln!(
                "GET {path} through the bridge: {status} {}",
                &body[..body.len().min(160)]
            );
            if path.ends_with("capabilities") {
                assert!(
                    status.contains(" 200 ") && body.contains("st3.client.v0"),
                    "{response}"
                );
            } else {
                assert_eq!(status.split_whitespace().nth(1), Some("403"), "{response}");
                let denied: serde_json::Value = serde_json::from_str(body)?;
                assert_eq!(denied["api_version"], "st3.client.v0");
                assert_eq!(denied["error_version"], "st3.client.error.v0");
                assert_eq!(denied["code"], "forbidden");
                assert_eq!(
                    denied["message"],
                    "the Fabric-loopback client credential is required"
                );
                assert_eq!(denied["retryable"], false);
            }
        }
        bridge.stop().await;

        // Raw connections, to read the member's close reasons and Error text.
        let raw = |key: SecretKey, alpn: String| {
            let addr = addr.clone();
            async move {
                let endpoint = Endpoint::builder(presets::N0)
                    .secret_key(key)
                    .bind()
                    .await?;
                let connection = endpoint.connect(addr, alpn.as_bytes()).await;
                Ok::<_, anyhow::Error>((endpoint, connection))
            }
        };
        let close_of = |error: &ConnectionError| match error {
            ConnectionError::ApplicationClosed(close) => (
                u64::from(close.error_code),
                String::from_utf8_lossy(&close.reason).into_owned(),
            ),
            other => (0, other.to_string()),
        };
        let hello = || Frame::Hello {
            id: [7; 16],
            next: 0,
            resume: false,
        };
        for (label, key, alpn, code) in [
            (
                "unknown node",
                SecretKey::generate(),
                service.clone(),
                "peer_not_in_allow_list",
            ),
            (
                "ungranted service",
                phone.clone(),
                "demo-denied/0".to_owned(),
                "peer_not_permitted_for_service",
            ),
        ] {
            let (endpoint, connection) = raw(key, alpn).await?;
            let error = match connection {
                Err(error) => anyhow::Error::from(error)
                    .chain()
                    .find_map(|e| e.downcast_ref::<ConnectionError>())
                    .map(close_of)
                    .context("connect failed without a close")?,
                Ok(connection) => close_of(
                    &tokio::time::timeout(Duration::from_secs(8), connection.closed()).await?,
                ),
            };
            eprintln!("{label}: close code {} reason {:?}", error.0, error.1);
            assert_eq!(error.0, 403);
            assert_eq!(wire::error_code(error.1.as_bytes()).as_deref(), Some(code));
            endpoint.close().await;
        }

        // A granted service whose socket is gone answers the Hello with an Error.
        let (endpoint, connection) = raw(phone.clone(), "demo-gone/0".into()).await?;
        let connection = connection?;
        let (mut send, mut recv) = connection.open_bi().await?;
        wire::write(&mut send, hello()).await?;
        let Frame::Error(text) =
            tokio::time::timeout(Duration::from_secs(8), wire::read(&mut recv)).await??
        else {
            bail!("expected an Error frame");
        };
        let code = wire::error_code(text.as_bytes());
        eprintln!(
            "unreachable service: Error frame code {code:?} ({} bytes of diagnostic)",
            text.len()
        );
        assert_eq!(code.as_deref(), Some("service_unavailable"));
        endpoint.close().await;

        // Removing the grant ends an attached session with 403, the code and
        // `(access revoked)`.
        let (endpoint, connection) = raw(phone.clone(), service.clone()).await?;
        let connection = connection?;
        let (mut send, mut recv) = connection.open_bi().await?;
        wire::write(&mut send, hello()).await?;
        let answer = tokio::time::timeout(Duration::from_secs(8), wire::read(&mut recv)).await??;
        assert!(
            matches!(answer, Frame::Hello { id, .. } if id == [7; 16]),
            "{answer:?}"
        );
        grant("demo-gone/0")?;
        let (code, reason) =
            close_of(&tokio::time::timeout(Duration::from_secs(8), connection.closed()).await?);
        eprintln!("grant removed while attached: close code {code} reason {reason:?}");
        assert_eq!(code, 403);
        assert_eq!(
            wire::error_code(reason.as_bytes()).as_deref(),
            Some("peer_not_permitted_for_service")
        );
        assert!(reason.ends_with("(access revoked)"), "{reason}");
        endpoint.close().await;
        fabric(&["remove", "demo-phone"])?;
        fabric(&["reload-peers"])?;
        Ok(())
    }

    /// The latest attempt, once it has a final result. The local socket closes
    /// as a tunnel fails, just before its result is recorded.
    async fn settled_attempt() -> serde_json::Value {
        let mut attempt = serde_json::Value::Null;
        for _ in 0..100 {
            attempt = diagnostics::snapshot()["attempts"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()
                .clone();
            if attempt["result"] != "connected" && attempt["result"] != "connecting" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        attempt
    }
}
