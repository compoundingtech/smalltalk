use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// How long a PTY session may take to accept a connection.
const LOCAL_TERMINAL_CONNECT: Duration = Duration::from_secs(3);

/// A running terminal that a daemon owns on its own host: the PTY session a local attach
/// connects to directly, with no WebSocket bridge through the daemon and no graph write.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LocalTerminal {
    pub subject: String,
    pub runtime_id: String,
    /// The graph's incarnation, `DAEMON_PID:CREATED_AT`, which the PTY itself must prove.
    pub incarnation_id: String,
    /// The daemon's PTY root as an absolute path.
    pub pty_root: PathBuf,
}

/// Connect to the terminal's PTY socket and prove it serves the incarnation the graph selected
/// before sending anything: the kernel names the process serving the socket, which must be the
/// incarnation's PTY daemon, and the registry must record the incarnation's start time. A socket
/// path proves nothing alone, since a replacement session binds the same path.
pub async fn open_local_terminal(terminal: &LocalTerminal) -> Result<StdUnixStream> {
    let (stream, peer) =
        connect_pty_session(&terminal.pty_root, &terminal.runtime_id, &terminal.subject).await?;
    let created_at = pty_core::registry::read_metadata_in(&terminal.pty_root, &terminal.runtime_id)
        .map(|metadata| metadata.created_at)
        .with_context(|| {
            format!(
                "terminal `{}` has no PTY record in {}",
                terminal.subject,
                terminal.pty_root.display()
            )
        })?;
    let incarnation = format!("{peer}:{created_at}");
    anyhow::ensure!(
        incarnation == terminal.incarnation_id,
        "terminal `{}` changed incarnation: st selected `{}`, but its PTY is `{incarnation}`",
        terminal.subject,
        terminal.incarnation_id
    );
    Ok(stream)
}

/// Connect to PTY session `runtime_id` under `pty_root`, returning the stream and the pid the
/// kernel reports for the process serving it. Nothing is sent.
pub async fn connect_pty_session(
    pty_root: &Path,
    runtime_id: &str,
    subject: &str,
) -> Result<(StdUnixStream, i32)> {
    let socket = pty_root.join(format!("{runtime_id}.sock"));
    let stream = tokio::time::timeout(
        LOCAL_TERMINAL_CONNECT,
        tokio::net::UnixStream::connect(&socket),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "terminal `{subject}` did not accept a connection at {} within {} ms",
            socket.display(),
            LOCAL_TERMINAL_CONNECT.as_millis()
        )
    })?
    .with_context(|| format!("connect to terminal `{subject}` at {}", socket.display()))?
    .into_std()?;
    stream.set_nonblocking(false)?;
    let peer = pty_core::unix_peer::credentials(&stream).with_context(|| {
        format!(
            "identify the process serving terminal `{subject}` at {}",
            socket.display()
        )
    })?;
    Ok((stream, peer.pid))
}
