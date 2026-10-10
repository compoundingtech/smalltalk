//! Positive terminal evidence of a provider that cannot accept work.
//! Match UI lines and complete menus, never a phrase quoted in a tool transcript.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockingScreen {
    pub code: &'static str,
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
    let latest_reply = lines.iter().rposition(|line| line.starts_with('●'));
    let login = match driver {
        "claude" => lines.iter().enumerate().find_map(|(index, line)| {
            let text = ui_line(line);
            let standalone = latest_reply.is_none_or(|latest| index == latest);
            let matched = [
                "Login expired · Please run /login",
                "Not logged in · Run /login",
            ]
            .iter()
            .any(|phrase| {
                standalone
                    && text
                        .strip_prefix(phrase)
                        .is_some_and(|tail| tail.is_empty() || tail.starts_with(" ·"))
                    || ["? for shortcuts", "⏵⏵ bypass permissions on"]
                        .iter()
                        .any(|prefix| {
                            text.starts_with(prefix)
                                && text
                                    .strip_suffix(phrase)
                                    .is_some_and(|footer| footer.ends_with("  "))
                        })
            }) || standalone && crate::claude_session::claude_login_reply(&text);
            matched.then_some(line)
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
            code: "provider-auth-expired",
            text: (*line).to_owned(),
        });
    }
    if driver == "claude" && lines.contains(&"Login") && lines.contains(&"Esc to cancel") {
        let opening = lines
            .iter()
            .any(|line| line.ends_with("Opening browser to sign in…"));
        let browser = lines
            .iter()
            .find(|line| line.starts_with("Browser didn't open? Use the url below to sign in"));
        let code_prompt = lines
            .iter()
            .any(|line| line.starts_with("Paste code here if prompted >"));
        if opening || (browser.is_some() && code_prompt) {
            return Some(BlockingScreen {
                code: "provider-auth-expired",
                // Never include an OAuth URL, state/challenge, or a pasted code in a diagnostic.
                text: browser
                    .map_or("Opening browser to sign in…", |line| *line)
                    .into(),
            });
        }
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
            code: "provider-auth-expired",
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
            code: "provider-auth-expired",
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
                code: "provider-update-prompt",
                text: (*headline).to_owned(),
            });
        }
    }
    None
}

/// Whether Claude's own input box is on screen: a full-width rule, the `❯` prompt line and
/// another rule, as at the end of every finished turn. Claude replaces the box with its dialog
/// while a permission or question prompt is up, so the box being back means the prompt is gone,
/// answered or dismissed in the terminal (measured on Claude Code 2.1.296: Esc or "No" ends the
/// turn without any hook event, and the input box returns).
pub fn claude_input_ready(screen: &str) -> bool {
    let rule = |line: &str| line.chars().count() >= 20 && line.chars().all(|c| c == '─');
    let lines = screen
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    lines
        .windows(3)
        .any(|window| rule(window[0]) && window[1].starts_with('❯') && rule(window[2]))
}

#[cfg(test)]
mod tests {
    const PERMISSION_DIALOG: &str = "❯ Run it.\n  ⎿  $ touch probe-file\n────────────────────────────────────────\n Bash command\n touch probe-file\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n Esc to cancel · Tab to amend\n";
    const DENIED: &str = "❯ Run it.\n  ⎿  Interrupted · What should Claude do instead?\n────────────────────────────────────────\n❯ \n────────────────────────────────────────\n  ⏸ manual mode on · ? for shortcuts · ← for agents\n";

    #[test]
    fn claude_input_box_marks_a_prompt_gone_and_a_dialog_does_not() {
        assert!(!super::claude_input_ready(PERMISSION_DIALOG));
        assert!(super::claude_input_ready(DENIED));
        assert!(!super::claude_input_ready(""));
    }

    #[test]
    fn plain_followup_tool_lines_are_not_login_diagnostics() {
        for text in [
            "Please run /login",
            "Invalid API key",
            "Login expired · Please run /login",
        ] {
            let screen = format!("● Bash(cat log)\n  ⎿ first output line\n    {text}");
            assert_eq!(super::detect("claude", &screen), None);
        }
    }

    use super::*;

    #[test]
    fn captured_blocking_screens() {
        for (driver, screen, code) in [
            (
                "claude",
                include_str!("../tests/fixtures/blocking-screens/claude-footer.txt"),
                "provider-auth-expired",
            ),
            (
                "claude",
                include_str!("../tests/fixtures/blocking-screens/claude-login.txt"),
                "provider-auth-expired",
            ),
            (
                "claude",
                include_str!("../tests/fixtures/blocking-screens/claude-authenticating.txt"),
                "provider-auth-expired",
            ),
            (
                "claude",
                include_str!("../tests/fixtures/blocking-screens/claude-code-prompt.txt"),
                "provider-auth-expired",
            ),
            (
                "claude",
                "? for shortcuts                 Not logged in · Run /login",
                "provider-auth-expired",
            ),
            (
                "claude",
                include_str!("../tests/fixtures/blocking-screens/claude-wide-footer.txt"),
                "provider-auth-expired",
            ),
            (
                "claude",
                "● Login expired · Please run /login",
                "provider-auth-expired",
            ),
            (
                "codex",
                include_str!("../tests/fixtures/blocking-screens/codex-login.txt"),
                "provider-auth-expired",
            ),
            (
                "codex",
                include_str!("../tests/fixtures/blocking-screens/codex-update.txt"),
                "provider-update-prompt",
            ),
            (
                "pi",
                include_str!("../tests/fixtures/blocking-screens/pi-no-key.txt"),
                "provider-auth-expired",
            ),
            (
                "omp",
                include_str!("../tests/fixtures/blocking-screens/omp-no-key.txt"),
                "provider-auth-expired",
            ),
        ] {
            assert_eq!(detect(driver, screen).unwrap().code, code, "{driver}");
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
