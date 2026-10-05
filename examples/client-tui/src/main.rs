use anyhow::{Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use st3_client::{Client, discover_unix_endpoint, set_client_name};
use st3_client_tui::{App, send};
use std::{sync::mpsc, time::Duration};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--help") {
        println!(
            "Usage: st3-client-tui person/NAME [UNIX_SOCKET]\n↑/↓ selects an agent; type then Enter sends; Esc clears; Ctrl-C quits."
        );
        return Ok(());
    }
    if args.is_empty() || args.len() > 2 || !args[0].starts_with("person/") {
        bail!("Use st3-client-tui person/NAME [UNIX_SOCKET]; --help shows keys");
    }
    let explicit = args
        .get(1)
        .map(Into::into)
        .or_else(|| std::env::var_os("ST3_ENDPOINT").map(Into::into));
    let socket = discover_unix_endpoint(explicit)?;
    set_client_name(format!(
        "smalltalk-example-tui {}",
        env!("CARGO_PKG_VERSION")
    ));
    let (updates, incoming) = mpsc::channel();
    let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(st3_feed::run(
        Client::unix_as(socket, &args[0]),
        updates,
        receiver,
    ));
    let (results, sent) = mpsc::channel();
    let mut app = App::default();
    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        loop {
            while let Ok(update) = incoming.try_recv() {
                if let Some(command) = app.update(update) {
                    commands.send(command)?;
                }
            }
            while let Ok(result) = sent.try_recv() {
                app.sent(result);
            }
            terminal.draw(|frame| app.draw(frame))?;
            if !event::poll(Duration::from_millis(50))? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                KeyCode::Up | KeyCode::Down => {
                    if let Some(command) = app.select(if key.code == KeyCode::Up { -1 } else { 1 })
                    {
                        commands.send(command)?;
                    }
                }
                KeyCode::Enter => {
                    if let Some(client) = app.client.clone()
                        && let Some(request) = app.submit()
                    {
                        let results = results.clone();
                        tokio::spawn(async move {
                            let _ = results.send(send(&client, &request).await);
                        });
                    }
                }
                KeyCode::Esc if !app.sending => {
                    app.pending = None;
                    app.draft.clear();
                }
                KeyCode::Backspace if !app.sending && app.pending.is_none() => {
                    app.draft.pop();
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && !app.sending
                        && app.pending.is_none() =>
                {
                    app.draft.push(ch)
                }
                _ => {}
            }
        }
        Ok(())
    })();
    ratatui::restore();
    drop(commands);
    task.abort(); // no queued mutations; closing the runtime cancels unfinished client tasks
    result
}
