//! Text that is safe to put in a cell, wrapped the way a reader expects.
//!
//! Every string from the graph passes through `sanitize` before it is measured. A control
//! character or an escape sequence that reaches the terminal moves its cursor, and the frame
//! is then drawn in the wrong cells: the stray digits on the old stui's borders.

use crate::theme::{self, Theme};
use ratatui::layout::Alignment;
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
    let finish = |current: &mut Vec<Span<'static>>,
                  used: usize,
                  lines: &mut Vec<Line<'static>>,
                  joining: Option<Alignment>| {
        if let Some(bg) = fill
            && used < width
        {
            current.push(Span::styled(
                " ".repeat(width - used),
                Style::default().bg(bg),
            ));
        }
        let mut line = Line::from(std::mem::take(current));
        line.alignment = joining;
        lines.push(line);
    };
    // How the line being built continues the one before it (see `continues`).
    let mut joining = None;
    start(&mut current, &mut used, first);
    let mut line_has_words = false;
    for run in runs {
        let style = with_bg(run.style, fill);
        for (line_index, logical_line) in run.text.split('\n').enumerate() {
            if line_index > 0 {
                // A source newline is a real row boundary, including blank paragraphs.
                // Ratatui spans cannot carry it: Buffer::set_line drops control characters.
                finish(&mut current, used, &mut lines, joining);
                joining = None;
                start(&mut current, &mut used, rest);
                line_has_words = false;
            }
            for (index, piece) in logical_line.split(' ').enumerate() {
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
                    finish(&mut current, used, &mut lines, joining);
                    joining = Some(SPACE_WRAP);
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
                            finish(&mut current, used, &mut lines, joining);
                            joining = Some(WORD_WRAP);
                            start(&mut current, &mut used, rest);
                        }
                        push(&mut current, &character.to_string(), style);
                        used += w;
                    }
                }
                line_has_words = true;
            }
        }
    }

    finish(&mut current, used, &mut lines, joining);
    lines
}

/// A line that only wraps the one before it, at a space (the space is dropped) or inside a long
/// word, is marked in its alignment: drawing a line (`Buffer::set_line`) ignores alignment, and
/// copying joins the two back into one line, so only real newlines reach the clipboard
/// (Nathan, 2026-10-03).
const SPACE_WRAP: Alignment = Alignment::Left;
const WORD_WRAP: Alignment = Alignment::Right;

/// What joins `line` to the line before it when copied: a space or nothing if it only wraps that
/// line, `None` if it starts a line of its own.
pub fn continues(line: &Line<'_>) -> Option<&'static str> {
    match line.alignment {
        Some(SPACE_WRAP) => Some(" "),
        Some(WORD_WRAP) => Some(""),
        _ => None,
    }
}

impl crate::SelectionLine for Line<'_> {
    fn plain_text(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Owned(plain(self))
    }

    fn continuation(&self) -> Option<&str> {
        continues(self)
    }
}

/// `line` drawn after `prefix` (an edge, an indent), still wrapping the line before it if it did.
pub fn prefixed(prefix: Vec<Span<'static>>, line: Line<'static>) -> Line<'static> {
    let alignment = line.alignment;
    let mut spans = prefix;
    spans.extend(line.spans);
    let mut line = Line::from(spans);
    line.alignment = alignment;
    line
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

