//! Positive terminal evidence of a provider that cannot accept work.
//! Match UI lines and complete menus, never a phrase quoted in a tool transcript.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockingScreen {
    pub blocked_on: &'static str,
    pub text: String,
}

pub fn detect(driver: &str, screen: &str) -> Option<BlockingScreen> {
    let lines: Vec<_> = screen.lines().map(str::trim).collect();
    let ui_line = |line: &str| {
        line.trim_matches(['│', '┃', ' '])
            .trim_start_matches(['●', '⎿', '!', '✦', '✨'])
            .trim_start()
            .to_owned()
    };
    let login = match driver {
        "claude" => lines.iter().find(|line| {
            let text = ui_line(line);
            [
                "Login expired · Please run /login",
                "Not logged in · Run /login",
            ]
            .iter()
            .any(|phrase| {
                text.strip_prefix(phrase)
                    .is_some_and(|tail| tail.is_empty() || tail.starts_with(" ·"))
                    || text
                        .strip_prefix("? for shortcuts")
                        .is_some_and(|footer| footer.trim() == *phrase)
            })
        }),
        "pi" | "omp" => lines.iter().enumerate().find_map(|(index, line)| {
            let text = ui_line(line);
            let text = text.strip_prefix("Error: ").unwrap_or(&text);
            let provider = text
                .strip_prefix("No API key found for ")?
                .trim_end_matches('.')
                .to_lowercase();
            if text.contains(['`', '\''])
                || !lines
                    .iter()
                    .any(|line| ui_line(line).starts_with("Use /login"))
            {
                return None;
            }
            // Authentication status is later in the transcript than the credential error it
            // resolves. A successful login to another provider does not resolve this one.
            let restored = lines[index + 1..].iter().any(|line| {
                let line = line.trim_start_matches(['✓', '✔', ' ']).to_lowercase();
                [
                    "logged in to ",
                    "saved api key for ",
                    "successfully logged in to ",
                ]
                .iter()
                .any(|prefix| {
                    line.strip_prefix(prefix).is_some_and(|tail| {
                        tail == provider || tail.starts_with(&format!("{provider} as "))
                    })
                })
            });
            (!restored).then_some(line)
        }),
        _ => None,
    };
    if let Some(line) = login {
        return Some(BlockingScreen {
            blocked_on: "login",
            text: (*line).to_owned(),
        });
    }
    if driver == "claude"
        && lines.contains(&"Select login method:")
        && lines.iter().any(|line| {
            line.trim_start_matches(['❯', ' '])
                .starts_with("1. Claude account with subscription")
        })
        && lines
            .iter()
            .any(|line| line.starts_with("2. Anthropic Console account"))
    {
        return Some(BlockingScreen {
            blocked_on: "login",
            text: "Select login method:".into(),
        });
    }
    if driver == "codex"
        && lines
            .iter()
            .any(|line| line.starts_with("Welcome to Codex,"))
        && lines.iter().any(|line| {
            line.trim_start_matches(['>', '›', ' '])
                .starts_with("1. Sign in with ChatGPT")
        })
        && lines
            .iter()
            .any(|line| line.starts_with("2. Sign in with Device Code"))
        && lines.iter().any(|line| {
            line.starts_with("3. Provide your own API key")
                || line.starts_with("3. Use an OpenAI API key")
        })
    {
        return Some(BlockingScreen {
            blocked_on: "login",
            text: "Sign in with ChatGPT or provide an API key".into(),
        });
    }
    if driver == "codex" {
        let headline = lines
            .iter()
            .find(|line| ui_line(line).starts_with("Update available"));
        let option = |prefix: &str| {
            lines
                .iter()
                .any(|line| line.trim_start_matches(['›', '❯', ' ']).starts_with(prefix))
        };
        if let Some(headline) = headline
            && option("1. Update now")
            && option("2. Skip")
            && option("3. Skip until next version")
        {
            return Some(BlockingScreen {
                blocked_on: "harness-modal",
                text: (*headline).to_owned(),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_blocking_screens() {
        for (driver, screen, blocked_on) in [
            (
                "claude",
                include_str!("../tests/fixtures/blocking-screens/claude-footer.txt"),
                "login",
            ),
            (
                "claude",
                include_str!("../tests/fixtures/blocking-screens/claude-login.txt"),
                "login",
            ),
            (
                "claude",
                "? for shortcuts                 Not logged in · Run /login",
                "login",
            ),
            ("claude", "● Login expired · Please run /login", "login"),
            (
                "codex",
                include_str!("../tests/fixtures/blocking-screens/codex-login.txt"),
                "login",
            ),
            (
                "codex",
                include_str!("../tests/fixtures/blocking-screens/codex-update.txt"),
                "harness-modal",
            ),
            (
                "pi",
                include_str!("../tests/fixtures/blocking-screens/pi-no-key.txt"),
                "login",
            ),
            (
                "omp",
                include_str!("../tests/fixtures/blocking-screens/omp-no-key.txt"),
                "login",
            ),
        ] {
            assert_eq!(
                detect(driver, screen).unwrap().blocked_on,
                blocked_on,
                "{driver}"
            );
        }
    }

    #[test]
    fn a_later_login_clears_a_pi_family_error_for_that_provider() {
        for (driver, success) in [
            ("pi", "Logged in to Anthropic"),
            ("omp", "✓ Successfully logged in to anthropic"),
        ] {
            let error = "Error: No API key found for anthropic.\nUse /login or set an API key environment variable.";
            assert!(detect(driver, &format!("{error}\n{success}")).is_none());
            assert!(detect(driver, &format!("{success}\n{error}")).is_some());
            assert!(detect(driver, &format!("{error}\nLogged in to openai")).is_some());
        }
    }

    #[test]
    fn transcripts_banners_and_other_drivers_are_not_blocking_screens() {
        for (driver, screen) in [
            (
                "claude",
                r#"37: text.starts_with("Not logged in · Run /login")"#,
            ),
            (
                "claude",
                "● The detector matches Not logged in · Run /login",
            ),
            ("claude", "Please use /login to switch accounts"),
            ("codex", "✨ Update available! 1.0 → 1.1"),
            (
                "codex",
                r#"source: "Update available!"
"1. Update now"
"2. Skip"
"3. Skip until next version""#,
            ),
            ("pi", "No API key found for an example in documentation."),
            ("opencode", "Connect a provider"),
            (
                "claude",
                include_str!("../tests/fixtures/blocking-screens/codex-update.txt"),
            ),
        ] {
            assert!(detect(driver, screen).is_none(), "{driver}: {screen}");
        }
    }
}
