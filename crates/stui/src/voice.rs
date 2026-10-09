//! Voice mode: the speech helper (`st-listen`, inside SmallTalk.app on a Mac) run as a child,
//! its JSON lines read into events. The helper owns the microphone and the transcription; stui
//! only shows what it hears and decides what happens to the words.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;

/// What the helper says, one line at a time.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Listening, to this input.
    Ready { device: String },
    /// Loudness, 0 to 1, about twenty a second.
    Level(f32),
    /// The words so far; `final` once a stretch of speech has settled.
    Text { text: String, settled: bool },
    /// Only silence from this input for a few seconds.
    Silent { device: String },
    /// Everything heard, after `finish`.
    Done { text: String },
    /// Why it stopped, in words.
    Error(String),
    /// The helper is gone.
    Exited,
}

/// The helper to run: `ST_LISTEN` when set (any machine, so a stand-in can be tested), else the
/// one inside the SmallTalk.app this stui runs from, else the try-out copy beside it.
pub fn helper() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ST_LISTEN").filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }
    if !cfg!(target_os = "macos") {
        return None;
    }
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    candidates(&exe, dirs_home().as_deref())
        .into_iter()
        .find(|path| path.is_file())
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Whether this stui can listen, looked up once.
pub fn available() -> bool {
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| helper().is_some())
}

/// Where a helper may be, for a stui at `exe`: in its app's Helpers, then in the try-out folder.
fn candidates(exe: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    const INNER: &str = "StListen.app/Contents/MacOS/st-listen";
    let mut found = Vec::new();
    // SmallTalk.app/Contents/MacOS/st3 → SmallTalk.app/Contents/Helpers/StListen.app
    if let Some(contents) = exe.parent().and_then(Path::parent) {
        found.push(contents.join("Helpers").join(INNER));
    }
    if let Some(folder) = exe.parent() {
        found.push(folder.join(INNER));
    }
    if let Some(home) = home {
        found.push(home.join(".local/share/stui-try").join(INNER));
    }
    found
}

/// Why voice is not offered here, in words.
pub fn unavailable() -> &'static str {
    if cfg!(target_os = "macos") {
        "voice needs SmallTalk.app's speech helper, which this install does not have yet"
    } else {
        "voice works on a Mac for now (macOS 26 and later)"
    }
}

/// One listening, from start to its words.
pub struct Listening {
    child: Child,
    stdin: Option<ChildStdin>,
    pub events: mpsc::Receiver<Event>,
}

impl Listening {
    /// Start the helper; its events arrive on `events`, read on each pass of the screen's loop.
    pub fn start(helper: &Path) -> std::io::Result<Listening> {
        let mut child = Command::new(helper)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped stdout");
        let (sender, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Some(event) = parse(&line)
                    && sender.send(event).is_err()
                {
                    return;
                }
            }
            let _ = sender.send(Event::Exited);
        });
        Ok(Listening {
            child,
            stdin,
            events,
        })
    }

    /// Stop listening; the helper answers with `Done` and everything it heard.
    pub fn finish(&mut self) {
        self.say("finish");
    }

    /// Stop at once and keep nothing.
    pub fn cancel(mut self) {
        self.say("cancel");
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn say(&mut self, word: &str) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = writeln!(stdin, "{word}");
            let _ = stdin.flush();
        }
    }
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.stdin = None;
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// One line of the helper's output as an event; anything else is skipped.
pub fn parse(line: &str) -> Option<Event> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let text = |key: &str| value.get(key).and_then(|v| v.as_str()).unwrap_or_default();
    Some(match text("event") {
        "ready" => Event::Ready {
            device: text("device").to_owned(),
        },
        "level" => Event::Level(value.get("level")?.as_f64()?.clamp(0.0, 1.0) as f32),
        "text" => Event::Text {
            text: text("text").to_owned(),
            settled: value.get("final").and_then(|v| v.as_bool()) == Some(true),
        },
        "silent" => Event::Silent {
            device: text("device").to_owned(),
        },
        "done" => Event::Done {
            text: text("text").to_owned(),
        },
        "error" => Event::Error(text("message").to_owned()),
        _ => return None,
    })
}

/// Recent levels as a waveform of block characters, newest on the right, `width` wide.
pub fn waveform(levels: &[f32], width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let shown = &levels[levels.len().saturating_sub(width)..];
    let mut line: String = std::iter::repeat_n(' ', width - shown.len()).collect();
    line.extend(shown.iter().map(|level| {
        let index = (level.clamp(0.0, 1.0) * (BARS.len() - 1) as f32).round() as usize;
        BARS[index]
    }));
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_helper_lines_become_events() {
        assert_eq!(
            parse(r#"{"event":"ready","device":"MacBook Pro Microphone"}"#),
            Some(Event::Ready {
                device: "MacBook Pro Microphone".into()
            })
        );
        assert_eq!(
            parse(r#"{"event":"level","level":0.77000000000000002}"#),
            Some(Event::Level(0.77))
        );
        assert_eq!(
            parse(r#"{"final":true,"event":"text","text":"Hello there."}"#),
            Some(Event::Text {
                text: "Hello there.".into(),
                settled: true
            })
        );
        assert_eq!(
            parse(r#"{"event":"done","text":"Hello there."}"#),
            Some(Event::Done {
                text: "Hello there.".into()
            })
        );
        assert_eq!(
            parse(r#"{"event":"error","message":"the microphone is off"}"#),
            Some(Event::Error("the microphone is off".into()))
        );
        assert_eq!(parse("not json"), None);
        assert_eq!(parse(r#"{"event":"later"}"#), None);
    }

    #[test]
    #[cfg(unix)]
    fn a_helper_is_run_and_told_to_finish() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("st-listen");
        std::fs::write(
            &helper,
            "#!/bin/sh\n\
             echo '{\"event\":\"ready\",\"device\":\"Stand-in\"}'\n\
             echo '{\"event\":\"text\",\"text\":\"hello\",\"final\":false}'\n\
             read word\n\
             echo \"{\\\"event\\\":\\\"done\\\",\\\"text\\\":\\\"hello $word\\\"}\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut listening = Listening::start(&helper).unwrap();
        let next = |listening: &Listening| {
            listening
                .events
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
        };
        assert_eq!(
            next(&listening),
            Event::Ready {
                device: "Stand-in".into()
            }
        );
        assert!(matches!(next(&listening), Event::Text { .. }));
        listening.finish();
        assert_eq!(
            next(&listening),
            Event::Done {
                text: "hello finish".into()
            }
        );
        assert_eq!(next(&listening), Event::Exited);
    }

    #[test]
    fn the_waveform_is_newest_on_the_right() {
        assert_eq!(waveform(&[0.0, 0.5, 1.0], 5), "  ▁▅█");
        assert_eq!(waveform(&[1.0; 10], 3), "███");
    }

    #[test]
    fn the_helper_is_looked_for_in_the_app_then_the_try_out_folder() {
        let found = candidates(
            Path::new("/Apps/SmallTalk.app/Contents/MacOS/st3"),
            Some(Path::new("/home/example")),
        );
        assert_eq!(
            found[0],
            Path::new("/Apps/SmallTalk.app/Contents/Helpers/StListen.app/Contents/MacOS/st-listen")
        );
        assert_eq!(
            found[2],
            Path::new("/home/example/.local/share/stui-try/StListen.app/Contents/MacOS/st-listen")
        );
    }
}
