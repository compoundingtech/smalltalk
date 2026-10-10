//! The terminal drawing stui and Fractal share. Each passes its own palette, so the cards read
//! the same in both while each keeps its theme.

use crate::{Card, Tone};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub name: Style,
    pub label: Style,
    pub separator: Style,
    pub normal: Style,
    pub busy: Style,
    pub stale: Style,
    pub unknown: Style,
}

impl Palette {
    pub fn tone(&self, tone: Tone) -> Style {
        match tone {
            Tone::Normal => self.normal,
            Tone::Busy => self.busy,
            Tone::Stale => self.stale,
            Tone::Unknown => self.unknown,
        }
    }
}

/// `agents 5 running · load 1m 0.42 / 8 cpu`: one subject's cards, in order.
pub fn spans(cards: &[Card], palette: &Palette) -> Vec<Span<'static>> {
    let mut spans = Vec::with_capacity(cards.len() * 3);
    for (index, card) in cards.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", palette.separator));
        }
        spans.push(Span::styled(format!("{} ", card.label), palette.label));
        spans.push(Span::styled(card.value.clone(), palette.tone(card.tone)));
    }
    spans
}

/// One line per subject, named by its machine: `dev3  agents 5 running · load 1m …`.
pub fn lines(cards: &[Card], palette: &Palette) -> Vec<Line<'static>> {
    cards
        .chunk_by(|left, right| left.subject == right.subject)
        .map(|group| {
            let subject = &group[0].subject;
            let name = subject.strip_prefix("machine/").unwrap_or(subject);
            let mut line = vec![Span::styled(format!("{name}  "), palette.name)];
            line.extend(spans(group, palette));
            Line::from(line)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CardKind;
    use ratatui::style::{Color, Modifier};

    const PALETTE: Palette = Palette {
        name: Style::new().add_modifier(Modifier::BOLD),
        label: Style::new().fg(Color::DarkGray),
        separator: Style::new().fg(Color::DarkGray),
        normal: Style::new(),
        busy: Style::new().fg(Color::Yellow),
        stale: Style::new().fg(Color::DarkGray),
        unknown: Style::new().add_modifier(Modifier::ITALIC),
    };

    fn card(subject: &str, kind: CardKind, label: &str, value: &str, tone: Tone) -> Card {
        Card {
            id: format!("{subject}#{label}"),
            kind,
            subject: subject.into(),
            label: label.into(),
            value: value.into(),
            tone,
        }
    }

    #[test]
    fn one_line_per_machine_in_card_order() {
        let cards = [
            card("machine/dev3", CardKind::AgentsRunning, "agents", "5 running", Tone::Normal),
            card("machine/dev3", CardKind::HostLoad, "load 1m", "9.10 / 8 cpu", Tone::Busy),
            card("machine/mbp", CardKind::AgentsRunning, "agents", "0 running", Tone::Stale),
        ];
        let lines = lines(&cards, &PALETTE);
        let text: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();
        assert_eq!(
            text,
            ["dev3  agents 5 running · load 1m 9.10 / 8 cpu", "mbp  agents 0 running"]
        );
        assert_eq!(lines[0].spans.last().unwrap().style, PALETTE.busy);
    }
}
