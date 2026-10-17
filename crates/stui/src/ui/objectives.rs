//! Objectives and budgets in the Usage tab, from st's `slo.list` and `slo.history` reads.
//!
//! An objective is a target with windows and an error budget; a budget is an allowance that
//! resets (an account's weekly and 5-hour limits). Both are one row shape, so the list draws
//! them the same way: red when an objective is missed or a budget is nearly spent. A row's id is
//! `slo/<id>`, so the pane that opens it can be named by its key alone, and selecting it shows
//! the explanation and a chart of what was left across the Usage period (at most seven days,
//! which is what st keeps).

use super::doc::{Doc, DocExt};
use super::screens::Item;
use super::theme;
use super::view::{Load, World};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use st3_client::{SloHistory, SloRow};

/// A budget with this little left (percent) reads as nearly spent.
pub const NEARLY_SPENT: f64 = 10.0;

/// The longest span a chart covers, in hours: st keeps seven days of readings.
pub const CHART_HOURS: u64 = 168;

/// The columns a chart is asked for, which a wide pane shows whole.
pub const CHART_STEPS: usize = 72;

const HEIGHT: usize = 8;
const LEVELS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// The list id of an objective or budget.
pub fn id_of(row: &SloRow) -> String {
    format!("slo/{}", row.id)
}

/// The objective or budget id a list id names.
pub fn slo_id(id: &str) -> Option<&str> {
    id.strip_prefix("slo/")
}

/// Whether a row needs the person: an objective is missed, or a budget is nearly spent.
pub fn is_red(row: &SloRow) -> bool {
    row.status == "missed"
        || row.kind == "budget"
            && row.remaining_percent.is_some_and(|left| left <= NEARLY_SPENT)
}

fn color(row: &SloRow) -> ratatui::style::Color {
    if is_red(row) {
        theme::RED
    } else {
        match row.status.as_str() {
            "met" => theme::GREEN,
            "stale" => theme::YELLOW,
            _ => theme::OVERLAY1,
        }
    }
}

fn span(text: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(text.into(), style)
}

fn left(row: &SloRow) -> String {
    row.remaining_percent
        .map_or_else(|| "no data".into(), |left| format!("{left:.0}% left"))
}

/// The list's objectives and budgets, a header each, appended beside the accounts. A read still
/// loading or failed says so in one line, so the pane never looks empty by accident.
pub fn list(world: &World, items: &mut Vec<Item>, ids: &mut Vec<String>) {
    let rows = match &world.slo {
        Load::Ready(rows) => rows,
        Load::Loading => {
            items.push(Item::Note(Line::from(span(
                " Loading objectives and budgets…",
                theme::dim(),
            ))));
            return;
        }
        Load::Failed(why) => {
            items.push(Item::Note(Line::from(span(format!(" {why}"), theme::dim()))));
            return;
        }
    };
    for (kind, title) in [("budget", "budgets"), ("objective", "objectives")] {
        let group = rows.iter().filter(|row| row.kind == kind).collect::<Vec<_>>();
        if group.is_empty() {
            continue;
        }
        items.push(Item::Header {
            title: title.into(),
            count: group.len(),
            color: if group.iter().any(|row| is_red(row)) {
                theme::RED
            } else {
                theme::ACCENT
            },
        });
        for row in group {
            let color = color(row);
            let name = match &row.account {
                Some(account) => format!("{} · {account}", row.id),
                None => row.id.clone(),
            };
            items.push(Item::Row {
                index: ids.len(),
                first: vec![
                    span(" ● ", theme::fg(color)),
                    span(name, theme::bold()),
                ],
                right: vec![span(left(row), theme::strong(color))],
                second: vec![span(
                    format!(
                        "   {} · target {} · {}",
                        row.current, row.target, row.window
                    ),
                    if is_red(row) { theme::fg(theme::RED) } else { theme::dim() },
                )],
                children: Vec::new(),
            });
            ids.push(id_of(row));
        }
    }
}

