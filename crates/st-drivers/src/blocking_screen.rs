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
    // A first run: the text-style picker precedes the login screen, and a fresh config is signed
    // out by definition. Attaching to the seat completes both.
    if driver == "claude"
        && lines.contains(&"Choose the text style that looks best with your terminal")
        && lines.contains(&"To change this later, run /theme")
    {
        return Some(BlockingScreen {
            code: "provider-auth-expired",
            text: "Choose the text style that looks best with your terminal".into(),
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

/// The controlled Codex update menu explicitly offers Escape as Skip. Refuse
/// partial menus and transcript content: Escape must never reach an input box,
/// authentication dialog, or a menu whose key semantics have changed.
pub fn codex_update_skip_key(driver: &str, screen: &str) -> Option<&'static str> {
    if detect(driver, screen)?.code != "provider-update-prompt" {
        return None;
    }
    let lines: Vec<_> = screen
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if !lines.contains(&"enter continue · esc skip") {
        return None;
    }
    lines
        .iter()
        .all(|line| {
            let line = line.trim_start_matches(['›', '❯', ' ']);
            line.starts_with("Update available · ")
                || line == "Release notes: https://github.com/openai/codex/releases/latest"
                || line.starts_with("1. Update now (runs `") && line.ends_with("`)")
                || line == "2. Skip"
                || line == "3. Skip until next version"
                || line == "enter continue · esc skip"
        })
        .then_some("escape")
}

/// A prompt a harness shows only on its screen, with no hook or protocol event: what it asks,
/// its options in screen order, and which one is selected. st raises it for the seat's person and
/// answers it by moving the dialog's own selection and pressing Return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenPrompt {
    /// `mcp-server` and `trust` (Claude), `plan` (Claude's plan approval), `trust` (Codex).
    pub kind: &'static str,
    /// The prompt's headline as the screen shows it.
    pub title: String,
    pub options: Vec<String>,
    pub selected: usize,
}

impl ScreenPrompt {
    /// The keys that choose the option whose label starts with `label`: the dialog's arrows from
    /// the selected option to it, then Return.
    pub fn keys_for(&self, label: &str) -> Option<Vec<&'static str>> {
        let target = self.options.iter().position(|option| option.starts_with(label))?;
        let step = if target > self.selected { "down" } else { "up" };
        let mut keys = vec![step; target.abs_diff(self.selected)];
        keys.push("return");
        Some(keys)
    }
}

/// The answers a person may give a screen-only prompt, each with the label of the option it
/// chooses. Claude's plan approval is answered through its permission prompt, not here.
pub fn screen_prompt_choices(driver: &str, kind: &str) -> &'static [(&'static str, &'static str)] {
    match (driver, kind) {
        ("claude", "mcp-server") => &[
            ("use", "Use this MCP server"),
            ("use-all", "Use this and all future MCP servers in this project"),
            ("skip", "Continue without using this MCP server"),
        ],
        ("claude", "trust") => &[("trust", "Yes, I trust this folder"), ("exit", "No, exit")],
        ("codex", "trust") => &[("trust", "Yes, continue"), ("quit", "No, quit")],
        _ => &[],
    }
}

