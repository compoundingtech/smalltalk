//! Launch-only native conversation selection for audited st3 seat restarts.
//!
//! Ordinary launches and binary adoption do not select a different conversation. Explicit
//! restart context supersedes authored session selection, but never arguments after `--`.

use anyhow::Result;

pub const NATIVE_SESSION_ENV: &str = "ST3_RESTART_NATIVE_SESSION";
pub const FRESH_CONTEXT_ENV: &str = "ST3_RESTART_FRESH_CONTEXT";
/// Hook-only proof of the first native session selected by a restart launch.
pub const EXPECTED_SESSION_ENV: &str = "ST_RESTART_EXPECTED_NATIVE_SESSION";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Context {
    #[default]
    Ordinary,
    Fresh,
    Resume(String),
}

impl Context {
    pub fn from_env() -> Self {
        Self::from_values(
            std::env::var(FRESH_CONTEXT_ENV).ok().as_deref(),
            std::env::var(NATIVE_SESSION_ENV).ok().as_deref(),
        )
    }

    pub fn from_values(fresh: Option<&str>, native_session: Option<&str>) -> Self {
        if fresh == Some("1") {
            Self::Fresh
        } else if let Some(id) = native_session.filter(|id| !id.is_empty()) {
            Self::Resume(id.to_owned())
        } else {
            Self::Ordinary
        }
    }

    pub fn native_session(&self) -> Option<&str> {
        match self {
            Self::Resume(id) => Some(id),
            Self::Ordinary | Self::Fresh => None,
        }
    }

    pub fn apply_argv(&self, harness: &str, mut argv: Vec<String>) -> Result<Vec<String>> {
        if matches!(self, Self::Ordinary) {
            return Ok(argv);
        }
        anyhow::ensure!(!argv.is_empty(), "restart provider argv is empty");
        let mut program = true;
        let mut literal = false;
        let mut selection_value = false;
        let mut preserve_value = false;
        let mut image_values = false;
        argv.retain(|arg| {
            if program {
                program = false;
                return true;
            }
            if literal {
                return true;
            }
            if preserve_value {
                preserve_value = false;
                return true;
            }
            if image_values {
                if !arg.starts_with('-') {
                    return true;
                }
                image_values = false;
            }
            if selection_value {
                selection_value = false;
                if !arg.starts_with('-') {
                    return false;
                }
            }
            if arg == "--" {
                literal = true;
                return true;
            }
            let name = arg.split_once('=').map_or(arg.as_str(), |(name, _)| name);
            // Codex's image option is variadic. Its launch parser requires a `--` prompt
            // boundary for automatic resume, so image paths must remain opaque up to it.
            if harness == "codex" && matches!(name, "--image" | "-i") {
                image_values = true;
                return true;
            }
            // Keep values opaque, even when prompt text or a model name happens to spell a
            // session selector. Provider-specific short flags have different meanings.
            let takes_unrelated_value = matches!(name,
                "--model" | "--system-prompt" | "--append-system-prompt" | "--system-prompt-file"
                | "--append-system-prompt-file" | "--mcp-config" | "--settings" | "--permission-mode"
                | "--session-dir" | "--provider" | "--thinking" | "--extension" | "-e"
                | "--config" | "--profile" | "--cd" | "--sandbox" | "--ask-for-approval"
                | "--enable" | "--disable" | "--add-dir" | "--remote" | "--remote-auth"
                | "--image" | "-i" | "-m" | "--agent" | "--prompt" | "--port" | "--hostname"
            ) || (harness == "codex" && matches!(name,
                "-c" | "-p" | "-C" | "-s" | "-a" | "--local-provider" | "--remote-auth-token-env"
            ));
            if takes_unrelated_value {
                preserve_value = !arg.contains('=');
                return true;
            }
            let selection = match harness {
                "claude" => match name {
                    "-c" | "--continue" | "--fork-session" => Some(false),
                    "-r" | "--resume" | "--from-pr" | "--session-id" | "--teleport" => Some(true),
                    _ => None,
                },
                "omp" => match name {
                    "-c" | "--continue" | "--no-session" | "--fork" | "--from-claude" | "--from-codex" => Some(false),
                    "-r" | "--resume" | "--session" => Some(true),
                    _ => None,
                },
                "pi" => match name {
                    "-c" | "--continue" | "--no-session" => Some(false),
                    "-r" | "--resume" | "--session" => Some(true),
                    _ => None,
                },
                "opencode" => match name {
                    "-c" | "--continue" | "--fork" => Some(false),
                    "-s" | "--session" => Some(true),
                    _ => None,
                },
                "codex" => match name {
                    "--last" | "--all" => Some(false),
                    "resume" | "fork" => Some(true),
                    _ => None,
                },
                _ => None,
            };
            if let Some(takes_value) = selection {
                selection_value = takes_value && !arg.contains('=');
                false
            } else {
                true
            }
        });
        if let Some(id) = self.native_session() {
            let selector = match harness {
                "claude" | "omp" => Some("--resume"),
                "pi" | "opencode" => Some("--session"),
                // The controlled Codex launcher selects the exact thread through app-server.
                "codex" => None,
                _ => anyhow::bail!("{harness} does not support native restart context"),
            };
            if let Some(selector) = selector {
                argv.splice(1..1, [selector.to_owned(), id.to_owned()]);
            }
        }
        Ok(argv)
    }
}