thread_local! {
    /// The address behind each markdown link's text, so a client can open what it drew.
    static LINKS: std::cell::RefCell<std::collections::HashMap<String, String>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// The address of a markdown link drawn with this text, once it has been drawn.
pub fn link_for(text: &str) -> Option<String> {
    LINKS.with(|links| links.borrow().get(text.trim()).cloned())
}

fn remember_link(text: &str, url: &str) {
    LINKS.with(|links| {
        let mut links = links.borrow_mut();
        if links.len() > 2048 {
            links.clear();
        }
        links.insert(text.trim().to_owned(), url.trim().to_owned());
    });
}

/// Inline markdown: **bold**, `code`, [text](url).
pub fn inline(text: &str, base: Style, theme: &Theme) -> Vec<Run> {
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
            runs.push(run(&after[..end], base.fg(theme.teal)));
            rest = &after[end + 1..];
        } else if let Some(after) = rest.strip_prefix('[')
            && let Some(close) = after.find("](")
            && let Some(end) = after[close + 2..].find(')')
        {
            flush(&mut plain, &mut runs);
            remember_link(&after[..close], &after[close + 2..close + 2 + end]);
            runs.push(run(
                &after[..close],
                base.fg(theme.blue).add_modifier(Modifier::UNDERLINED),
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
pub fn markdown(input: &str, width: usize, base: Style, theme: &Theme) -> Vec<Line<'static>> {
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
                theme.dim(),
            )));
            index += 1;
            while index < source.len() && !source[index].trim_start().starts_with("```") {
                let code = source[index];
                lines.extend(wrap(
                    &[run(code, Style::default().fg(theme.subtext1))],
                    width,
                    &[run("  ", base)],
                    &[run("  ", base)],
                    None,
                ));
                index += 1;
            }
            lines.push(Line::from(Span::styled("```", theme.dim())));
            index += 1;
            continue;
        }
        if trimmed.starts_with('|') {
            let mut rows = Vec::new();
            while index < source.len() && source[index].trim_start().starts_with('|') {
                rows.push(source[index].trim());
                index += 1;
            }
            lines.extend(table(&rows, width, base, theme));
            continue;
        }
        if trimmed.is_empty() {
            lines.push(Line::default());
        } else if let Some(heading) = heading(trimmed) {
            let (level, text) = heading;
            let mut style = Style::default()
                .fg(theme.peach)
                .add_modifier(Modifier::BOLD);
            if level == 1 {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            lines.extend(wrap(&inline(text, style, theme), width, &[], &[], None));
        } else if matches!(trimmed, "---" | "***" | "___") {
            lines.push(Line::from(Span::styled(
                "─".repeat(width.min(80)),
                theme::fg(theme.surface2),
            )));
        } else if let Some(quote) = trimmed.strip_prefix("> ").or(trimmed.strip_prefix('>')) {
            let style = base.fg(theme.subtext0).add_modifier(Modifier::ITALIC);
            let bar = [run("│ ", theme::fg(theme.surface2))];
            lines.extend(wrap(&inline(quote, style, theme), width, &bar, &bar, None));
        } else if let Some((marker, item)) = list_item(trimmed) {
            let indent = " ".repeat(line.len() - trimmed.len());
            let first = [
                run(indent.clone(), base),
                run(marker.clone(), theme::fg(theme.lavender)),
            ];
            let rest = [run(indent + &" ".repeat(width_of(&marker)), base)];
            lines.extend(wrap(&inline(item, base, theme), width, &first, &rest, None));
        } else {
            lines.extend(wrap(&inline(trimmed, base, theme), width, &[], &[], None));
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

/// The narrowest a column is squeezed to before the table is shown as stacked records instead.
const MIN_COLUMN: usize = 6;

/// A markdown table. It is a grid when it fits; when a cell is long or the pane is narrow the
/// columns are narrowed and their cells wrap inside them, so it is still a table. Only when even
/// the narrowest columns cannot fit is each row shown as a record, one `heading: value` per cell.
/// Cells are read as inline markdown: bold, code and links keep their look and stay clickable.
fn table(rows: &[&str], width: usize, base: Style, theme: &Theme) -> Vec<Line<'static>> {
    let cells = rows
        .iter()
        .filter(|row| !row.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')))
        .map(|row| {
            row.trim_matches('|')
                .split('|')
                .map(|cell| cell.trim().to_owned())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let columns = cells.iter().map(Vec::len).max().unwrap_or(0);
    let runs = cells
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let style = if index == 0 {
                base.add_modifier(Modifier::BOLD)
            } else {
                base
            };
            (0..columns)
                .map(|column| inline(row.get(column).map(String::as_str).unwrap_or(""), style, theme))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let natural = (0..columns)
        .map(|column| {
            runs.iter()
                .map(|row| row[column].iter().map(|run| width_of(&run.text)).sum::<usize>())
                .max()
                .unwrap_or(0)
        })
        .collect::<Vec<_>>();
    let budget = width.saturating_sub(columns.saturating_sub(1) * 3);
    let Some(widths) = fit_columns(&natural, budget) else {
        return records(&cells, &runs, width, base, theme);
    };
    let mut lines = Vec::new();
    for (index, row) in runs.iter().enumerate() {
        let wrapped = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| {
                if cell.is_empty() {
                    vec![Line::default()]
                } else {
                    wrap(cell, *w, &[], &[], None)
                }
            })
            .collect::<Vec<_>>();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        for line_index in 0..height {
            let mut spans = Vec::new();
            for (column, w) in widths.iter().enumerate() {
                if column > 0 {
                    spans.push(Span::styled(" │ ", theme::fg(theme.surface2)));
                }
                let mut used = 0;
                if let Some(line) = wrapped[column].get(line_index) {
                    used = line.width();
                    spans.extend(line.spans.iter().cloned());
                }
                // The last column's trailing spaces would only be copied; the others hold the grid.
                if column + 1 < widths.len() {
                    spans.push(Span::styled(" ".repeat(w.saturating_sub(used)), base));
                }
            }
            // Nothing after the last column but what is in it: no trailing spaces to copy.
            while let Some(last) = spans.last_mut() {
                let kept = last.content.trim_end().len();
                if kept == 0 {
                    spans.pop();
                } else {
                    last.content = last.content[..kept].to_owned().into();
                    break;
                }
            }
            lines.push(Line::from(spans));
        }
        if index == 0 {
            let rule = widths
                .iter()
                .map(|w| "─".repeat(*w))
                .collect::<Vec<_>>()
                .join("─┼─");
            lines.push(Line::from(Span::styled(rule, theme::fg(theme.surface2))));
        }
    }
    lines
}

/// Column widths that fit `budget`: each column's natural width when they all fit, else the
/// widest columns give up space first, down to `MIN_COLUMN`. `None` when even that is too wide.
fn fit_columns(natural: &[usize], budget: usize) -> Option<Vec<usize>> {
    if natural.iter().sum::<usize>() <= budget {
        return Some(natural.to_vec());
    }
    let mut widths = natural
        .iter()
        .map(|natural| (*natural).min(MIN_COLUMN))
        .collect::<Vec<_>>();
    let mut spare = budget.checked_sub(widths.iter().sum::<usize>())?;
    while spare > 0 {
        // The column that still wants the most gets the next cell of width.
        let Some((column, _)) = natural
            .iter()
            .zip(&widths)
            .enumerate()
            .map(|(column, (natural, width))| (column, natural - width))
            .filter(|(_, wants)| *wants > 0)
            .max_by_key(|(column, wants)| (*wants, std::cmp::Reverse(*column)))
        else {
            break;
        };
        widths[column] += 1;
        spare -= 1;
    }
    Some(widths)
}

/// A table too narrow for any grid: each row as a record, the heading before each value.
fn records(
    cells: &[Vec<String>],
    runs: &[Vec<Vec<Run>>],
    width: usize,
    base: Style,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let Some(headings) = cells.first() else {
        return lines;
    };
    for (row, row_runs) in runs.iter().enumerate().skip(1) {
        if row > 1 {
            lines.push(Line::default());
        }
        for (column, cell) in row_runs.iter().enumerate() {
            let heading = headings.get(column).map(String::as_str).unwrap_or("");
            let first = [run(format!("{heading}: "), theme.dim())];
            lines.extend(wrap(cell, width, &first, &[run("  ", base)], None));
        }
    }
    if runs.len() == 1 {
        lines.extend(wrap(
            &[run(headings.join(" · "), base.add_modifier(Modifier::BOLD))],
            width,
            &[],
            &[],
            None,
        ));
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

/// One styled line as terminal text: 24-bit colors, bold, italic, underline and dim, reset at the
/// end. For a program with no screen of its own, such as `st documents get`.
pub fn ansi(line: &Line<'_>) -> String {
    let mut out = String::new();
    for span in &line.spans {
        let style = line.style.patch(span.style);
        let mut codes: Vec<String> = Vec::new();
        for (add, code) in [
            (Modifier::BOLD, "1"),
            (Modifier::DIM, "2"),
            (Modifier::ITALIC, "3"),
            (Modifier::UNDERLINED, "4"),
            (Modifier::REVERSED, "7"),
            (Modifier::CROSSED_OUT, "9"),
        ] {
            if style.add_modifier.contains(add) {
                codes.push(code.to_owned());
            }
        }
        if let Some(Color::Rgb(r, g, b)) = style.fg {
            codes.push(format!("38;2;{r};{g};{b}"));
        }
        if let Some(Color::Rgb(r, g, b)) = style.bg {
            codes.push(format!("48;2;{r};{g};{b}"));
        }
        if codes.is_empty() {
            out.push_str(&span.content);
        } else {
            out.push_str(&format!("\x1b[{}m{}\x1b[0m", codes.join(";"), span.content));
        }
    }
    out
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
            &crate::tests::theme(),
        );
        let text = lines.iter().map(plain).collect::<Vec<_>>();
        assert_eq!(text[0], "Head");
        assert_eq!(text[2], "• a b");
        assert!(text.iter().any(|line| line.starts_with("k │ v")));
    }

    /// The "Factory / Now / Target" table from a real conversation: long cells and links.
    const FACTORY: &str = "| Factory | Now | Target |\n|---|---|---|\n\
        | Merges an hour while PRs wait | 2.8 | 4 |\n\
        | Merges this hour | 2: [#2076](https://example.com/pull/2076), [#2090](https://example.com/pull/2090) | — |\n\
        | Merge queue | 12 deep; each group carries one PR and takes 30–60 min | — |\n\
        | Missions stalled | 9 | 0 |";

    fn rendered(input: &str, width: usize) -> Vec<String> {
        markdown(input, width, Style::default(), &crate::tests::theme())
            .iter()
            .map(plain)
            .collect()
    }

    #[test]
    fn a_table_with_long_cells_is_still_a_grid_that_fits_the_pane() {
        for width in [60, 80, 100, 140] {
            let text = rendered(FACTORY, width);
            assert!(
                text.iter().all(|line| self::width(line) <= width),
                "{width}: {text:#?}"
            );
            // Still a table: a header, a rule and a separator on every row.
            assert!(text[0].contains(" │ ") && text[0].starts_with("Factory"), "{width}: {text:#?}");
            assert!(text[1].contains("─┼─"), "{width}: {text:#?}");
            assert!(text[2..].iter().all(|line| line.contains(" │ ") || line.starts_with(' ')), "{width}: {text:#?}");
            // Links read as their text; the address is not printed into the cell.
            let all = text.join("\n");
            assert!(all.contains("#2076") && all.contains("#2090"), "{width}: {text:#?}");
            assert!(!all.contains("]("), "{width}: {text:#?}");
            assert!(!all.contains("example.com"), "{width}: {text:#?}");
            // Nothing is lost to the wrapping.
            for word in ["Merges", "queue", "stalled", "30–60", "deep;"] {
                assert!(all.contains(word), "{width}: missing {word}: {text:#?}");
            }
        }
    }

    #[test]
    fn a_table_that_fits_is_unchanged_and_wide_ones_do_not_pad_their_last_column() {
        let text = rendered("| k | value |\n|---|---|\n| x | y |", 40);
        assert_eq!(text, ["k │ value", "──┼──────", "x │ y"]);
        let wide = rendered(FACTORY, 60);
        assert!(wide.iter().all(|line| !line.ends_with(' ')), "{wide:#?}");
    }

    #[test]
    fn a_table_too_narrow_for_any_grid_is_stacked_as_records() {
        let text = rendered(FACTORY, 14);
        assert!(text.iter().all(|line| self::width(line) <= 14), "{text:#?}");
        assert!(text.iter().any(|line| line.starts_with("Now: ")), "{text:#?}");
        assert!(text.iter().any(|line| line.starts_with("Target: ")), "{text:#?}");
        assert!(text.contains(&String::new()), "records are told apart by a blank line: {text:#?}");
        assert!(!text.iter().any(|line| line.contains("─┼─")), "{text:#?}");
    }

    #[test]
    fn column_widths_give_up_space_widest_first_and_never_below_the_minimum() {
        assert_eq!(fit_columns(&[4, 5], 20), Some(vec![4, 5]));
        let squeezed = fit_columns(&[4, 60, 30], 50).unwrap();
        assert_eq!(squeezed.iter().sum::<usize>(), 50);
        assert_eq!(squeezed[0], 4, "a short column keeps its width");
        assert!(squeezed[1] > squeezed[2], "{squeezed:?}");
        assert_eq!(fit_columns(&[40, 40, 40], 12), None);
    }

    #[test]
    fn ansi_writes_colors_and_modifiers_and_resets() {
        let line = Line::from(vec![
            Span::styled("hi", Style::default().fg(Color::Rgb(1, 2, 3)).add_modifier(Modifier::BOLD)),
            Span::raw(" plain"),
        ]);
        assert_eq!(ansi(&line), "\x1b[1;38;2;1;2;3mhi\x1b[0m plain");
    }
}