/// The screen-only prompt `screen` shows, when it is one st knows in full. Every option and the
/// dialog's own footer must be present, so a transcript quoting a line of it is never mistaken
/// for the dialog, and a changed dialog is never answered with keys meant for another.
pub fn screen_prompt(driver: &str, screen: &str) -> Option<ScreenPrompt> {
    let lines: Vec<&str> = screen.lines().map(str::trim).collect();
    // One option row: its label, and whether the dialog's selection marker is on it.
    let row = |line: &str| -> (String, bool) {
        let selected = line.starts_with(['❯', '›']);
        let label = line.trim_start_matches(['❯', '›', ' ', '✔']);
        let digits = label.chars().take_while(char::is_ascii_digit).count();
        let label = match label[digits..].strip_prefix(". ") {
            Some(rest) if digits > 0 => rest,
            _ => label,
        };
        (label.trim().to_owned(), selected)
    };
    // The rows from `start` whose labels begin with each of `labels`, in that order.
    let menu = |start: usize, labels: &[&str]| -> Option<(Vec<String>, usize)> {
        let rows: Vec<(String, bool)> = lines[start..]
            .iter()
            .filter(|line| !line.is_empty())
            .map(|line| row(line))
            .collect();
        let first = rows.iter().position(|(label, _)| label.starts_with(labels[0]))?;
        let found = rows.get(first..first + labels.len())?;
        if !found.iter().zip(labels).all(|((label, _), want)| label.starts_with(want)) {
            return None;
        }
        let selected = found.iter().position(|(_, selected)| *selected)?;
        Some((found.iter().map(|(label, _)| label.clone()).collect(), selected))
    };
    let footer = |text: &str| lines.iter().any(|line| line.starts_with(text));
    let headline = |prefix: &str| lines.iter().position(|line| line.starts_with(prefix));
    let prompt = |kind, title: &str, (options, selected): (Vec<String>, usize)| ScreenPrompt {
        kind,
        title: title.to_owned(),
        options,
        selected,
    };
    match driver {
        "claude" => {
            if let Some(at) = headline("New MCP server found in this project: ")
                && footer("Enter to confirm · Esc to cancel")
            {
                let options = menu(
                    at,
                    &[
                        "Use this MCP server",
                        "Use this and all future MCP servers in this project",
                        "Continue without using this MCP server",
                    ],
                )?;
                return Some(prompt("mcp-server", lines[at], options));
            }
            if let Some(at) = headline("Quick safety check: Is this a project you created or one you trust?")
                && footer("Enter to confirm · Esc to cancel")
            {
                // The order changed between Claude versions; the labels did not.
                let options = menu(at, &["No, exit", "Yes, I trust this folder"])
                    .or_else(|| menu(at, &["Yes, I trust this folder", "No, exit"]))?;
                return Some(prompt(
                    "trust",
                    "Quick safety check: Is this a project you created or one you trust?",
                    options,
                ));
            }
            if let Some(at) = lines
                .iter()
                .position(|line| line.ends_with("Would you like to proceed?"))
            {
                // The approving options name the permission mode Claude continues in, which
                // depends on the seat's own mode: only their shape is fixed.
                let (options, selected) =
                    menu(at, &["Yes, ", "Yes, ", "Tell Claude what to change"])?;
                return Some(prompt("plan", "Would you like to proceed?", (options, selected)));
            }
            None
        }
        "codex" => {
            let at = headline("Do you trust the contents of this directory?")?;
            if !footer("Press enter to continue") {
                return None;
            }
            let options = menu(at, &["Yes, continue", "No, quit"])?;
            Some(prompt("trust", "Do you trust the contents of this directory?", options))
        }
        _ => None,
    }
}

/// Whether Claude's own input box is on screen: a full-width rule, the `❯` prompt line and
/// another rule. Claude replaces the box with its dialog while a permission, question or plan
/// prompt is up, and shows it again once the prompt is answered or dismissed (measured on Claude
/// Code 2.1.296: Esc or "No" ends the turn without any hook event). The box is also on screen
/// while Claude works; use `claude_input_idle` when its absence of a dialog must prove closure.
pub fn claude_input_ready(screen: &str) -> bool {
    let rule = |line: &str| line.chars().count() >= 20 && line.chars().all(|c| c == '─');
    // A dialog's selected choice (`❯ 1. Yes`) is not the input prompt.
    let prompt = |line: &str| {
        line.strip_prefix('❯').is_some_and(|rest| {
            let rest = rest.trim_start();
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            digits == 0 || !rest[digits..].starts_with('.')
        })
    };
    let lines = screen
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    lines
        .windows(3)
        .any(|window| rule(window[0]) && prompt(window[1]) && rule(window[2]))
}

/// Positive native idle evidence, not merely a box that can remain visible while a tool runs
/// or before its permission dialog is drawn. The current footer must offer shortcuts rather
/// than interruption. This can be re-derived after a missed dismissal or daemon restart.
pub fn claude_input_idle(screen: &str) -> bool {
    claude_input_ready(screen)
        && screen.lines().rev().find(|line| !line.trim().is_empty()).is_some_and(|footer| {
            footer.contains("? for shortcuts") && !footer.contains("esc to interrupt")
        })
}

