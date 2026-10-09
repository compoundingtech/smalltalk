//! Attach this terminal to a terminal that another fleet host owns, PTY to PTY over Fabric.
//!
//! `st terminals expose-fabric` has the owning host's Fabric daemon run
//! `st terminals serve-fabric --stdio` for each tunnel under the fleet's PTY protocol, so no st
//! daemon takes part on either side. The dialing CLI sends one route line that names the PTY
//! session, the subject, and the runtime incarnation st selected. The owner proves that
//! incarnation against the PTY itself, as a local attach does, answers `{"ok":true}`, and then
//! splices the tunnel to the session socket: the attach client speaks the PTY session protocol to
//! the session. The route line is the `pty remote-serve` control line with two more fields.

use std::io::{BufRead as _, Read as _, Write as _};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use pty_client::{ClientIo, Reconnect, RouteRefusedError};

pub use st3_terminal_direct::{
    FabricTarget, ROUTE_LINE_LIMIT, ROUTE_TIMEOUT, RouteAnswer, RouteError, RouteRequest,
    open_route, owner_peer, protocol,
};

use crate::model::LocalTerminal;

/// Attach this terminal to `request`'s PTY session over Fabric until the person detaches or the
/// session ends. A lost tunnel is dialed again with the same incarnation until the owner refuses.
/// `Err(RouteError)` inside means no route opened at all, so nothing was attached.
pub async fn attach(
    target: &FabricTarget,
    request: &RouteRequest,
) -> Result<Result<i32, RouteError>> {
    attach_with_io(target, request, ClientIo::default()).await
}

async fn attach_with_io(
    target: &FabricTarget,
    request: &RouteRequest,
    io: ClientIo,
) -> Result<Result<i32, RouteError>> {
    let stream = match open_route(target, request).await {
        Ok(stream) => stream,
        Err(error) => return Ok(Err(error)),
    };
    let handle = tokio::runtime::Handle::current();
    let (target, again) = (target.clone(), request.clone());
    let reconnect: Reconnect =
        Box::new(move || match handle.block_on(open_route(&target, &again)) {
            Ok(stream) => Ok(Some(stream)),
            Err(RouteError::Refused(reason)) => Err(RouteRefusedError(reason)),
            Err(RouteError::Unreachable(_)) => Ok(None),
        });
    let code =
        crate::client::proxy_stream_with_io(&request.name, stream, Some(reconnect), io).await?;
    Ok(Ok(code))
}

/// Serve one Fabric tunnel on stdin and stdout: read a route line, prove that the named PTY
/// session under `pty_root` is the subject's terminal at the requested incarnation, answer, and
/// then splice the tunnel to the session until either side closes. It never starts or restarts
/// a session.
pub async fn serve_stdio(pty_root: &Path) -> Result<()> {
    // One thread reads stdin from the route line to the end, so no byte of the session protocol
    // waits in a buffer that the splice cannot see.
    let (lines, line) = std::sync::mpsc::channel();
    let (splice, session) = std::sync::mpsc::channel::<StdUnixStream>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut request = Vec::new();
        let read = (&mut stdin)
            .take(ROUTE_LINE_LIMIT as u64)
            .read_until(b'\n', &mut request);
        if lines.send(read.map(|_| request)).is_err() {
            return;
        }
        if let Ok(mut session) = session.recv() {
            let _ = std::io::copy(&mut stdin, &mut session);
            let _ = session.shutdown(std::net::Shutdown::Write);
        }
    });
    // Unbuffered, so every screen reaches the tunnel when the session writes it.
    let mut stdout = std::fs::File::from(std::io::stdout().as_fd().try_clone_to_owned()?);
    let request = tokio::task::spawn_blocking(move || line.recv_timeout(ROUTE_TIMEOUT))
        .await?
        .context("no route line arrived")?
        .context("read the route line")?;
    let stream = match route(pty_root, &request).await {
        Ok(stream) => stream,
        Err(error) => {
            let answer = RouteAnswer {
                error: Some(format!("{error:#}")),
                ..RouteAnswer::default()
            };
            writeln!(stdout, "{}", serde_json::to_string(&answer)?)?;
            return Ok(());
        }
    };
    let answer = RouteAnswer {
        ok: Some(true),
        ..RouteAnswer::default()
    };
    writeln!(stdout, "{}", serde_json::to_string(&answer)?)?;
    splice
        .send(stream.try_clone()?)
        .context("the tunnel closed before the splice")?;
    let mut from_session = stream;
    // A closed tunnel is how the attach client leaves, so neither direction's end is an error.
    tokio::task::spawn_blocking(move || {
        let _ = std::io::copy(&mut from_session, &mut stdout);
    })
    .await?;
    Ok(())
}

