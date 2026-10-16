//! `st ui open` against the real terminal interface: a seat asks, in a separate process, for an
//! agent or a mission to be shown in a tab or a split, and the picture in a PTY changes.
use alacritty_terminal::{
    event::{Event, EventListener},
    grid::Dimensions,
    index::{Column, Line},
    term::{Config, Term},
    vte::ansi::Processor,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use st3::{api::AppState, store::Store};
use st3_client::Client;
use std::{
    io::{Read, Write},
    path::Path,
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, watch};

const ROWS: usize = 40;
const COLUMNS: usize = 160;
const PERSON: &str = "person/avery";

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

struct Tui {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    screen: Arc<Mutex<String>>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}
impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The environment every st process of this test shares: one person, one daemon, one state home.
fn environment(root: &Path, socket: &Path) -> Vec<(&'static str, String)> {
    let path = |name: &str| root.join(name).display().to_string();
    vec![
        ("HOME", root.display().to_string()),
        ("XDG_CONFIG_HOME", path("config")),
        ("XDG_CACHE_HOME", path("cache")),
        ("XDG_STATE_HOME", path("state")),
        ("ST3_ENDPOINT", socket.display().to_string()),
        ("ST3_PERSON", PERSON.into()),
        ("TERM", "xterm-256color".into()),
    ]
}

impl Tui {
    fn start(root: &Path, socket: &Path) -> Self {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: ROWS as u16,
                cols: COLUMNS as u16,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(test_env!("CARGO_BIN_EXE_st3"));
        command.arg("ui");
        command.env_clear();
        for (name, value) in environment(root, socket) {
            command.env(name, value);
        }
        command.cwd(root);
        let child = pty.slave.spawn_command(command).unwrap();
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader().unwrap();
        let writer = pty.master.take_writer().unwrap();
        let screen = Arc::new(Mutex::new(String::new()));
        let observed = screen.clone();
        std::thread::spawn(move || {
            let mut term = Term::new(Config::default(), &Size, Events);
            let mut parser: Processor = Processor::new();
            let mut bytes = [0; 16384];
            while let Ok(length) = reader.read(&mut bytes) {
                if length == 0 {
                    break;
                }
                parser.advance(&mut term, &bytes[..length]);
                let text = (0..ROWS)
                    .map(|row| {
                        (0..COLUMNS)
                            .map(|col| term.grid()[Line(row as i32)][Column(col)].c)
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                *observed.lock().unwrap() = text;
            }
        });
        Self {
            child,
            writer,
            screen,
            _master: pty.master,
        }
    }

    fn send(&mut self, text: &str) {
        self.writer.write_all(text.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    fn screen(&self) -> String {
        self.screen.lock().unwrap().clone()
    }

    async fn wait(&self, label: &str, predicate: impl Fn(&str) -> bool) -> String {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
        loop {
            let screen = self.screen();
            if predicate(&screen) {
                return screen;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{label} did not appear:\n{screen}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn state(root: &Path, node: &str) -> AppState {
    let store = Store::open_memory(node).unwrap();
    AppState {
        owning_person: None,
        store: Arc::new(store),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0).0,
        node: node.into(),
        state_dir: root.into(),
        pty_root: root.join("pty"),
        pty_binary: root.join("unused-pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: Default::default(),
    }
}

/// An in-memory store holding `sources`, served on a unix socket for `PERSON`.
async fn serve(
    root: &Path,
    sources: &[&str],
) -> (
    std::path::PathBuf,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let state = state(root, "seat-one");
    for source in sources {
        let intent = st3::graph::parse_intent(source, state.store.origin()).unwrap();
        state.store.apply_internal(&intent, "ui-open-test").unwrap();
    }
    let socket = root.join("trusted.sock");
    let server = tokio::spawn({
        let path = socket.clone();
        let app = st3::api::router(state.clone());
        async move { st3::api::serve_unix(&path, app).await }
    });
    let client = Client::unix_as(&socket, PERSON);
    for _ in 0..200 {
        if client.capabilities().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (socket, server)
}

/// `st ui open …` from a seat, as its own process.
async fn seat_opens(root: &Path, socket: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(test_env!("CARGO_BIN_EXE_st3"));
    command
        .arg("ui")
        .arg("open")
        .args(args)
        .env_clear()
        .stdin(Stdio::null());
    for (name, value) in environment(root, socket) {
        command.env(name, value);
    }
    command.env("ST_AGENT", "agent/seat-one.assistant");
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

fn lower(screen: &str) -> String {
    screen.to_lowercase()
}

fn said(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seat_opens_agents_and_missions_in_splits_of_the_running_terminal_interface() {
    let root = tempfile::tempdir().unwrap();
    let (socket, server) = serve(
        root.path(),
        &[
            "version 2\nagent \"assistant\" { workspace \"/tmp\"; command \"true\" }\n\
             agent \"gardener\" { workspace \"/tmp\"; command \"true\" }",
            "version 2\nmission \"garden/notes\" state=\"ready\" {\n  goal \"Write a garden note.\"\n  \
             step \"draft\" { goal \"Draft it.\" }\n}",
        ],
    )
    .await;

    // Nobody has a terminal interface open: the seat is told so and nothing waits for later.
    let alone = seat_opens(
        root.path(),
        &socket,
        &["agent/seat-one.gardener", "--wait", "1"],
    )
    .await;
    assert!(!alone.status.success(), "{}", said(&alone));
    assert!(
        said(&alone).contains("No terminal interface"),
        "{}",
        said(&alone)
    );

    let mut tui = Tui::start(root.path(), &socket);
    tui.wait("the live interface", |screen| screen.contains("live"))
        .await;
    let before = tui.screen();
    assert!(!lower(&before).contains("gardener"), "{before}");

    // A split, keeping the person's focus where it was.
    let output = seat_opens(
        root.path(),
        &socket,
        &["agent/seat-one.gardener", "--split", "--keep-focus"],
    )
    .await;
    assert!(output.status.success(), "{}", said(&output));
    assert!(said(&output).contains("focus stayed"), "{}", said(&output));
    let screen = tui
        .wait("the agent's split", |screen| {
            lower(screen).contains("agent/seat-one.gardener")
        })
        .await;
    // The person is told why the screen changed, by the seat's own name.
    assert!(
        lower(&screen).contains("assistant opened gardener"),
        "{screen}"
    );
    assert!(
        screen.contains("Empty. Ctrl+K opens something here"),
        "the person's split was left alone:\n{screen}"
    );

    // The mission view, in a split below, taking the focus.
    let run = format!(
        "mission-run/garden/notes/{}",
        chrono::Utc::now().format("%F")
    );
    let output = seat_opens(root.path(), &socket, &[&run, "--split", "--below"]).await;
    assert!(output.status.success(), "{}", said(&output));
    assert!(said(&output).contains("focus moved"), "{}", said(&output));
    let screen = tui
        .wait("the mission view", |screen| {
            lower(screen).contains("garden/notes")
        })
        .await;
    assert!(
        lower(&screen).contains("agent/seat-one.gardener"),
        "the agent stays open beside the mission:\n{screen}"
    );

    // Asking again for what is open shows it; it is not opened twice.
    let again = seat_opens(
        root.path(),
        &socket,
        &["agent/seat-one.gardener", "--split"],
    )
    .await;
    assert!(again.status.success(), "{}", said(&again));
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        lower(&tui.screen())
            .matches("agent/seat-one.gardener")
            .count(),
        lower(&screen).matches("agent/seat-one.gardener").count(),
        "the same pane, not another"
    );

    // Something st has never heard of is answered as not found, after a short grace.
    let missing = seat_opens(root.path(), &socket, &["agent/seat-one.ghost"]).await;
    assert!(!missing.status.success(), "{}", said(&missing));
    assert!(
        said(&missing).contains("st has no agent/seat-one.ghost"),
        "{}",
        said(&missing)
    );

    // A name that is not an agent, mission or machine never reaches the interface.
    let refused = seat_opens(root.path(), &socket, &["attention/one"]).await;
    assert!(!refused.status.success());
    assert!(said(&refused).contains("agent/…"), "{}", said(&refused));

    tui.send("\x11");
    server.abort();
}

/// `st work ask …` from a seat, as its own process.
async fn seat_asks(root: &Path, socket: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(test_env!("CARGO_BIN_EXE_st3"));
    command
        .arg("work")
        .arg("ask")
        .args(args)
        .env_clear()
        .stdin(Stdio::null());
    for (name, value) in environment(root, socket) {
        command.env(name, value);
    }
    command.env("ST_AGENT", "agent/seat-one.assistant");
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

impl Tui {
    /// A left click at the first cell of `needle`, in the lowest row that has it.
    fn click(&mut self, needle: &str) {
        let screen = self.screen();
        let (row, line) = screen
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains(needle))
            .last()
            .unwrap_or_else(|| panic!("{needle} is not on the screen:\n{screen}"));
        let column = line.chars().collect::<Vec<_>>();
        let column = column
            .windows(needle.chars().count())
            .position(|window| window.iter().collect::<String>() == needle)
            .unwrap();
        self.send(&format!(
            "\x1b[<0;{};{}M\x1b[<0;{};{}m",
            column + 2,
            row + 1,
            column + 2,
            row + 1
        ));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_alert_a_seat_raises_for_itself_shows_above_the_input_and_is_dismissed_there() {
    let root = tempfile::tempdir().unwrap();
    let (socket, server) = serve(
        root.path(),
        &["version 2\nagent \"assistant\" { workspace \"/tmp\"; command \"true\" }"],
    )
    .await;
    let mut tui = Tui::start(root.path(), &socket);
    tui.wait("the live interface", |screen| screen.contains("live"))
        .await;
    let shown = seat_opens(root.path(), &socket, &["agent/seat-one.assistant"]).await;
    assert!(shown.status.success(), "{}", said(&shown));
    tui.wait("the agent's conversation", |screen| {
        lower(screen).contains("agent/seat-one.assistant")
    })
    .await;

    let request = root.path().join("alert.json");
    std::fs::write(
        &request,
        r#"{"version": 1, "type": "decision",
            "question": "Your demo folder is ready.",
            "why_person": "Nothing waits on you; this is a notice you can dismiss.",
            "answers": [
              {"id": "ok", "label": "Got it", "outcome": "accept",
               "consequence": "The notice goes away; nothing else happens."},
              {"id": "later", "label": "Dismiss", "outcome": "decline",
               "consequence": "The notice goes away; nothing else happens."}]}"#,
    )
    .unwrap();
    let asked = seat_asks(
        root.path(),
        &socket,
        &[
            "--for",
            PERSON,
            "--title",
            "Your demo folder is ready",
            "--request",
            request.to_str().unwrap(),
            "--new-run",
            "demo-folder-ready",
            "--idempotency-key",
            "demo-folder-ready-1",
        ],
    )
    .await;
    assert!(asked.status.success(), "{}", said(&asked));

    tui.wait("the alert band", |screen| screen.contains("1 alert"))
        .await;
    tui.click("1 alert");
    tui.wait("the alert card", |screen| screen.contains("Got it"))
        .await;
    tui.click("Got it");
    tui.wait("the alert gone", |screen| !screen.contains("1 alert"))
        .await;
    server.abort();
}

/// The request the bundled guide tells a seat to raise for itself is one the daemon accepts.
/// (Lives here, not in `onboarding.rs`, because that file is compiled into the binary too, where
/// the daemon's modules are reached as `st3::…`.)
#[test]
fn the_guides_alert_request_is_one_the_daemon_accepts() {
    const GUIDE: &str = include_str!("../../../docs/st3/onboarding-guide.md");
    let start = GUIDE.find("<<'JSON'\n").expect("the alert request") + "<<'JSON'\n".len();
    let json = &GUIDE[start..];
    let json = &json[..json.find("\nJSON\n").expect("the end of the request")];
    let value: serde_json::Value = serde_json::from_str(json).unwrap();
    let (request, _) = st3::person_request::StructuredRequest::parse(&value).unwrap();
    request.validate().unwrap();
    assert_eq!(request.answers.len(), 2, "one tap to dismiss, either way");
}