#[cfg(test)]
mod tests {
    #[test]
    fn claude_input_box_is_back_after_a_refusal_and_absent_in_every_dialog() {
        let screen = |name: &str| -> &'static str {
            match name {
                "claude-permission-dialog.txt" => include_str!("../tests/fixtures/blocking-screens/claude-permission-dialog.txt"),
                "claude-edit-dialog.txt" => include_str!("../tests/fixtures/blocking-screens/claude-edit-dialog.txt"),
                "claude-ask-question.txt" => include_str!("../tests/fixtures/blocking-screens/claude-ask-question.txt"),
                "claude-plan-dialog.txt" => include_str!("../tests/fixtures/blocking-screens/claude-plan-dialog.txt"),
                "claude-trust.txt" => include_str!("../tests/fixtures/blocking-screens/claude-trust.txt"),
                "claude-permission-refused.txt" => include_str!("../tests/fixtures/blocking-screens/claude-permission-refused.txt"),
                "claude-edit-refused.txt" => include_str!("../tests/fixtures/blocking-screens/claude-edit-refused.txt"),
                "claude-working.txt" => include_str!("../tests/fixtures/blocking-screens/claude-working.txt"),
                _ => unreachable!("{name}"),
            }
        };
        for dialog in [
            "claude-permission-dialog.txt",
            "claude-edit-dialog.txt",
            "claude-ask-question.txt",
            "claude-plan-dialog.txt",
            "claude-trust.txt",
        ] {
            assert!(!super::claude_input_ready(screen(dialog)), "{dialog}");
            assert!(!super::claude_input_idle(screen(dialog)), "{dialog}");
        }
        // The working box is not closure; the current native idle footer is.
        for input in [
            "claude-permission-refused.txt",
            "claude-edit-refused.txt",
            "claude-working.txt",
        ] {
            assert!(super::claude_input_ready(screen(input)), "{input}");
        }
        for idle in ["claude-permission-refused.txt", "claude-edit-refused.txt"] {
            assert!(super::claude_input_idle(screen(idle)), "{idle}");
        }
        assert!(!super::claude_input_idle(screen("claude-working.txt")));
        assert!(!super::claude_input_idle(
            "────────────────────────────────────────\n❯\n────────────────────────────────────────\n"));
        assert!(!super::claude_input_idle(""));
        assert!(!super::claude_input_ready(""));
        // A dialog's selected choice between two rules is not the input prompt.
        assert!(!super::claude_input_ready(
            "────────────────────────────────────────\n❯ 1. Yes\n────────────────────────────────────────\n"
        ));
    }

    #[test]
    fn screen_prompts_are_read_in_full_and_answered_with_their_own_keys() {
        let mcp = super::screen_prompt(
            "claude",
            include_str!("../tests/fixtures/blocking-screens/claude-mcp-server.txt"),
        )
        .unwrap();
        assert_eq!(mcp.kind, "mcp-server");
        assert_eq!(mcp.title, "New MCP server found in this project: probe-echo");
        assert_eq!(mcp.selected, 2);
        assert_eq!(mcp.keys_for("Use this MCP server"), Some(vec!["up", "up", "return"]));
        assert_eq!(
            mcp.keys_for("Continue without using this MCP server"),
            Some(vec!["return"])
        );

        let trust = super::screen_prompt(
            "claude",
            include_str!("../tests/fixtures/blocking-screens/claude-trust.txt"),
        )
        .unwrap();
        assert_eq!(trust.kind, "trust");
        assert_eq!(trust.options, vec!["No, exit", "Yes, I trust this folder"]);
        assert_eq!(trust.keys_for("Yes, I trust this folder"), Some(vec!["down", "return"]));

        let plan = super::screen_prompt(
            "claude",
            include_str!("../tests/fixtures/blocking-screens/claude-plan-dialog.txt"),
        )
        .unwrap();
        assert_eq!(plan.kind, "plan");
        assert_eq!(plan.options[0], "Yes, and use auto mode");
        assert_eq!(plan.keys_for("Yes, "), Some(vec!["return"]));

        let codex = super::screen_prompt(
            "codex",
            include_str!("../tests/fixtures/blocking-screens/codex-trust.txt"),
        )
        .unwrap();
        assert_eq!(codex.kind, "trust");
        assert_eq!(codex.keys_for("No, quit"), Some(vec!["down", "return"]));

        // Other screens, a quoted line, a partial dialog or another driver are no prompt.
        for (driver, screen) in [
            ("claude", include_str!("../tests/fixtures/blocking-screens/claude-working.txt")),
            ("claude", include_str!("../tests/fixtures/blocking-screens/claude-permission-dialog.txt")),
            ("claude", "● New MCP server found in this project: probe-echo"),
            ("codex", include_str!("../tests/fixtures/blocking-screens/claude-mcp-server.txt")),
            ("claude", &*include_str!("../tests/fixtures/blocking-screens/claude-mcp-server.txt")
                .replace("Use this and all future MCP servers in this project", "Something else")),
        ] {
            assert_eq!(super::screen_prompt(driver, screen), None, "{screen}");
        }
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
                "claude",
                include_str!("../tests/fixtures/blocking-screens/claude-theme.txt"),
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
    fn codex_update_skip_requires_the_complete_active_menu() {
        let menu = include_str!("../tests/fixtures/blocking-screens/codex-update.txt");
        assert_eq!(codex_update_skip_key("codex", menu), Some("escape"));
        for screen in [
            menu.replace("enter continue · esc skip", "enter continue"),
            menu.replace("2. Skip", "2. Cancel"),
            format!("Tool output:\n{menu}"),
            format!("{menu}\n› unsent draft"),
            menu.lines().map(|line| format!("> {line}\n")).collect(),
            include_str!("../tests/fixtures/blocking-screens/codex-login.txt").into(),
        ] {
            assert_eq!(codex_update_skip_key("codex", &screen), None, "{screen}");
        }
        assert_eq!(codex_update_skip_key("claude", menu), None);
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
