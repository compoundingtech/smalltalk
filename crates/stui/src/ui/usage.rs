//! The Usage tab: token spend and its API-equivalent cost over a period, grouped by agent,
//! mission, step, model, account or host, from st's usage read (`usage.period`).
//!
//! Each list row's id is the subject it groups (`agent/…`, `mission/…`, `step-run/…`,
//! `machine/…`) or, for what st has no subject for, `model/…`, `account/…` and
//! `usage/no-…`, so a usage pane can be opened by its key alone and say what it shows.

use super::doc::{Doc, DocExt};
use super::screens::{Item, ListState, Listing};
use super::text;
use super::theme;
use super::view::{Load, World};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use st3_client::{UsageLimit, UsageRow};
use std::collections::BTreeMap;

/// What the Usage list groups by; `b` steps through them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum By {
    #[default]
    Agent,
    Mission,
    Step,
    Model,
    Account,
    Host,
}

impl By {
    const ALL: [By; 6] = [
        By::Agent,
        By::Mission,
        By::Step,
        By::Model,
        By::Account,
        By::Host,
    ];

    pub fn name(self) -> &'static str {
        match self {
            By::Agent => "agent",
            By::Mission => "mission",
            By::Step => "step",
            By::Model => "model",
            By::Account => "account",
            By::Host => "host",
        }
    }

    pub fn next(self) -> By {
        let at = By::ALL.iter().position(|by| *by == self).unwrap_or(0);
        By::ALL[(at + 1) % By::ALL.len()]
    }

    /// The grouping a row id belongs to, from its prefix.
    fn of(id: &str) -> Option<By> {
        let kind = id.split('/').next()?;
        Some(match kind {
            "agent" => By::Agent,
            "mission" => By::Mission,
            "step-run" => By::Step,
            "model" => By::Model,
            "account" => By::Account,
            "machine" => By::Host,
            "usage" => By::ALL
                .into_iter()
                .find(|by| id == format!("usage/no-{}", by.name()))?,
            _ => return None,
        })
    }
}

/// The periods `p` steps through, in hours: a day, a week and thirty days.
pub const PERIODS: [u64; 3] = [24, 168, 720];

pub fn next_period(hours: u64) -> u64 {
    let at = PERIODS.iter().position(|period| *period == hours);
    PERIODS[at.map_or(0, |at| (at + 1) % PERIODS.len())]
}

pub fn period_name(hours: u64) -> String {
    match hours {
        24 => "the last 24 hours".into(),
        hours if hours % 24 == 0 => format!("the last {} days", hours / 24),
        hours => format!("the last {hours} hours"),
    }
}

/// A group's spend.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Total {
    pub tokens: u64,
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cached: u64,
    pub cost_microusd: u64,
    pub unpriced: u64,
}

impl Total {
    fn add(&mut self, row: &UsageRow) {
        self.tokens += row.total_tokens;
        self.input += row.input_tokens;
        self.output += row.output_tokens;
        self.cache_write += row.cache_write_tokens;
        self.cached += row.cached_tokens;
        self.cost_microusd += row.cost_microusd;
        self.unpriced += row.unpriced_tokens;
    }

    /// The cost, with a `+` when some tokens had no price: the true cost is higher, never
    /// lower.
    pub fn money(&self) -> String {
        let plus = if self.unpriced > 0 { "+" } else { "" };
        format!("{}{plus}", money(self.cost_microusd))
    }
}

pub fn money(microusd: u64) -> String {
    let dollars = microusd as f64 / 1_000_000.0;
    if microusd == 0 {
        "$0".into()
    } else if dollars < 0.01 {
        "<$0.01".into()
    } else if dollars < 100.0 {
        format!("${dollars:.2}")
    } else {
        format!("${dollars:.0}")
    }
}

pub fn tokens(count: u64) -> String {
    let count = count as f64;
    for (size, unit) in [(1e9, "B"), (1e6, "M"), (1e3, "k")] {
        if count >= size {
            let value = count / size;
            return if value < 10.0 {
                format!("{value:.1}{unit}")
            } else {
                format!("{value:.0}{unit}")
            };
        }
    }
    format!("{count}")
}