/// One objective or budget in detail: what it is in plain words, the numbers, and a chart of
/// what was left across the period.
pub fn detail(world: &World, id: &str, hours: u64, width: usize) -> Doc {
    let mut doc = Doc::new();
    let Some(slo) = slo_id(id) else {
        return doc;
    };
    let row = match &world.slo {
        Load::Ready(rows) => rows.iter().find(|row| row.id == slo),
        Load::Loading => {
            doc.line(Line::from(span("Loading objectives and budgets…", theme::dim())));
            return doc;
        }
        Load::Failed(why) => {
            doc.line(Line::from(span(why.clone(), theme::dim())));
            return doc;
        }
    };
    let Some(row) = row else {
        doc.line(Line::from(span(
            format!("{slo} is not an objective or budget this st reports."),
            theme::dim(),
        )));
        return doc;
    };
    let color = color(row);
    doc.line(Line::from(vec![
        span("● ", theme::fg(color)),
        span(row.id.clone(), theme::bold()),
        span(format!("  {}", row.kind), theme::dim()),
    ]));
    doc.line(Line::from(vec![
        span(left(row), theme::strong(color)),
        span(format!(" · {}", row.status), theme::dim()),
    ]));
    doc.blank();
    doc.field("about", &row.about, width, theme::text());
    doc.field("target", &row.target, width, theme::text());
    doc.field("current", &row.current, width, theme::text());
    doc.field("window", &row.window, width, theme::text());
    if let Some(account) = &row.account {
        doc.field("account", account, width, theme::text());
    }
    if let Some(at) = row.resets_at_unix_ms {
        let now = now_ms();
        doc.field(
            "resets",
            &if at > now {
                format!("in {}", span_of(at - now))
            } else {
                "now".into()
            },
            width,
            theme::text(),
        );
    }
    if let Some(at) = row.measured_at_unix_ms {
        doc.field(
            "measured",
            &format!("{} ago", span_of(now_ms().saturating_sub(at))),
            width,
            theme::text(),
        );
    }
    doc.field("explain", &row.explain, width, theme::text());
    doc.blank();
    let hours = hours.min(CHART_HOURS);
    let what = if row.kind == "budget" {
        "allowance left"
    } else {
        "error budget left"
    };
    doc.section(&format!("{what} · last {}", period(hours)), None, width);
    match world.slo_history.get(slo) {
        None | Some(Load::Loading) => {
            doc.line(Line::from(span("Loading the chart…", theme::dim())));
        }
        Some(Load::Failed(why)) => {
            doc.line(Line::from(span(why.clone(), theme::dim())));
        }
        Some(Load::Ready(history)) => {
            for line in chart(history, width) {
                doc.line(line);
            }
        }
    }
    doc
}

