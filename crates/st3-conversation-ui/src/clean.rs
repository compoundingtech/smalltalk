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
                && trimmed
                    .split("source=\"")
                    .nth(1)
                    .and_then(|rest| rest.split('"').next())
                    .is_some_and(crate::adapt::channel_source)
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
    text
}
