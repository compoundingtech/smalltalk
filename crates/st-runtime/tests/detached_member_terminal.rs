#![cfg(unix)]
//! A member launched the way the daemon launches a seat — a detached `pty` session under a
//! temporary PTY root — runs, renders, and can be observed while no client ever attaches.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use st_runtime::{Launch, PtyRuntime, materialize_environment, resolve_executable};

fn keep_launches_out_of_the_user_manager() {
    // Without a runtime directory the shared isolation mode is a detached session, so these
    // launches never create transient units in the account's service manager.
    unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

struct Member {
    runtime: PtyRuntime,
    id: String,
}

impl Drop for Member {
    fn drop(&mut self) {
        let _ = self.runtime.stop(&self.id);
        for _ in 0..40 {
            if self.runtime.remove(&self.id).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn wait_for(what: &str, mut ready: impl FnMut() -> Option<String>) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(value) = ready() {
            return value;
        }
        assert!(Instant::now() < deadline, "{what} never appeared");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A full-screen harness needs a controlling terminal with a real size to draw its interface and
/// accept input. With no client attached, the member still gets one, and what it draws is readable
/// from its screen.
#[test]
fn a_member_no_client_attached_to_has_a_sized_terminal_and_a_readable_screen() {
    keep_launches_out_of_the_user_manager();
    let root = tempfile::tempdir().unwrap();
    let executable = std::env::current_exe().unwrap();
    let environment = materialize_environment(&BTreeMap::new(), &executable).unwrap();
    let pty = resolve_executable("pty", &environment)
        .expect("the member launch path needs `pty` on the login-shell PATH");
    let size_out = root.path().join("size");
    let script = format!(
        "{{ [ -t 0 ] && echo tty; stty size; }} > {out}.tmp && mv {out}.tmp {out}; \
         printf 'seat-ready-marker\\n'; exec sleep 60",
        out = shell_quote(&size_out),
    );
    let runtime = PtyRuntime::new(root.path().join("pty")).with_binary(pty.to_string_lossy());
    let id = "headless-probe".to_string();
    runtime
        .spawn(
            &id,
            &Launch::Shell(script),
            root.path(),
            &environment,
            None,
            &BTreeMap::new(),
        )
        .unwrap();
    let member = Member { runtime, id };

    let size = wait_for("the member's terminal report", || {
        std::fs::read_to_string(&size_out).ok()
    });
    let mut lines = size.lines();
    assert_eq!(
        lines.next(),
        Some("tty"),
        "stdin is not a terminal: {size:?}"
    );
    let dimensions = lines
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .map(|value| value.parse::<u16>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(dimensions.len(), 2, "{size:?}");
    assert!(
        dimensions.iter().all(|value| *value > 0),
        "a detached member's terminal has no size: {size:?}"
    );

    let screen = wait_for("the member's rendered output", || {
        member
            .runtime
            .screen(&member.id)
            .ok()
            .filter(|screen| screen.contains("seat-ready-marker"))
    });
    assert!(screen.contains("seat-ready-marker"));
    let observed = member
        .runtime
        .snapshot()
        .unwrap()
        .into_iter()
        .find(|observation| observation.name == member.id)
        .expect("the detached member is listed");
    assert_eq!(observed.status, "running");
}
