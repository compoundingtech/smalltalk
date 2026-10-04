use crate::wire::{self, CHUNK, Frame, WINDOW};
use anyhow::{Context, Result, bail};
use iroh::{
    Endpoint, EndpointAddr, SecretKey,
    endpoint::{Connection, presets},
};
use std::{net::Ipv4Addr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, mpsc},
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

pub struct Bridge {
    pub endpoint: Endpoint,
    pub url: String,
    pub node: String,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl Bridge {
    pub async fn start(key: SecretKey, target: EndpointAddr, service: String) -> Result<Self> {
        validate_service(&service)?;
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(key)
            .bind()
            .await?;
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
                        sessions.spawn(async move {
                            let _permit = permit;
                            let _ = tunnel(endpoint, target, service, local, stopped).await;
                        });
                    }
                }
            }
            // Every tunnel owns its cleanup; cancel and join before closing the endpoint.
            while sessions.join_next().await.is_some() {}
            owned_endpoint.close().await;
        });
        Ok(Self {
            endpoint,
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
) -> Result<()> {
    let connection = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        connected = tokio::time::timeout(Duration::from_secs(5), endpoint.connect(target, service.as_bytes())) => connected.context("fabric connect timed out")??,
    };
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
        // Keep remote host-local paths out of app errors and logs.
        Frame::Error(_) => bail!("fabric member rejected the tunnel session"),
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
    #[ignore = "requires FABRIC_BIN pointing at fabric 0.2.30"]
    async fn pinned_daemon_grants_flow_control_and_half_close() -> Result<()> {
        let binary = std::env::var("FABRIC_BIN").context("set FABRIC_BIN")?;
        let version = Command::new(&binary).arg("--version").output()?;
        assert_eq!(String::from_utf8(version.stdout)?.trim(), "0.2.30+8bd9017");
        let home = tempfile::Builder::new()
            .prefix("fabric-proof-")
            .tempdir_in("/tmp")?;
        let command = |arguments: &[&str]| -> Result<String> {
            let output = Command::new(&binary)
                .arg("--home")
                .arg(home.path())
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
        let member: EndpointId = command(&["id"])?.trim().parse()?;
        let phone = SecretKey::generate();
        command(&[
            "add",
            &phone.public().to_string(),
            "demo-phone",
            "--allow",
            "demo-client/0",
        ])?;
        let _daemon = Daemon(
            Command::new(&binary)
                .arg("--home")
                .arg(home.path())
                .args(["up", "--foreground"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
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

        // Denied and unknown peers must never reach the exposed target.
        for (key, service) in [
            (phone.clone(), "demo-denied/0"),
            (SecretKey::generate(), "demo-client/0"),
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
            assert!(
                tokio::time::timeout(Duration::from_millis(100), target.accept())
                    .await
                    .is_err()
            );
            bridge.stop().await;
        }

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
        let bridge = Bridge::start(phone.clone(), addr.clone(), "demo-client/0".into()).await?;
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
        let bridge = Bridge::start(phone, addr, "demo-client/0".into()).await?;
        let mut local = TcpStream::connect(bridge.url.trim_start_matches("http://")).await?;
        local.write_all(b"idle request").await?;
        tokio::time::timeout(Duration::from_secs(8), ready).await??;
        tokio::time::timeout(Duration::from_secs(8), bridge.stop()).await?;
        served.await??;
        Ok(())
    }
}
