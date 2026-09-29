//! Text that is safe to put in a cell, wrapped the way a reader expects.
//!
//! Every string from the graph passes through `sanitize` before it is measured. A control
//! character or an escape sequence that reaches the terminal moves its cursor, and the frame
//! is then drawn in the wrong cells: the stray digits on the old stui's borders.

use super::theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Remove escape sequences, control characters and zero-width marks; expand tabs.
pub fn sanitize(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\u{1b}' => {
                // CSI: ESC [ params final; OSC: ESC ] ... BEL or ESC \; others: one more char.
                match chars.peek() {
                    Some('[') => {
                        chars.next();
                        for next in chars.by_ref() {
                            if ('@'..='~').contains(&next) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        chars.next();
                        while let Some(next) = chars.next() {
                            if next == '\u{7}' {
                                break;
                            }
                            if next == '\u{1b}' {
                                chars.next();
                                break;
                            }
                        }
                    }
                    Some(_) => {
                        chars.next();
                    }
                    None => {}
                }
            }
            '\n' => output.push('\n'),
            '\t' => output.push_str("    "),
            '\r' => {}
            '\u{200b}'..='\u{200f}' | '\u{2060}' | '\u{feff}' | '\u{fe0e}' | '\u{fe0f}' => {}
            character if character.is_control() => {}
            character => output.push(character),
        }
    }
    output
}

pub fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Cut to `max` columns, ending in `…` when anything was dropped.
pub fn truncate(text: &str, max: usize) -> String {
    if width(text) <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut output = String::new();
    let mut used = 0;
    for character in text.chars() {
        let w = character.width().unwrap_or(0);
        if used + w + 1 > max {
            break;
        }
        used += w;
        output.push(character);
    }
    output.push('…');
    output
}

/// A run of text with one style: the unit the wrapper moves around.
#[derive(Clone, Debug)]
pub struct Run {
    pub text: String,
    pub style: Style,
}

pub fn run(text: impl Into<String>, style: Style) -> Run {
    Run {
        text: text.into(),
        style,
    }
}

/// Word-wrap runs into lines of `width` columns. The first line starts with `first`, later
/// lines with `rest` (a hanging indent). With `fill`, every line is padded to full width
/// in that background, so a tinted block has straight edges.
pub fn wrap(
    runs: &[Run],
    width: usize,
    first: &[Run],
    rest: &[Run],
    fill: Option<Color>,
) -> Vec<Line<'static>> {
    let width = width.max(4);
    let mut lines = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    let prefix_width = |prefix: &[Run]| {
        prefix
            .iter()
            .map(|run| self::width(&run.text))
            .sum::<usize>()
    };
    let start = |current: &mut Vec<Span<'static>>, used: &mut usize, prefix: &[Run]| {
        for run in prefix {
            current.push(Span::styled(run.text.clone(), with_bg(run.style, fill)));
        }
        *used = prefix_width(prefix);
    };
    let finish = |current: &mut Vec<Span<'static>>, used: usize, lines: &mut Vec<Line<'static>>| {
        if let Some(bg) = fill
            && used < width
        {
            current.push(Span::styled(
                " ".repeat(width - used),
                Style::default().bg(bg),
            ));
        }
        lines.push(Line::from(std::mem::take(current)));
    };
    start(&mut current, &mut used, first);
    let base = used;
    let mut line_has_words = false;
    for run in runs {
        let style = with_bg(run.style, fill);
        for (index, piece) in run.text.split(' ').enumerate() {
            if index > 0 {
                // A space: keep it unless it would start a line.
                if line_has_words && used < width {
                    push(&mut current, " ", style);
                    used += 1;
                }
            }
            if piece.is_empty() {
                continue;
            }
            let piece_width = self::width(piece);
            if used + piece_width > width && line_has_words {
                trim_trailing_space(&mut current, &mut used);
                finish(&mut current, used, &mut lines);
                start(&mut current, &mut used, rest);
            }
            if used + piece_width <= width {
                push(&mut current, piece, style);
                used += piece_width;
            } else {
                // A word longer than the line: break it by character.
                for character in piece.chars() {
                    let w = character.width().unwrap_or(0);
                    if used + w > width {
                        finish(&mut current, used, &mut lines);
                        start(&mut current, &mut used, rest);
                    }
                    push(&mut current, &character.to_string(), style);
                    used += w;
                }
            }
            line_has_words = true;
        }
    }
    if line_has_words || used > base || lines.is_empty() {
        finish(&mut current, used, &mut lines);
    }
    lines
}

