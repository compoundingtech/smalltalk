pub fn clean_message_text(raw: &str) -> String {
    let normalized = raw.replace("\r\n", "\n");
    // st keeps 8 KB of a transcript value and says so in its own words.
    let (normalized, cut) =
        match normalized.strip_suffix("\n[st truncated this native timeline value]") {
            Some(kept) => (kept.to_owned(), true),
            None => (normalized, false),
        };
    let mut output = String::new();
    let mut plain = String::new();
    let mut code = false;
    for line in normalized.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            if !code {
                output.push_str(&strip_internal_markup(&plain));
                plain.clear();
            }
            output.push_str(line);
            code = !code;
        } else if code {
            output.push_str(line);
        } else {
            plain.push_str(line);
        }
    }
    output.push_str(&strip_internal_markup(&plain));
    let safe = output
        .chars()
        .filter(|character| *character == '\n' || *character == '\t' || !character.is_control())
        .collect::<String>();
    let text = safe
        .trim()
        .strip_prefix("[PING] ?")
        .unwrap_or(safe.trim())
        .trim();
    let text = if let Some(start) = text.rfind(" [id:message/")
        && text.ends_with(']')
    {
        text[..start].trim_end().to_owned()
    } else {
        text.to_owned()
    };
    if cut {
        format!("{text}\n\n… st kept only the start of this")
    } else {
        text
    }
}
fn strip_internal_markup(input: &str) -> String {
    let mut in_st3_channel = false;
    let mut text = input
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("<channel ")
                && trimmed.contains("source=\"plugin:st3-channel:st3\"")
                && trimmed.ends_with('>')
            {
                in_st3_channel = true;
                return false;
            }
            if in_st3_channel && trimmed == "</channel>" {
                in_st3_channel = false;
                return false;
            }
            if trimmed.starts_with("[st3-delivery:") && trimmed.ends_with(".md]") {
                return false;
            }
            true
        })
        .collect::<Vec<_>>()
        .join("\n");
    if input.ends_with('\n') && !text.is_empty() {
        text.push('\n');
    }
    for tag in [
        "analysis",
        "thinking",
        "think",
        "internal",
        "system-reminder",
        "function_calls",
        "tool_result",
    ] {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        let mut from = 0;
        loop {
            let lower = text.to_ascii_lowercase();
            let Some(start) = lower[from..].find(&open).map(|offset| from + offset) else {
                break;
            };
            // `<think` is not `<thinking`, and a tag named in `code` or mid-sentence is prose:
            // people write about these tags, and a mention must not cut their message short.
            let named = !lower[start + open.len()..].starts_with(['>', ' ', '/', '\n'])
                || in_inline_code(&text, start);
            let block = lower[..start]
                .rsplit('\n')
                .next()
                .is_some_and(|before| before.trim().is_empty());
            let open_end = lower[start..].find('>').map(|offset| start + offset + 1);
            let close = open_end.and_then(|open_end| {
                lower[open_end..]
                    .find(&close)
                    .map(|offset| open_end + offset + close.len())
            });
            match (named, close) {
                (true, _) => from = start + open.len(),
                (false, Some(end)) => text.replace_range(start..end, ""),
                // Hidden reasoning that never closed hides the rest; a mention does not.
                (false, None) if block => {
                    text.truncate(start);
                    break;
                }
                (false, None) => from = start + open.len(),
            }
        }
    }
    while let Some(start) = text.find("<|im_start|>") {
        let Some(separator) = text[start..]
            .find("<|im_sep|>")
            .map(|offset| start + offset)
        else {
            text.truncate(start);
            break;
        };
        let header = &text[start..separator];
        let hidden =
            header.contains("<|meta_sep|>analysis") || header.contains("<|meta_sep|>commentary");
        let body_start = separator + "<|im_sep|>".len();
        if hidden {
            let end = text[body_start..]
                .find("<|im_end|>")
                .map(|offset| body_start + offset + "<|im_end|>".len())
                .unwrap_or(text.len());
            text.replace_range(start..end, "");
        } else {
            text.replace_range(start..body_start, "");
        }
    }
    for token in ["<|im_end|>", "<|fim_suffix|>", "<|im_sep|>"] {
        text = text.replace(token, "");
    }
    text
}
/// Whether `at` falls inside a `code span` on its line.
fn in_inline_code(text: &str, at: usize) -> bool {
    let line_start = text[..at].rfind('\n').map_or(0, |index| index + 1);
    text[line_start..at].matches('`').count() % 2 == 1
}
