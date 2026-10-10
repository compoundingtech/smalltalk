//! A reusable renderer; this crate owns no product layout or sidebar tabs.
use crate::{PanelDocument, Sample, Unit};
use anyhow::{Result, ensure};
use ratatui::{Frame, Terminal, backend::TestBackend, crossterm::event::{self, Event, KeyCode}, layout::Rect, widgets::{Block, Borders, Paragraph}};
use std::io::IsTerminal;

pub fn render(frame: &mut Frame, area: Rect, document: &PanelDocument, client: &str) {
    let block = Block::default().borders(Borders::ALL).title(format!("{client} · composable metrics · r refresh / q quit"));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    for (index, panel) in document.panels.iter().enumerate() {
        let y = inner.y.saturating_add(index as u16 * 4);
        if y >= inner.bottom() { break; }
        let value = match &panel.sample {
            Sample::Available { value, age_seconds, freshness, .. } => {
                let text = match panel.unit {
                    Unit::Count => format!("{value:.0}"),
                    Unit::Ratio => format!("{:.1}%", value * 100.0),
                };
                format!("{text} · {freshness:?} · source age {age_seconds:.1}s")
            }
            Sample::Unavailable { reason, detail } => format!("Unavailable ({reason:?}): {detail}"),
        };
        frame.render_widget(Paragraph::new(format!("{} [{}] · {:?}\n{}", panel.title, panel.subject, panel.source, value)), Rect::new(inner.x, y, inner.width, (inner.bottom() - y).min(3)));
    }
}

/// Opt-in modes prove the exact same composition and widget in both products.
/// This does not resurrect either of the retired monitor tabs.
pub fn run_mode(client: &str) -> Result<bool> {
    let args: Vec<String> = std::env::args().collect();
    let Some(mode) = args.iter().find(|arg| matches!(arg.as_str(), "--metric-panels" | "--metric-panels-json" | "--metric-panels-text")) else { return Ok(false) };
    let host = std::env::var("METRIC_HOST").unwrap_or_else(|_| "dev3".into());
    let base = std::env::var("METRIC_MIMIR_URL").unwrap_or_else(|_| "http://127.0.0.1:42030/prometheus".into());
    let tenant = std::env::var("METRIC_MIMIR_TENANT").unwrap_or_else(|_| "anonymous".into());
    let socket = st3_client::discover_unix_endpoint(std::env::var_os("ST3_ENDPOINT").map(Into::into))?;
    st3_client::set_client_name(format!("{client}-metric-panels-bakeoff"));
    let st = st3_client::Client::unix(socket);
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let fetch = || runtime.block_on(crate::compose(&host, &base, &tenant, &st));
    let mut document = fetch()?;
    match mode.as_str() {
        "--metric-panels-json" => println!("{}", serde_json::to_string_pretty(&document)?),
        "--metric-panels-text" => {
            let mut terminal = Terminal::new(TestBackend::new(90, 14))?;
            terminal.draw(|frame| render(frame, frame.area(), &document, client))?;
            let buffer = terminal.backend().buffer();
            for y in 0..14 {
                let row: String = (0..90).map(|x| buffer[(x,y)].symbol()).collect();
                println!("{}", row.trim_end());
            }
        }
        _ => {
            ensure!(std::io::stdin().is_terminal() && std::io::stdout().is_terminal(), "--metric-panels needs a terminal; use --metric-panels-json or --metric-panels-text for capture");
            let mut terminal = ratatui::try_init()?;
            let result = (|| -> Result<()> {
                loop {
                    terminal.draw(|frame| render(frame, frame.area(), &document, client))?;
                    if let Event::Key(key) = event::read()? {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => break,
                            KeyCode::Char('r') => document = fetch()?,
                            _ => {},
                        }
                    }
                }
                Ok(())
            })();
            ratatui::restore();
            result?;
        }
    }
    Ok(true)
}