/// The PTY session a route line names, connected and proven: it is tagged as the subject's
/// terminal, and the kernel and the registry prove the incarnation as a local attach does.
async fn route(pty_root: &Path, line: &[u8]) -> Result<StdUnixStream> {
    let request: RouteRequest = serde_json::from_slice(line).context("parse the route line")?;
    anyhow::ensure!(
        request.op == "route",
        "unsupported operation `{}`",
        request.op
    );
    anyhow::ensure!(
        is_session_name(&request.name),
        "`{}` is not a PTY session name",
        request.name
    );
    let metadata = pty_core::registry::read_metadata_in(pty_root, &request.name)
        .with_context(|| format!("no PTY session `{}` runs here", request.name))?;
    let tagged = metadata
        .tags
        .as_ref()
        .and_then(|tags| tags.get("st3.subject"))
        .is_some_and(|subject| *subject == request.subject);
    anyhow::ensure!(
        tagged,
        "PTY session `{}` is not `{}`'s terminal",
        request.name,
        request.subject
    );
    crate::client::open_local_terminal(&LocalTerminal {
        subject: request.subject,
        runtime_id: request.name,
        incarnation_id: request.incarnation,
        pty_root: PathBuf::from(pty_root),
    })
    .await
}

/// A PTY session name is one plain file name under the PTY root.
fn is_session_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.contains(['/', '\0'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_route_names_one_file_under_the_pty_root() {
        assert!(is_session_name("fleet.example.worker.0123abcd"));
        for name in ["", ".", "..", "../pty/other", "a/b", "nul\0byte"] {
            assert!(!is_session_name(name), "{name:?}");
        }
        assert!(!is_session_name(&"a".repeat(256)));
    }

    #[test]
    fn a_route_line_is_the_pty_remote_line_with_a_subject_and_an_incarnation() {
        let line = serde_json::to_value(RouteRequest::new(
            "example-worker",
            "agent/example/worker",
            "42:2026-09-29T08:00:00.000Z",
        ))
        .unwrap();
        assert_eq!(
            line,
            serde_json::json!({
                "op": "route",
                "name": "example-worker",
                "subject": "agent/example/worker",
                "incarnation": "42:2026-09-29T08:00:00.000Z",
            })
        );
    }

    #[tokio::test]
    async fn a_route_outside_the_pty_root_or_to_another_subject_is_refused() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("example-other.json"),
            serde_json::json!({
                "createdAt": "2026-09-29T08:00:00.000Z",
                "tags": { "st3.subject": "agent/example/other" },
            })
            .to_string(),
        )
        .unwrap();
        let refused = |name: &str| {
            serde_json::to_vec(&RouteRequest::new(
                name,
                "agent/example/worker",
                "42:2026-09-29T08:00:00.000Z",
            ))
            .unwrap()
        };
        let outside = route(root.path(), &refused("../example-other")).await;
        assert!(
            format!("{:#}", outside.unwrap_err()).contains("is not a PTY session name"),
            "a name must not leave the PTY root"
        );
        let other = route(root.path(), &refused("example-other")).await;
        assert!(
            format!("{:#}", other.unwrap_err()).contains("is not `agent/example/worker`'s"),
            "a session tagged for another subject is refused"
        );
        let missing = route(root.path(), &refused("example-missing")).await;
        assert!(format!("{:#}", missing.unwrap_err()).contains("no PTY session"));
        let list = route(root.path(), br#"{"op":"list"}"#).await;
        assert!(list.is_err());
    }
}