/// These controls are for the wrapper's one launch, never the provider or its future children.
pub fn remove_launch_environment(command: &mut std::process::Command) {
    command.env_remove(NATIVE_SESSION_ENV).env_remove(FRESH_CONTEXT_ENV).env_remove(EXPECTED_SESSION_ENV);
}

pub fn verify_native_session(expected: Option<&str>, actual: &str) -> Result<()> {
    if let Some(expected) = expected {
        anyhow::ensure!(actual == expected, "native restart bound session {actual:?}, expected {expected:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn restart_context_fresh_overrides_capture_and_authored_selection() {
        assert_eq!(Context::from_values(Some("1"), Some("captured")), Context::Fresh);
        for (harness, selection) in [
            ("claude", argv(&["--resume=old", "--fork-session", "--session-id", "other"])),
            ("omp", argv(&["-r", "old", "--continue", "--from-codex"])),
            ("pi", argv(&["--session", "old", "-r", "--continue"])),
            ("codex", argv(&["resume", "old", "--last"])),
            ("opencode", argv(&["-s", "old", "--continue", "--fork"])),
        ] {
            let mut input = argv(&[harness, "--model", "keep"]);
            input.extend(selection);
            input.extend(argv(&["--", "--resume", "literal prompt"]));
            assert_eq!(Context::Fresh.apply_argv(harness, input).unwrap(), argv(&[harness, "--model", "keep", "--", "--resume", "literal prompt"]));
        }
    }

    #[test]
    fn restart_context_resume_selects_exact_capture_without_forking() {
        for (harness, selector) in [("claude", "--resume"), ("omp", "--resume"), ("pi", "--session"), ("opencode", "--session")] {
            let input = argv(&[harness, "--continue", "--model", "keep"]);
            assert_eq!(Context::Resume("exact".into()).apply_argv(harness, input).unwrap(), argv(&[harness, selector, "exact", "--model", "keep"]));
        }
        assert_eq!(Context::Resume("exact".into()).apply_argv("codex", argv(&["codex", "fork", "other", "--model", "keep"])).unwrap(), argv(&["codex", "--model", "keep"]));
    }

    #[test]
    fn restart_context_ordinary_does_not_change_authored_selection() {
        let input = argv(&["pi", "--session", "authored", "--model", "keep"]);
        assert_eq!(Context::Ordinary.apply_argv("pi", input.clone()).unwrap(), input);
        assert_eq!(Context::from_values(None, None), Context::Ordinary);
    }

    #[test]
    fn restart_context_keeps_unrelated_option_values_opaque() {
        for (harness, input) in [
            ("claude", argv(&["claude", "--system-prompt", "--resume", "--resume", "old", "--mcp-config", "keep"])),
            ("codex", argv(&["codex", "--model", "resume", "-c", "fork", "resume", "old", "--sandbox", "keep"])),
            ("pi", argv(&["pi", "--session-dir", "/keep", "--session", "old", "--model", "keep"])),
            ("omp", argv(&["omp", "--from-codex", "unrelated prompt", "--model", "keep"])),
        ] {
            let expected = match harness {
                "claude" => argv(&["claude", "--system-prompt", "--resume", "--mcp-config", "keep"]),
                "codex" => argv(&["codex", "--model", "resume", "-c", "fork", "--sandbox", "keep"]),
                "omp" => argv(&["omp", "unrelated prompt", "--model", "keep"]),
                _ => argv(&["pi", "--session-dir", "/keep", "--model", "keep"]),
            };
            assert_eq!(Context::Fresh.apply_argv(harness, input).unwrap(), expected);
        }
    }

    #[test]
    fn restart_context_keeps_codex_provider_auth_and_variadic_images_opaque() {
        let input = argv(&[
            "codex", "--local-provider", "resume", "--remote-auth-token-env", "fork",
            "--image", "resume", "fork", "--", "literal prompt",
        ]);
        assert_eq!(
            Context::Resume("exact".into()).apply_argv("codex", input.clone()).unwrap(),
            input,
        );
    }

    #[test]
    fn restart_context_binding_rejects_a_different_conversation() {
        verify_native_session(Some("exact"), "exact").unwrap();
        assert!(verify_native_session(Some("exact"), "other").is_err());
        verify_native_session(None, "new").unwrap();
    }
}