/// The id a row is grouped under.
pub fn key(row: &UsageRow, by: By) -> String {
    let none = || format!("usage/no-{}", by.name());
    fn known(value: &Option<String>) -> Option<&str> {
        value.as_deref().filter(|value| !value.is_empty())
    }
    match by {
        By::Agent => row.agent.clone(),
        By::Mission => known(&row.mission_run).map_or_else(none, mission_of),
        By::Step => known(&row.step).map_or_else(none, str::to_owned),
        By::Model => known(&row.model).map_or_else(none, |model| format!("model/{model}")),
        By::Account => {
            known(&row.account).map_or_else(none, |account| format!("account/{account}"))
        }
        By::Host => known(&row.host).map_or_else(none, |host| format!("machine/{host}")),
    }
}

/// `mission-run/NAME/RUN` names the mission `mission/NAME`.
fn mission_of(run: &str) -> String {
    let name = run.strip_prefix("mission-run/").unwrap_or(run);
    let name = name.rsplit_once('/').map_or(name, |(name, _)| name);
    format!("mission/{name}")
}

/// Each group's spend, the largest first.
pub fn groups<'a>(rows: impl Iterator<Item = &'a UsageRow>, by: By) -> Vec<(String, Total)> {
    let mut totals = BTreeMap::<String, Total>::new();
    for row in rows {
        totals.entry(key(row, by)).or_default().add(row);
    }
    let mut groups = totals.into_iter().collect::<Vec<_>>();
    groups.sort_by(|(a_id, a), (b_id, b)| {
        (b.cost_microusd, b.tokens, a_id).cmp(&(a.cost_microusd, a.tokens, b_id))
    });
    groups
}

/// A group as a person reads it: an agent's or mission's name, a step with its mission.
pub fn label(world: &World, id: &str) -> String {
    if let Some(account) = id.strip_prefix("account/") {
        // `claude/<digest>`: the provider, and enough of the digest to tell two apart.
        return match account.split_once('/') {
            Some((driver, "unknown")) => format!("{driver} · account not named"),
            Some((driver, digest)) => format!("{driver} · {}", &digest[..digest.len().min(8)]),
            None => account.to_owned(),
        };
    }
    if let Some(rest) = id.strip_prefix("usage/no-") {
        return match rest {
            "account" => "no account recorded (before st kept accounts)".into(),
            "mission" => "no mission (standing seats)".into(),
            "step" => "no step (standing seats)".into(),
            other => format!("unknown {other}"),
        };
    }
    if id.starts_with("agent/") {
        if let Some(agent) = world.agents.items().iter().find(|agent| agent.id == id) {
            return agent.name.clone();
        }
        return id.trim_start_matches("agent/").to_owned();
    }
    if id.starts_with("mission/") {
        if let Some(mission) = world
            .missions
            .items()
            .iter()
            .find(|mission| mission.id == id)
        {
            return mission.title.clone();
        }
        return id.trim_start_matches("mission/").to_owned();
    }
    if id.starts_with("step-run/") {
        let step = id.rsplit('/').next().unwrap_or(id);
        let mission = world.usage.items().iter().find_map(|row| {
            (row.step.as_deref() == Some(id))
                .then(|| row.mission_run.as_deref().map(mission_of))
                .flatten()
        });
        return match mission {
            Some(mission) => format!("{} · {step}", label(world, &mission)),
            None => step.to_owned(),
        };
    }
    id.split_once('/').map_or(id, |(_, rest)| rest).to_owned()
}

fn span(text: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(text.into(), style)
}