fn trim_trailing_space(current: &mut [Span<'static>], used: &mut usize) {
    if let Some(last) = current.last_mut()
        && last.content.ends_with(' ')
    {
        let trimmed = last.content.trim_end_matches(' ').to_owned();
        *used -= last.content.len() - trimmed.len();
        last.content = trimmed.into();
    }
}

fn push(current: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if let Some(last) = current.last_mut()
        && last.style == style
    {
        let mut content = last.content.to_string();
        content.push_str(text);
        last.content = content.into();
        return;
    }
    current.push(Span::styled(text.to_owned(), style));
}

fn with_bg(style: Style, fill: Option<Color>) -> Style {
    match fill {
        Some(bg) if style.bg.is_none() => style.bg(bg),
        _ => style,
    }
}

/// Inline markdown: **bold**, `code`, [text](url).
pub fn inline(text: &str, base: Style) -> Vec<Run> {
    let mut runs = Vec::new();
    let mut plain = String::new();
    let mut rest = text;
    let flush = |plain: &mut String, runs: &mut Vec<Run>| {
        if !plain.is_empty() {
            runs.push(run(std::mem::take(plain), base));
        }
    };
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("**")
            && let Some(end) = after.find("**")
        {
            flush(&mut plain, &mut runs);
            runs.push(run(&after[..end], base.add_modifier(Modifier::BOLD)));
            rest = &after[end + 2..];
        } else if let Some(after) = rest.strip_prefix('`')
            && let Some(end) = after.find('`')
        {
            flush(&mut plain, &mut runs);
            runs.push(run(&after[..end], base.fg(theme::TEAL)));
            rest = &after[end + 1..];
        } else if let Some(after) = rest.strip_prefix('[')
            && let Some(close) = after.find("](")
            && let Some(end) = after[close + 2..].find(')')
        {
            flush(&mut plain, &mut runs);
            runs.push(run(
                &after[..close],
                base.fg(theme::BLUE).add_modifier(Modifier::UNDERLINED),
            ));
            rest = &after[close + 2 + end + 1..];
        } else {
            let character = rest.chars().next().unwrap_or(' ');
            plain.push(character);
            rest = &rest[character.len_utf8()..];
        }
    }
    flush(&mut plain, &mut runs);
    runs
}

/// Block markdown, rendered the way pi renders an assistant reply: real headings, bullets
/// with a hanging indent, code set off in a gutter, quotes in a bar, and simple tables.
pub fn markdown(input: &str, width: usize, base: Style) -> Vec<Line<'static>> {
    let input = sanitize(input);
    let source = input.lines().collect::<Vec<_>>();
    let mut lines = Vec::new();
    let mut index = 0;
    while index < source.len() {
        let line = source[index];
        let trimmed = line.trim_start();
        if let Some(language) = trimmed.strip_prefix("```") {
            lines.push(Line::from(Span::styled(
                format!("```{}", language.trim()),
                theme::dim(),
            )));
            index += 1;
            while index < source.len() && !source[index].trim_start().starts_with("```") {
                let code = source[index];
                lines.extend(wrap(
                    &[run(code, Style::default().fg(theme::SUBTEXT1))],
                    width,
                    &[run("  ", base)],
                    &[run("  ", base)],
                    None,
                ));
                index += 1;
            }
            lines.push(Line::from(Span::styled("```", theme::dim())));
            index += 1;
            continue;
        }
        if trimmed.starts_with('|') {
            let mut rows = Vec::new();
            while index < source.len() && source[index].trim_start().starts_with('|') {
                rows.push(source[index].trim());
                index += 1;
            }
            lines.extend(table(&rows, width, base));
            continue;
        }
        if trimmed.is_empty() {
            lines.push(Line::default());
        } else if let Some(heading) = heading(trimmed) {
            let (level, text) = heading;
            let mut style = Style::default()
                .fg(theme::PEACH)
                .add_modifier(Modifier::BOLD);
            if level == 1 {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            lines.extend(wrap(&inline(text, style), width, &[], &[], None));
        } else if matches!(trimmed, "---" | "***" | "___") {
            lines.push(Line::from(Span::styled(
                "─".repeat(width.min(80)),
                theme::fg(theme::SURFACE2),
            )));
        } else if let Some(quote) = trimmed.strip_prefix("> ").or(trimmed.strip_prefix('>')) {
            let style = base.fg(theme::SUBTEXT0).add_modifier(Modifier::ITALIC);
            let bar = [run("│ ", theme::fg(theme::SURFACE2))];
            lines.extend(wrap(&inline(quote, style), width, &bar, &bar, None));
        } else if let Some((marker, item)) = list_item(trimmed) {
            let indent = " ".repeat(line.len() - trimmed.len());
            let first = [
                run(indent.clone(), base),
                run(marker.clone(), theme::fg(theme::LAVENDER)),
            ];
            let rest = [run(indent + &" ".repeat(width_of(&marker)), base)];
            lines.extend(wrap(&inline(item, base), width, &first, &rest, None));
        } else {
            lines.extend(wrap(&inline(trimmed, base), width, &[], &[], None));
        }
        index += 1;
    }
    while lines.last().is_some_and(|line| line.width() == 0) {
        lines.pop();
    }
    lines
}

