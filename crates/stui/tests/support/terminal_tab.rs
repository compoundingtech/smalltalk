//! The probe runs a real program under the PTY daemon and a production Ui terminal tab
//! under another PTY. Only its graph data is invented; input, parser, transport and painter
//! are the same ones the live client uses. No display, daemon graph, or person's space.
use super::*;
use std::path::PathBuf;

#[test]
fn terminal_tab_probe_baseline() {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/terminal_tab_probe.py"
        ))
        .arg("--worker")
        .arg(std::env::current_exe().unwrap())
        .arg("--check")
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

// Launched only by the Python harness, with its own controlling PTY and isolated session.
#[test]
#[ignore = "worker for terminal_tab_probe_baseline; needs harness-owned PTYs"]
fn terminal_tab_probe_worker() -> Result<()> {
    let root = PathBuf::from(std::env::var_os("STUI_PROBE_ROOT").expect("probe root"));
    let socket = std::env::var_os("STUI_PROBE_SOCKET").expect("probe socket");
    let _guard = Guard::enter(true)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut ui = Ui::new(demo::world());
    ui.glasses = Some(glass::Glasses::open(None, None));
    let agent = if std::env::var_os("STUI_PROBE_AGENT").is_some() {
        "agent/example/atlas/builder"
    } else {
        "terminal/probe"
    }
    .to_owned();
    ui.open_in_glass(Pane::Terminal(agent.clone()), glass::Open::Tab);
    ui.terminal = Some(TerminalView {
        agent,
        name: "Probe".into(),
        title: "Probe".into(),
        lines: vec![],
        cursor: None,
        stale: None,
        ended: None,
        native: Some(pty::NativeTerminal::spawn(
            std::os::unix::net::UnixStream::connect(socket)?,
            "probe",
            "one".into(),
            24,
            80,
        )),
    });
    let mut last = String::new();
    let mut copied = None;
    let mut bell = false;
    let deadline = Instant::now() + Duration::from_secs(180);
    while !root.join("stop").exists() && Instant::now() < deadline {
        terminal.draw(|frame| ui.render(frame))?;
        let native = ui.native_terminal().unwrap();
        let asked = native.asked();
        if asked.copied.is_some() {
            copied = asked.copied;
        }
        bell |= asked.bell;
        let body = ui.terminal_body.get().unwrap();
        let mut buf = Buffer::empty(Rect::new(0, 0, body.width, body.height));
        let area = buf.area;
        let cursor = native.draw(&mut buf, area, true);
        let text: String = buf.content.iter().map(|cell| cell.symbol()).collect();
        let status = serde_json::json!({
            "title": native.title(), "mode": format!("{:?}", native.mode()),
            "mode_bits": native.mode().bits(),
            "body": [body.x, body.y, body.width, body.height],
            "copied": copied, "bell": bell, "text": text,
            "cursor": format!("{cursor:?}"), "scrolled": native.scrolled(),
            "detached": ui.effects.iter().any(|effect| matches!(effect, Effect::CloseTerminal)),
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