/// The Usage tab's list: each account's spend and limits, a short summary, then every group
/// the list is grouped by, the largest first.
pub fn list(world: &World, by: By, hours: u64) -> Listing {
    let rows = world.usage.items();
    let mut items = Vec::new();
    let mut ids = Vec::new();
    let mut row = |items: &mut Vec<Item>, id: String, first, right, second| {
        items.push(Item::Row {
            index: ids.len(),
            first,
            right,
            second,
            children: Vec::new(),
        });
        ids.push(id);
    };
    if !rows.is_empty() || !world.usage_limits.is_empty() {
        let accounts = accounts(world);
        items.push(Item::Header {
            title: "accounts".into(),
            count: accounts.len(),
            color: theme::ACCENT,
        });
        for (id, spent, limit) in accounts {
            row(
                &mut items,
                id.clone(),
                vec![span(format!(" {}", label(world, &id)), theme::bold())],
                vec![span(spent.money(), theme::fg(theme::ACCENT))],
                limit_line(limit, &spent),
            );
        }
        let mut total = Total::default();
        rows.iter().for_each(|row| total.add(row));
        items.push(Item::Header {
            title: "summary".into(),
            count: rows.len(),
            color: theme::OVERLAY1,
        });
        items.push(Item::Note(Line::from(vec![
            span(" ", theme::dim()),
            span(total.money(), theme::bold()),
            span(
                format!(
                    " · {} tokens · {}",
                    tokens(total.tokens),
                    period_name(hours)
                ),
                theme::dim(),
            ),
        ])));
        for (name, grouping) in [("agent", By::Agent), ("mission", By::Mission)] {
            if let Some((id, spent)) = groups(rows.iter(), grouping)
                .into_iter()
                .find(|(id, _)| !id.starts_with("usage/"))
            {
                items.push(Item::Note(Line::from(vec![
                    span(format!(" most by {name:<8}"), theme::dim()),
                    span(label(world, &id), theme::text()),
                    span(format!("  {}", spent.money()), theme::fg(theme::ACCENT)),
                ])));
            }
        }
        if total.unpriced > 0 {
            items.push(Item::Note(Line::from(span(
                format!(
                    " {} tokens had no price, so the cost is at least this",
                    tokens(total.unpriced)
                ),
                theme::fg(theme::YELLOW),
            ))));
        }
    }
    let grouped = groups(rows.iter(), by);
    if !grouped.is_empty() {
        items.push(Item::Header {
            title: format!("by {}", by.name()),
            count: grouped.len(),
            color: theme::OVERLAY1,
        });
    }
    for (id, total) in grouped {
        let cached = if total.tokens > 0 {
            total.cached * 100 / total.tokens
        } else {
            0
        };
        row(
            &mut items,
            id.clone(),
            vec![span(format!(" {}", label(world, &id)), theme::bold())],
            vec![span(total.money(), theme::fg(theme::ACCENT))],
            vec![span(
                format!(
                    "   {} tokens · {} out · {cached}% cached",
                    tokens(total.tokens),
                    tokens(total.output)
                ),
                theme::dim(),
            )],
        );
    }
    let state = match &world.usage {
        Load::Loading => ListState::Loading("Loading usage…"),
        Load::Failed(why) => ListState::Failed(why.clone()),
        Load::Ready(rows) if rows.is_empty() && world.usage_limits.is_empty() => {
            ListState::Empty("No spend recorded in this period.")
        }
        Load::Ready(_) => ListState::Ready,
    };
    Listing {
        items,
        ids,
        state,
        legend: vec![
            Line::from(vec![
                span("b ", theme::fg(theme::ACCENT)),
                span(format!("by {:<9}", by.name()), theme::dim()),
                span("p ", theme::fg(theme::ACCENT)),
                span(period_name(hours), theme::dim()),
            ]),
            Line::from(span("Costs are API-equivalent list prices.", theme::dim())),
        ],
    }
}

/// Every account that spent in the period or reported its limits, the largest spender first,
/// with spend st could not tie to an account last.
fn accounts(world: &World) -> Vec<(String, Total, Option<&UsageLimit>)> {
    let mut accounts = groups(world.usage.items().iter(), By::Account)
        .into_iter()
        .map(|(id, spent)| {
            let limit = world
                .usage_limits
                .iter()
                .find(|limit| id == format!("account/{}", limit.account));
            (id, spent, limit)
        })
        .collect::<Vec<_>>();
    for limit in &world.usage_limits {
        let id = format!("account/{}", limit.account);
        if !accounts.iter().any(|(known, _, _)| *known == id) {
            accounts.push((id, Total::default(), Some(limit)));
        }
    }
    accounts.sort_by_key(|(id, _, _)| id.starts_with("usage/"));
    accounts
}

