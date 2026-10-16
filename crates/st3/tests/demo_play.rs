//! `st demo play` end to end: the real st binary under a PTY in a throwaway HOME resets its own
//! instance, seeds a demo, plays timed and key-paced beats in the terminal interface, and leaves
//! no daemon or scripted agent behind when the presenter quits.
use alacritty_terminal::{
    event::{Event, EventListener},
    grid::Dimensions,
    index::{Column, Line},
    term::{Config, Term},
    vte::ansi::Processor,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const ROWS: usize = 40;
const COLUMNS: usize = 160;
/// F8, as an xterm sends it.
const ADVANCE: &str = "\x1b[19~";

struct Size;
impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        ROWS
    }
    fn screen_lines(&self) -> usize {
        ROWS
    }
    fn columns(&self) -> usize {
        COLUMNS
    }
}
struct Events;
impl EventListener for Events {
    fn send_event(&self, _: Event) {}
}

const DEMO: &str = r#"
version 2
demo "garden-check" {
  title "A garden check"
  me "avery"
  machine "greenhouse"
}
seed {
  agent "garden/gardener" { name "Gardener"; }
  mission "garden/watering" state="ready" {
    goal "Water the invented greenhouse."
    step "water" { assigned-to "agent/garden/gardener"; goal "Water the beds."; }
  }
  say "agent/garden/gardener" ago="10m" "The tomatoes look thirsty this morning."
  run "garden/watering" id="garden/watering/1"
  document "garden/notes" file="notes.md"
}
beats {
  open "agent/garden/gardener"
  say "agent/garden/gardener" after="300ms" "Watering bed three of the invented greenhouse now."
  claim "agent/garden/gardener" run="garden/watering/1" step="water"
  open "doc/garden/notes" after="200ms" split="right" by="agent/garden/gardener"
  focus "agent/garden/gardener" after="400ms"
  type "Thanks, gardener" after="200ms" speed="80/s"
}
"#;

/// The `pty` the daemon runs its agents in, from this PATH, as CI and source installs have it.
fn pty() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("pty"))
            .find(|candidate| candidate.is_file())
    })
}

fn processes_mentioning(needle: &str) -> Vec<String> {
    let output = std::process::Command::new("ps")
        .args(["-eo", "pid=,args="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains(needle))
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_demo_plays_in_its_own_instance_and_ends_with_the_interface() {
    let Some(pty_binary) = pty() else {
        eprintln!("skipped: no pty on PATH for the demo's agents");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let home = root.path();
    let file = home.join("garden.kdl");
    std::fs::write(&file, DEMO).unwrap();
    std::fs::write(
        home.join("notes.md"),
        "# Greenhouse notes\n\nWater bed three twice a week in summer.\n",
    )
    .unwrap();
    // st launches agents through the login shell, which finds pty on its PATH.
    let profile = format!(
        "export PATH=\"{}:$PATH\"\n",
        pty_binary.parent().unwrap().display()
    );
    for name in [".profile", ".bash_profile", ".zprofile"] {
        std::fs::write(home.join(name), &profile).unwrap();
    }

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: ROWS as u16,
            cols: COLUMNS as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(test_env!("CARGO_BIN_EXE_st3"));
    command.args(["--instance", "demo-test", "demo", "play"]);
    command.arg(&file);
    command.env_clear();
    command.env("HOME", home);
    command.env(
        "PATH",
        format!(
            "{}:/usr/bin:/bin",
            pty_binary.parent().unwrap().display()
        ),
    );
    command.env("TERM", "xterm-256color");
    command.cwd(home);
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let player = child.process_id().unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let screen = Arc::new(Mutex::new(String::new()));
    let printed = Arc::new(Mutex::new(String::new()));
    {
        let screen = screen.clone();
        let printed = printed.clone();
        std::thread::spawn(move || {
            let mut term = Term::new(Config::default(), &Size, Events);
            let mut parser: Processor = Processor::new();
            let mut bytes = [0; 16384];
            while let Ok(length) = reader.read(&mut bytes) {
                if length == 0 {
                    break;
                }
                printed
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&bytes[..length]));
                parser.advance(&mut term, &bytes[..length]);
                *screen.lock().unwrap() = (0..ROWS)
                    .map(|row| {
                        (0..COLUMNS)
                            .map(|col| term.grid()[Line(row as i32)][Column(col)].c)
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
            }
        });
    }
    let instance = home.join(".st-instance/demo-test");
    let wait = |label: &str, seconds: u64, predicate: &dyn Fn(&str) -> bool| -> String {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            let now = screen.lock().unwrap().clone();
            if predicate(&now) {
                return now;
            }
            assert!(
                Instant::now() < deadline,
                "{label} did not appear:\n{now}\n--- printed ---\n{}\n--- play log ---\n{}\n--- daemon log tail ---\n{}",
                printed.lock().unwrap(),
                std::fs::read_to_string(instance.join("state/st3-demo/play.log")).unwrap_or_default(),
                std::fs::read_to_string(instance.join("state/st3-demo/daemon.log"))
                    .unwrap_or_default()
                    .lines()
                    .rev()
                    .take(30)
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    wait("the live interface", 90, &|screen| screen.contains("live"));
    // The instance holds the demo's person and machine, and nothing outside it was written.
    let config =
        std::fs::read_to_string(instance.join("config/st3/config.toml")).unwrap();
    assert!(config.contains("person = \"person/avery\""), "{config}");
    assert!(config.contains("node = \"greenhouse\""), "{config}");
    assert!(!home.join(".config/st3").exists());
    assert!(!home.join(".local/state/st3").exists());

    // The first beat waits for the presenter: nothing opens until F8.
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        !screen.lock().unwrap().contains("thirsty"),
        "the conversation opened before the key:\n{}",
        screen.lock().unwrap()
    );
    writer.write_all(ADVANCE.as_bytes()).unwrap();
    writer.flush().unwrap();
    // The seeded history, then the timed beat streaming in after it.
    wait("the seeded message", 30, &|screen| screen.contains("thirsty"));
    wait("the timed beat", 30, &|screen| {
        screen.contains("Watering bed three of the invented greenhouse now.")
    });

    // The claim waits for the key; the document, the focus and the typing follow it on timers.
    writer.write_all(ADVANCE.as_bytes()).unwrap();
    writer.flush().unwrap();
    wait("the document beside the conversation", 30, &|screen| {
        screen.contains("Water bed three twice a week in summer.")
            && screen.contains("Watering bed three of the invented greenhouse now.")
    });
    wait("the typed draft", 30, &|screen| screen.contains("Thanks, gardener"));
    let log = std::fs::read_to_string(instance.join("state/st3-demo/play.log")).unwrap();
    assert!(log.contains("beat 3 (line 21)"), "{log}");
    assert!(log.contains("work claim step-run/"), "{log}");
    assert!(!log.contains("failed"), "{log}");

    // Ctrl+Q leaves the draft unsent and ends the demo, its daemon and its agent.
    writer.write_all(b"\x1b").unwrap();
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    writer.write_all(b"\x11").unwrap();
    writer.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "the demo did not end on Ctrl+Q");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        printed.lock().unwrap().contains("The demo ended"),
        "{}",
        printed.lock().unwrap()
    );
    assert!(!instance.join("state/st3-demo/daemon.pid").exists());
    let stand_in = format!("demo seat --player {player}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !processes_mentioning(&stand_in).is_empty() {
        assert!(
            Instant::now() < deadline,
            "a scripted agent outlived the demo: {:?}",
            processes_mentioning(&stand_in)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = Path::new(&instance);
}
