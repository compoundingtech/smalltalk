//! Opening an agent attaches its terminal, an alert shows on top of it, and a dropped link
//! comes back by itself: a production Ui in a real PTY, attached to a real program under the
//! PTY daemon. Only the graph is invented (the demo's agents, and one alert added on cue), and
//! the live loop's two terminal jobs are done here as it does them: an `OpenTerminal` effect
//! attaches over a stream, and `Ui::reattach_due` decides when a lost one tries again.
use super::*;
use std::path::PathBuf;

#[test]
fn attach_default_probe() {
    let output = std::process::Command::new("python3")
        .arg(test_env!(
            "CARGO_MANIFEST_DIR",
            "/tests/attach_default_probe.py"
        ))
        .arg("--worker")
        .arg(std::env::current_exe().unwrap())
        .env_remove("ST_AGENT")
        .output()
        .expect("python3 is required by the PTY probe (provided by CI)");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The agent the probe opens: the demo's, with a terminal.
const AGENT: &str = "agent/example/atlas/builder";

fn ask(agent: &str) -> Attention {
    Attention {
        id: "ask/probe".into(),
        tier: Tier::Stopped,
        title: "Ship the release?".into(),
        waiting: None,
        age: "now".into(),
        mission: None,
        agent: Some(agent.into()),
        kind: AttentionKind::Request {
            from: "Atlas Builder".into(),
            from_id: agent.into(),
            question: "Ship the release?".into(),
            structured: None,
            images: Vec::new(),
        },
        actions: vec!["work.done".into()],
        related: vec![],
        raised_by: None,
        blocked: None,
        conversations: vec![agent.into()],
    }
}

// Launched only by the Python harness, with its own controlling PTY and isolated state.
#[test]
#[ignore = "worker for attach_default_probe; needs harness-owned PTYs"]
fn attach_default_probe_worker() -> Result<()> {
    let root = PathBuf::from(std::env::var_os("STUI_PROBE_ROOT").expect("probe root"));
    let socket = PathBuf::from(std::env::var_os("STUI_PROBE_SOCKET").expect("probe socket"));
    let _guard = Guard::enter(true)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut ui = Ui::new(demo::world());
    ui.live = true;
    // This device's own choice, from the isolated XDG_STATE_HOME the harness wrote (or none).
    ui.load_prefs();
    ui.glasses = Some(glass::Glasses::open(None, None));
    let mut answered: Vec<String> = Vec::new();
    let mut opened = 0;
    let mut last = String::new();
    let deadline = Instant::now() + Duration::from_secs(180);
    while !root.join("stop").exists() && Instant::now() < deadline {
        if let Ok(action) = std::fs::read_to_string(root.join("ui-action")) {
            std::fs::remove_file(root.join("ui-action"))?;
            match action.as_str() {
                // As the palette or the sidebar opens an agent.
                "open" => ui.open_in_glass(Pane::Agent(Some(AGENT.into())), glass::Open::Tab),
                "open-unmanaged" => {
                    let id = ui
                        .world
                        .agents
                        .items()
                        .iter()
                        .find(|agent| !agent.terminal || agent.unmanaged)
                        .map(|agent| agent.id.clone())
                        .expect("an agent without a terminal");
                    ui.open_in_glass(Pane::Agent(Some(id)), glass::Open::Tab)
                }
                // st lists a new alert for the attached agent.
                "alert" => ui.world.attention = Load::Ready(vec![ask(AGENT)]),
                _ => panic!("unknown fixture action"),
            }
        }
        // The live loop's steps, in its order.
        ui.step_terminal_hold();
        ui.step_default_view();
        for effect in std::mem::take(&mut ui.effects) {
            match effect {
                Effect::OpenTerminal { agent } => {
                    opened += 1;
                    ui.park_for(&agent);
                    ui.terminal = Some(TerminalView {
                        agent: agent.clone(),
                        name: "Probe".into(),
                        title: "Probe".into(),
                        lines: vec![],
                        cursor: None,
                        stale: None,
                        ended: None,
                        native: Some(pty::NativeTerminal::spawn(
                            std::os::unix::net::UnixStream::connect(&socket)?,
                            "copper",
                            "one".into(),
                            24,
                            80,
                        )),
                    });
                }
                Effect::CloseTerminal => ui.terminal = None,
                Effect::Attention {
                    id, answer, reason, ..
                } => answered.push(format!(
                    "{id}={}",
                    answer.or(reason).unwrap_or_default()
                )),
                _ => {}
            }
        }
        if let Some((agent, _)) = ui.reattach_due()
            && let Some(native) = ui.terminal_view(&agent).and_then(|view| view.native.as_ref())
        {
            match std::os::unix::net::UnixStream::connect(&socket) {
                Ok(stream) => native.reconnect(stream, "copper"),
                Err(error) => native.retry_failed(error.to_string()),
            }
        }
        let drawn = terminal.draw(|frame| ui.render(frame))?;
        let width = drawn.area.width as usize;
        let text: Vec<String> = drawn
            .buffer
            .content
            .chunks(width.max(1))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        let alerts: Vec<_> = ui
            .frame
            .borrow()
            .hits
            .iter()
            .filter_map(|(rect, hit)| match hit {
                Hit::Alert(_, tap) => Some(serde_json::json!({
                    "rect": [rect.x, rect.y, rect.width, rect.height],
                    "tap": format!("{tap:?}"),
                })),
                _ => None,
            })
            .collect();
        let native = ui.native_terminal();
        let status = serde_json::json!({
            "pane": ui.focused_pane().map(|pane| pane.key()),
            "conversation_first": ui.conversation_first(),
            "opened": opened,
            "attached": native.is_some_and(|native| native.attached()),
            "lost": native.and_then(|native| native.lost()),
            "tries": native.and_then(|native| native.reconnecting()).map(|(tries, _)| tries),
            "focused": ui.terminal_focused(),
            "home": ui.home_open(),
            "body": ui.terminal_body.get().map(|body| [body.x, body.y, body.width, body.height]),
            "alerts": alerts,
            "answered": answered,
            "flash": ui.flash.as_ref().map(|(text, _)| text.to_string()),
            "text": text,
        })
        .to_string();
        if status != last {
            std::fs::write(root.join("status.new"), &status)?;
            std::fs::rename(root.join("status.new"), root.join("status.json"))?;
            last = status;
        }
        if event::poll(Duration::from_millis(5))? {
            loop {
                ui.input_event(event::read()?);
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// What a key costs from press to drawn frame with an agent's terminal attached in a space,
/// with an alert on top when `STUI_BENCH_ALERT` is set: `cargo test -p stui --lib --release
/// attach_redraw_cost -- --ignored --nocapture`. Run it on two trees to compare them.
#[test]
#[ignore = "a measurement, not a check"]
fn attach_redraw_cost() {
    use ratatui::backend::TestBackend;
    let mut ui = Ui::new(demo::world());
    ui.glasses = Some(glass::Glasses::open(None, None));
    ui.open_in_glass(Pane::Terminal(AGENT.into()), glass::Open::Tab);
    let (stui, _daemon) = std::os::unix::net::UnixStream::pair().unwrap();
    ui.terminal = Some(TerminalView {
        agent: AGENT.into(),
        name: "Probe".into(),
        title: "Probe".into(),
        lines: vec![],
        cursor: None,
        stale: None,
        ended: None,
        native: Some(pty::NativeTerminal::spawn(stui, "bench", "one".into(), 38, 120)),
    });
    if std::env::var_os("STUI_BENCH_ALERT").is_some() {
        ui.world.attention = Load::Ready(vec![ask(AGENT)]);
    }
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    let mut times = Vec::new();
    for round in 0..3000 {
        let started = Instant::now();
        ui.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        ui.effects.clear();
        terminal.draw(|frame| ui.render(frame)).unwrap();
        if round >= 500 {
            times.push(started.elapsed());
        }
    }
    times.sort();
    let at = |q: f64| times[((times.len() - 1) as f64 * q) as usize].as_micros();
    println!(
        "key to drawn frame, attached: p50 {}µs p95 {}µs p99 {}µs over {}",
        at(0.5),
        at(0.95),
        at(0.99),
        times.len()
    );
}