/// An account's limits as one line: weekly first, since that is the one that stops seats.
fn limit_line(limit: Option<&UsageLimit>, spent: &Total) -> Vec<Span<'static>> {
    let Some(limit) = limit else {
        return vec![span(
            format!("   {} tokens · no limits reported", tokens(spent.tokens)),
            theme::dim(),
        )];
    };
    let now = now_ms();
    let share = |percent: Option<f64>| match percent {
        Some(percent) => (
            format!("{percent:.0}%"),
            if percent >= 90.0 {
                theme::RED
            } else if percent >= 75.0 {
                theme::YELLOW
            } else {
                theme::GREEN
            },
        ),
        None => ("?".into(), theme::OVERLAY1),
    };
    let (weekly, weekly_color) = share(limit.weekly_percent);
    let (five, five_color) = share(limit.five_hour_percent);
    let resets = limit
        .weekly_resets_at_unix_ms
        .map(|at| format!(" resets in {}", span_of(at.saturating_sub(now))))
        .unwrap_or_default();
    vec![
        span("   weekly ", theme::dim()),
        span(weekly, theme::strong(weekly_color)),
        span(format!("{resets} · 5-hour "), theme::dim()),
        span(five, theme::strong(five_color)),
        span(
            format!(
                " · measured {} ago",
                span_of(now.saturating_sub(limit.measured_at_unix_ms))
            ),
            theme::dim(),
        ),
    ]
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// A length of time as a person says it: `40s`, `12m`, `5h`, `3d`.
fn span_of(ms: u64) -> String {
    let seconds = ms / 1000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// One group's spend in detail (the whole period's with no id): its total, how its tokens
/// split, and where they went by step, agent, model and host.
pub fn detail(world: &World, id: Option<&str>, hours: u64, width: usize) -> Doc {
    let mut doc = Doc::new();
    let rows = match &world.usage {
        Load::Ready(rows) => rows,
        Load::Loading => {
            doc.line(Line::from(span("Loading usage…", theme::dim())));
            return doc;
        }
        Load::Failed(why) => {
            doc.line(Line::from(span(why.clone(), theme::dim())));
            return doc;
        }
    };
    let own = id.and_then(By::of);
    let mine = rows
        .iter()
        .filter(|row| match (id, own) {
            (Some(id), Some(by)) => key(row, by) == id,
            _ => true,
        })
        .collect::<Vec<_>>();
    let title = match id {
        Some(id) => label(world, id),
        None => "All spend".into(),
    };
    doc.line(Line::from(span(title, theme::bold())));
    if mine.is_empty() {
        doc.line(Line::from(span(
            format!("No spend recorded in {}.", period_name(hours)),
            theme::dim(),
        )));
        return doc;
    }
    let mut total = Total::default();
    mine.iter().for_each(|row| total.add(row));
    doc.line(Line::from(vec![
        span(total.money(), theme::strong(theme::ACCENT)),
        span(
            format!(
                " API-equivalent · {} tokens · {}",
                tokens(total.tokens),
                period_name(hours)
            ),
            theme::dim(),
        ),
    ]));
    if total.unpriced > 0 {
        doc.line(Line::from(span(
            format!(
                "{} tokens had no price, so the true cost is higher.",
                tokens(total.unpriced)
            ),
            theme::fg(theme::YELLOW),
        )));
    }
    doc.blank();
    for (name, count) in [
        ("input", total.input),
        ("output", total.output),
        ("to cache", total.cache_write),
        ("cached", total.cached),
    ] {
        doc.field(name, &tokens(count), width, theme::text());
    }
    for by in [By::Step, By::Agent, By::Model, By::Host] {
        if Some(by) == own {
            continue;
        }
        let groups = groups(mine.iter().copied(), by);
        // One group says nothing the total did not.
        if groups.len() < 2 && by != By::Step {
            continue;
        }
        let mut card = Doc::new();
        for (group, spent) in groups.iter().take(8) {
            // Inside a mission its steps need no mission name.
            let name = match (own, by) {
                (Some(By::Mission), By::Step) => {
                    group.rsplit('/').next().unwrap_or(group).to_owned()
                }
                _ => label(world, group),
            };
            let name = text::truncate(&name, width.saturating_sub(22).max(8));
            card.line(Line::from(vec![
                span(
                    format!("{name:<w$}", w = width.saturating_sub(22).max(8)),
                    theme::text(),
                ),
                span(format!("{:>9}", spent.money()), theme::fg(theme::ACCENT)),
                span(format!("{:>8}", tokens(spent.tokens)), theme::dim()),
            ]));
        }
        if groups.len() > 8 {
            card.line(Line::from(span(
                format!("and {} more", groups.len() - 8),
                theme::dim(),
            )));
        }
        doc.card(
            &format!("by {}", by.name()),
            theme::OVERLAY1,
            false,
            card,
            width,
        );
    }
    // An account's limits, or every account's on the whole period.
    let limits = world
        .usage_limits
        .iter()
        .filter(|limit| id.is_none_or(|id| id == format!("account/{}", limit.account)))
        .collect::<Vec<_>>();
    if !limits.is_empty() {
        let mut card = Doc::new();
        for limit in limits {
            let spent = mine
                .iter()
                .filter(|row| row.account.as_deref() == Some(limit.account.as_str()))
                .fold(Total::default(), |mut total, row| {
                    total.add(row);
                    total
                });
            card.line(Line::from(span(
                label(world, &format!("account/{}", limit.account)),
                theme::bold(),
            )));
            card.line(Line::from(limit_line(Some(limit), &spent)));
            card.line(Line::from(span(
                format!(
                    "   {} seat{} · measured by {} on {}",
                    limit.seats.len(),
                    if limit.seats.len() == 1 { "" } else { "s" },
                    label(world, &limit.measured_by),
                    limit.host
                ),
                theme::dim(),
            )));
        }
        doc.card("limits", theme::OVERLAY1, false, card, width);
    } else if id.is_none() || own == Some(By::Account) {
        doc.line(Line::from(span(
            "No account has reported its limits.",
            theme::dim(),
        )));
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_rank_by_cost_and_name_what_st_does_not_know() {
        let world = super::super::demo::world();
        let rows = world.usage.items();
        let by_agent = groups(rows.iter(), By::Agent);
        assert_eq!(by_agent[0].0, "agent/example/atlas/builder");
        assert_eq!(by_agent[0].1.cost_microusd, 48_900_000);
        let by_mission = groups(rows.iter(), By::Mission);
        assert_eq!(by_mission[0].0, "mission/fleet/atlas/store-move");
        assert!(by_mission.iter().any(|(id, _)| id == "usage/no-mission"));
        assert_eq!(
            label(&world, "usage/no-mission"),
            "no mission (standing seats)"
        );
        assert_eq!(By::of("usage/no-mission"), Some(By::Mission));
        assert_eq!(By::of("step-run/atlas-1/compare"), Some(By::Step));
        assert!(label(&world, "step-run/atlas-1/compare").ends_with(" · compare"));
    }

    #[test]
    fn money_and_tokens_read_at_a_glance() {
        assert_eq!(money(0), "$0");
        assert_eq!(money(4_000), "<$0.01");
        assert_eq!(money(4_250_000), "$4.25");
        assert_eq!(money(412_600_000), "$413");
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(1_250_000), "1.2M");
        assert_eq!(tokens(328_915_253), "329M");
        let unpriced = Total {
            cost_microusd: 1_000_000,
            unpriced: 5,
            ..Total::default()
        };
        assert_eq!(unpriced.money(), "$1.00+");
        assert_eq!(next_period(24), 168);
        assert_eq!(next_period(720), 24);
        assert_eq!(period_name(168), "the last 7 days");
    }

    #[test]
    fn a_groups_detail_shows_only_its_rows() {
        let world = super::super::demo::world();
        let plain = |doc: &Doc| {
            doc.lines
                .iter()
                .map(text::plain)
                .collect::<Vec<_>>()
                .join("\n")
        };
        let mission = plain(&detail(
            &world,
            Some("mission/fleet/atlas/store-move"),
            24,
            60,
        ));
        assert!(mission.contains("$58.20 API-equivalent"), "{mission}");
        assert!(mission.contains("compare"), "{mission}");
        assert!(!mission.contains("captain"), "{mission}");
        let all = plain(&detail(&world, None, 24, 60));
        assert!(all.contains("weekly 23%"), "{all}");
        let listed = list(&world, By::Agent, 24)
            .items
            .iter()
            .map(|item| match item {
                Item::Header { title, .. } => format!("# {title}"),
                Item::Row { first, second, .. } => {
                    format!(
                        "{} {}",
                        text::plain(&Line::from(first.clone())),
                        text::plain(&Line::from(second.clone()))
                    )
                }
                Item::Note(line) | Item::Folder(line) => text::plain(line),
            })
            .collect::<Vec<_>>()
            .join("\n");
        // Accounts first, then a summary, then every agent.
        let at = |needle: &str| {
            listed
                .find(needle)
                .unwrap_or_else(|| panic!("{needle}: {listed}"))
        };
        assert!(
            at("# accounts") < at("# summary") && at("# summary") < at("# by agent"),
            "{listed}"
        );
        assert!(listed.contains("weekly 23% resets in"), "{listed}");
        assert!(listed.contains("most by agent   Atlas Builder"), "{listed}");
    }
}
