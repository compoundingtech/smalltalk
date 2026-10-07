//! Startup output only: bounded plain text, with credential and transcript lines withheld.
use std::collections::BTreeMap;

pub fn safe_tail(text: &str, environment: &BTreeMap<String, String>) -> String {
    let mut lines = text
        .lines()
        .rev()
        .take(20)
        .map(|line| {
            if line.chars().count() > 512 {
                return "[long startup line withheld]".into();
            }
            let mut line = line
                .chars()
                .take(512)
                .filter(|ch| !ch.is_control())
                .collect::<String>();
            let lower = line.to_ascii_lowercase();
            if [
                "authorization",
                "bearer ",
                "password",
                "client_secret",
                "secret",
                "token=",
                "token:",
                "apikey",
                "credential",
                "api_key",
                "api key",
                "sk-",
                "ghp_",
                "github_pat_",
                "akia",
                "-----begin",
                "\"role\"",
                "\"messages\"",
                "\"content\"",
                "user:",
                "assistant:",
            ]
            .iter()
            .any(|word| lower.contains(word))
            {
                return "[startup line withheld]".into();
            }
            for (key, value) in environment {
                if !value.is_empty()
                    && ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "AUTH"]
                        .iter()
                        .any(|word| key.to_ascii_uppercase().contains(word))
                {
                    line = line.replace(value, "[REDACTED]");
                }
            }
            line
        })
        .collect::<Vec<_>>();
    lines.reverse();
    let text = lines.join("\n");
    let mut start = text.len().saturating_sub(2048);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_tail_bounds_unicode_and_withholds_credentials_and_transcripts() {
        let env = BTreeMap::from([("PROVIDER_TOKEN".into(), "private-value".into())]);
        let text = format!(
            "Authorization: Bearer abc\n{{\"role\":\"user\",\"content\":\"private prompt\"}}\nprivate-value\n{}\nfixture launch rejected",
            "é".repeat(3000)
        );
        let tail = safe_tail(&text, &env);
        assert!(tail.len() <= 2048);
        assert!(tail.ends_with("fixture launch rejected"));
        for private in ["abc", "private prompt", "private-value"] {
            assert!(!tail.contains(private));
        }
    }
}
