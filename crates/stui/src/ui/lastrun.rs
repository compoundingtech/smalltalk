//! Whether the stui before this one ended cleanly.
//!
//! A stui that is killed from outside (SIGKILL) or aborts cannot restore the terminal, so its
//! window keeps its last picture and looks frozen (Nathan, 2026-10-05). Each running stui keeps
//! a small file under `$XDG_CACHE_HOME/st3/stui/runs/` (the cache, not the state: a device
//! with no st state must stay free of it), removed when it exits by any route that
//! runs destructors: a quit, a signal it handles, an error or a panic. A file whose process is
//! gone is the trace of one that did not, and the next stui to start says so.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize)]
struct Mark {
    pid: u32,
    started_unix: u64,
    build: String,
}

/// This stui's file, removed on drop.
pub struct Run {
    file: Option<PathBuf>,
}

impl Drop for Run {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = std::fs::remove_file(file);
        }
    }
}

fn directory() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    base.is_absolute()
        .then(|| base.join("st3").join("stui").join("runs"))
}

/// Keep a trace of what a panic said and where, in `panics.log` beside the run files, so a
/// crash that the terminal's own restore hides can be read afterwards (Nathan, 2026-10-07: "it
/// keeps crashing"). The default hook still runs, so the terminal is restored as before.
pub fn log_panics(build: &str) {
    let build = build.to_owned();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(file) = directory().and_then(|runs| runs.parent().map(|dir| dir.join("panics.log")))
        {
            write_panic(
                &file,
                &build,
                &format!("{info}\n{}", std::backtrace::Backtrace::force_capture()),
            );
        }
        previous(info);
    }));
}

/// Append one panic to the log, which is cleared once it passes 200 KB.
fn write_panic(file: &Path, build: &str, what: &str) {
    use std::io::Write as _;
    let _ = std::fs::create_dir_all(file.parent().unwrap_or(Path::new(".")));
    if std::fs::metadata(file).is_ok_and(|meta| meta.len() > 200_000) {
        let _ = std::fs::remove_file(file);
    }
    if let Ok(mut out) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
        let _ = writeln!(out, "{} {build} {what}\n", now_unix());
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Whether a process with this ID exists.
fn alive(pid: u32) -> bool {
    // Signal 0 only checks: it succeeds, or fails with EPERM for a process of another user.
    // SAFETY: kill with signal 0 sends nothing.
    let found = unsafe { libc::kill(pid as libc::pid_t, 0) };
    found == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Record that this stui is running, and say whether an earlier one left without exiting
/// cleanly: its file is there and its process is gone. The newest such trace is returned
/// (and every one is cleared), as "started 3h ago".
pub fn begin(build: &str) -> (Run, Option<String>) {
    let Some(directory) = directory() else {
        return (Run { file: None }, None);
    };
    let _ = std::fs::create_dir_all(&directory);
    let note = left_without_exiting(&directory, std::process::id(), now_unix());
    let file = directory.join(format!("{}.json", std::process::id()));
    let mark = Mark {
        pid: std::process::id(),
        started_unix: now_unix(),
        build: build.to_owned(),
    };
    let written = serde_json::to_vec(&mark)
        .ok()
        .is_some_and(|bytes| std::fs::write(&file, bytes).is_ok());
    (
        Run {
            file: written.then_some(file),
        },
        note,
    )
}

fn left_without_exiting(directory: &Path, own: u32, now: u64) -> Option<String> {
    let mut newest: Option<Mark> = None;
    for entry in std::fs::read_dir(directory).ok()?.flatten() {
        let path = entry.path();
        let Some(mark) = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Mark>(&bytes).ok())
        else {
            continue;
        };
        if mark.pid == own || alive(mark.pid) {
            continue;
        }
        let _ = std::fs::remove_file(&path);
        if newest
            .as_ref()
            .is_none_or(|known| mark.started_unix > known.started_unix)
        {
            newest = Some(mark);
        }
    }
    newest.map(|mark| {
        format!(
            "An earlier stui (pid {}, started {} ago, {}) ended without exiting cleanly: killed or crashed",
            mark.pid,
            elapsed(now.saturating_sub(mark.started_unix)),
            mark.build
        )
    })
}

fn elapsed(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(directory: &Path, pid: u32, started: u64) {
        let mark = Mark {
            pid,
            started_unix: started,
            build: "0.1.0+test".into(),
        };
        std::fs::write(
            directory.join(format!("{pid}.json")),
            serde_json::to_vec(&mark).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn a_trace_whose_process_is_gone_is_reported_once_and_a_live_one_is_not() {
        let directory = tempfile::tempdir().unwrap();
        // A child that has exited: its ID is free (and not ours).
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let gone = child.id();
        child.wait().unwrap();
        mark(directory.path(), gone, 1_000);
        // This test's own process is alive; so is another that is still running.
        let mut sleeper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        mark(directory.path(), sleeper.id(), 2_000);
        let note = left_without_exiting(directory.path(), 1, 1_000 + 3 * 3_600).unwrap();
        assert!(note.contains(&format!("pid {gone}")), "{note}");
        assert!(note.contains("started 3h ago"), "{note}");
        assert!(note.contains("without exiting cleanly"), "{note}");
        // It was cleared, so the next start does not say it again; the live one stayed.
        assert!(left_without_exiting(directory.path(), 1, 5_000).is_none());
        assert!(
            directory
                .path()
                .join(format!("{}.json", sleeper.id()))
                .exists()
        );
        // Once that one is gone too, its trace is the next to be reported.
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        let later = left_without_exiting(directory.path(), 1, 5_000).unwrap();
        assert!(later.contains(&format!("pid {}", sleeper.id())), "{later}");
    }

    #[test]
    fn dropping_the_run_removes_its_file() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("1.json");
        std::fs::write(&file, b"{}").unwrap();
        drop(Run {
            file: Some(file.clone()),
        });
        assert!(!file.exists());
    }

    #[test]
    fn a_panic_is_written_to_the_log_and_the_log_stays_small() {
        let root = std::env::temp_dir().join(format!("stui-panic-{}", std::process::id()));
        let file = root.join("stui").join("panics.log");
        write_panic(&file, "build-1", "panicked at src/x.rs:1:1: boom");
        let first = std::fs::read_to_string(&file).unwrap();
        assert!(first.contains("build-1") && first.contains("boom"), "{first}");
        write_panic(&file, "build-1", "second");
        assert!(std::fs::read_to_string(&file).unwrap().contains("second"));
        std::fs::write(&file, "x".repeat(200_001)).unwrap();
        write_panic(&file, "build-1", "after the cut");
        let cut = std::fs::read_to_string(&file).unwrap();
        assert!(cut.len() < 1_000 && cut.contains("after the cut"));
        let _ = std::fs::remove_dir_all(root);
    }
}