fn period(hours: u64) -> String {
    if hours.is_multiple_of(24) && hours >= 48 {
        format!("{} days", hours / 24)
    } else {
        format!("{hours} hours")
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

fn span_of(ms: u64) -> String {
    let seconds = ms / 1000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// A bar chart of what was left, a column per step: the same eight-row block drawing `st slo
/// chart` prints. A step with no reading is a dot on the bottom line, never a gap that could be
/// read as zero.
pub fn chart(history: &SloHistory, width: usize) -> Vec<Line<'static>> {
    if history.samples == 0 {
        return vec![Line::from(span(
            "No readings in this period. st records one a minute from when it starts, and keeps seven days.",
            theme::dim(),
        ))];
    }
    // The axis label and rule take a few columns; a narrow pane shows the latest steps.
    let room = width.saturating_sub(7).max(1);
    let steps = &history.steps[history.steps.len().saturating_sub(room)..];
    let mut lines = Vec::new();
    for line in 0..HEIGHT {
        let label = match line {
            0 => "100%",
            l if l == HEIGHT / 2 => " 50%",
            l if l == HEIGHT - 1 => "  0%",
            _ => "    ",
        };
        let cells = steps
            .iter()
            .map(|step| match step {
                Some(percent) => {
                    let eighths = (percent.clamp(0.0, 100.0) / 100.0 * (HEIGHT * 8) as f64).round()
                        as usize;
                    // Even a spent budget shows its reading as the lowest sliver, never a gap.
                    let eighths = if line == HEIGHT - 1 { eighths.max(1) } else { eighths };
                    LEVELS[eighths.saturating_sub((HEIGHT - 1 - line) * 8).min(8)]
                }
                None if line == HEIGHT - 1 => '·',
                None => ' ',
            })
            .collect::<String>();
        let fill = steps
            .iter()
            .flatten()
            .next_back()
            .map_or(theme::OVERLAY1, |latest| level_color(*latest));
        lines.push(Line::from(vec![
            span(format!("{label} │"), theme::dim()),
            span(cells, theme::fg(fill)),
        ]));
    }
    lines.push(Line::from(span(
        format!("     └{}", "─".repeat(steps.len())),
        theme::dim(),
    )));
    let time = |at: u64| {
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(at as i64)
            .map_or_else(|| "?".to_owned(), |at| at.format("%m-%d %H:%M").to_string())
    };
    let (from, to) = (time(history.since_ms), time(history.until_ms));
    let gap = (steps.len() + 1)
        .saturating_sub(from.chars().count() + to.chars().count() + 4)
        .max(1);
    lines.push(Line::from(span(
        format!("      {from} UTC{}{to}", " ".repeat(gap)),
        theme::dim(),
    )));
    let low = steps.iter().flatten().copied().fold(f64::INFINITY, f64::min);
    let latest = steps.iter().rev().flatten().next().copied().unwrap_or_default();
    lines.push(Line::from(span(
        format!(
            "low {low:.0}% · latest {latest:.0}% · {} readings · a column is its lowest reading",
            history.samples
        ),
        theme::dim(),
    )));
    lines
}

/// The chart's color follows what is left now, so a nearly spent budget draws red.
fn level_color(percent: f64) -> ratatui::style::Color {
    if percent <= NEARLY_SPENT {
        theme::RED
    } else if percent <= 25.0 {
        theme::YELLOW
    } else {
        theme::GREEN
    }
}

/// A daemon that predates the reads answers their paths with a bare 404.
pub fn error(error: &st3_client::ClientError) -> String {
    match error {
        st3_client::ClientError::Api(st3_client::ErrorCode::NotFound, _, _) => {
            "This st does not serve objectives and budgets yet: its daemon needs an update.".into()
        }
        st3_client::ClientError::Protocol(message) if message.starts_with("HTTP 404") => {
            "This st does not serve objectives and budgets yet: its daemon needs an update.".into()
        }
        error => format!("Could not read objectives and budgets: {}", error.plain()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, status: &str, left: Option<f64>) -> SloRow {
        SloRow {
            id: "example".into(),
            kind: kind.into(),
            account: None,
            about: "about".into(),
            target: "target".into(),
            current: "current".into(),
            window: "1h".into(),
            remaining_percent: left,
            status: status.into(),
            resets_at_unix_ms: None,
            measured_at_unix_ms: None,
            explain: "explain".into(),
        }
    }

    fn history(samples: u64, steps: Vec<Option<f64>>) -> SloHistory {
        SloHistory { id: "example".into(), since_ms: 0, until_ms: 3_600_000, samples, steps }
    }

    #[test]
    fn a_missed_objective_or_a_nearly_spent_budget_is_red() {
        assert!(is_red(&row("objective", "missed", Some(0.0))));
        assert!(!is_red(&row("objective", "met", Some(5.0))), "an objective's budget alone is not red");
        assert!(is_red(&row("budget", "met", Some(NEARLY_SPENT))));
        assert!(!is_red(&row("budget", "met", Some(40.0))));
        assert!(!is_red(&row("budget", "no-data", None)), "no reading is not a spent budget");
    }

    #[test]
    fn ids_name_the_row_and_round_trip() {
        let row = row("budget", "met", Some(50.0));
        assert_eq!(id_of(&row), "slo/example");
        assert_eq!(slo_id("slo/example"), Some("example"));
        assert_eq!(slo_id("agent/example"), None);
    }

    #[test]
    fn a_chart_draws_a_column_per_step_and_a_gap_as_a_dot() {
        let lines = chart(&history(3, vec![Some(100.0), None, Some(0.0)]), 80)
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), HEIGHT + 3);
        assert!(lines[0].starts_with("100% │█"), "{lines:?}");
        // A spent budget keeps a sliver and a missing reading a dot, on the bottom line.
        assert!(lines[HEIGHT - 1].ends_with("│█·▁"), "{lines:?}");
        assert!(lines.last().unwrap().contains("low 0% · latest 0% · 3 readings"), "{lines:?}");
    }

    #[test]
    fn a_chart_without_readings_says_so_and_a_narrow_pane_keeps_the_latest_steps() {
        let empty = chart(&history(0, vec![None; 10]), 80);
        assert_eq!(empty.len(), 1);
        let steps = (0..40).map(|at| Some(at as f64)).collect::<Vec<_>>();
        let lines = chart(&history(40, steps), 27)
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>();
        assert!(lines.last().unwrap().contains("latest 39%"), "{lines:?}");
        assert_eq!(lines[HEIGHT].chars().count(), 6 + 20, "{lines:?}");
    }
}