fn width_of(text: &str) -> usize {
    width(text)
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|c| *c == '#').count();
    (level > 0 && level <= 6 && line[level..].starts_with(' '))
        .then(|| (level, line[level..].trim()))
}

fn list_item(line: &str) -> Option<(String, &str)> {
    if let Some(item) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
        return Some(("• ".into(), item));
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && digits < 4 && line[digits..].starts_with(". ") {
        return Some((format!("{} ", &line[..digits + 1]), &line[digits + 2..]));
    }
    None
}

fn table(rows: &[&str], width: usize, base: Style) -> Vec<Line<'static>> {
    let cells = rows
        .iter()
        .filter(|row| !row.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')))
        .map(|row| {
            row.trim_matches('|')
                .split('|')
                .map(|cell| cell.trim().replace("**", "").replace('`', ""))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let columns = cells.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0; columns];
    for row in &cells {
        for (column, cell) in row.iter().enumerate() {
            widths[column] = widths[column].max(self::width(cell));
        }
    }
    let total = widths.iter().sum::<usize>() + columns.saturating_sub(1) * 3;
    if total > width {
        // Too wide to draw as a grid: fall back to "key: value" lines.
        return cells
            .iter()
            .flat_map(|row| wrap(&[run(row.join(" · "), base)], width, &[], &[], None))
            .collect();
    }
    let mut lines = Vec::new();
    for (index, row) in cells.iter().enumerate() {
        let mut spans = Vec::new();
        for (column, w) in widths.iter().enumerate() {
            if column > 0 {
                spans.push(Span::styled(" │ ", theme::fg(theme::SURFACE2)));
            }
            let cell = row.get(column).map(String::as_str).unwrap_or("");
            let style = if index == 0 {
                base.add_modifier(Modifier::BOLD)
            } else {
                base
            };
            spans.push(Span::styled(format!("{cell:<w$}", w = *w), style));
        }
        lines.push(Line::from(spans));
        if index == 0 {
            let rule = widths
                .iter()
                .map(|w| "─".repeat(*w))
                .collect::<Vec<_>>()
                .join("─┼─");
            lines.push(Line::from(Span::styled(rule, theme::fg(theme::SURFACE2))));
        }
    }
    lines
}

/// Plain text of a line, for copying.
pub fn plain(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_drops_escapes_and_zero_width_marks() {
        assert_eq!(
            sanitize("a\u{1b}[38;5;97mb\u{1b}[0m\tc\u{200b}d⚠\u{fe0f}"),
            "ab    cd⚠"
        );
        assert_eq!(sanitize("x\u{1b}]8;;http://a\u{7}y"), "xy");
    }

    #[test]
    fn wrap_uses_a_hanging_indent_and_never_exceeds_width() {
        let lines = wrap(
            &[run("one two three four five six", Style::default())],
            12,
            &[run("• ", Style::default())],
            &[run("  ", Style::default())],
            None,
        );
        let text = lines.iter().map(plain).collect::<Vec<_>>();
        assert_eq!(text, vec!["• one two", "  three four", "  five six"]);
        assert!(lines.iter().all(|line| line.width() <= 12));
    }

    #[test]
    fn filled_lines_are_padded_to_full_width() {
        let lines = wrap(
            &[run("hi", Style::default())],
            10,
            &[],
            &[],
            Some(Color::Red),
        );
        assert_eq!(lines[0].width(), 10);
    }

    #[test]
    fn markdown_renders_lists_headings_and_tables() {
        let lines = markdown(
            "## Head\n\n- **a** b\n\n| k | v |\n|---|---|\n| x | y |",
            40,
            Style::default(),
        );
        let text = lines.iter().map(plain).collect::<Vec<_>>();
        assert_eq!(text[0], "Head");
        assert_eq!(text[2], "• a b");
        assert!(text.iter().any(|line| line.starts_with("k │ v")));
    }
}
