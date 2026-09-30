//! st no longer starts a seat with a prompt: a started or restarted seat takes no turn until a
//! person types or a graph message is posted. Declarations published before that change still
//! carry the old boot prompt at the end of their stored launch argv, so every driver removes it
//! before it starts the provider.

/// Every boot prompt that older st releases appended to a typed harness argv, oldest first. A
/// stored declaration keeps the wording of the release that published it.
pub const LEGACY_BOOT_PROMPTS: &[&str] = &[
    // 77efff71 (2026-09-08), the first composed boot prompt.
    "Read @.st3/boot.md completely. Then list and claim your current st3 work.",
    // 159ef8aa (2026-09-08) asked the seat to finish claimed work.
    "Read @.st3/boot.md completely. Then list, claim, do, and finish your current st3 work.",
    // 8bbc3a4d (2026-09-27) renamed st3 to st; 6e216698 (2026-09-29) removed the prompt.
    "Read @.st3/boot.md completely. Then list, claim, do, and finish your current st work.",
];

/// Whether one argument is a legacy boot prompt, alone or composed after an authored prompt.
fn is_legacy_prompt(argument: &str) -> bool {
    LEGACY_BOOT_PROMPTS.iter().any(|prompt| {
        argument == *prompt
            || argument
                .strip_suffix(prompt)
                .is_some_and(|authored| authored.ends_with("\n\n"))
    })
}

/// Remove a legacy startup prompt from a typed harness argv. The prompt was the last argument,
/// alone or after an authored prompt (`AUTHORED\n\nBOOT`); OpenCode received it as `--prompt`.
/// Both forms go, because an authored startup prompt would also start a turn.
pub fn strip_legacy_prompt(driver: &str, argv: &mut Vec<String>) {
    if !argv
        .last()
        .is_some_and(|argument| is_legacy_prompt(argument))
    {
        return;
    }
    argv.pop();
    if driver == "opencode" && argv.last().is_some_and(|argument| argument == "--prompt") {
        argv.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(arguments: &[&str]) -> Vec<String> {
        arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect()
    }

    #[test]
    fn every_shipped_wording_is_a_legacy_prompt() {
        for wording in [
            "Read @.st3/boot.md completely. Then list and claim your current st3 work.",
            "Read @.st3/boot.md completely. Then list, claim, do, and finish your current st3 work.",
            "Read @.st3/boot.md completely. Then list, claim, do, and finish your current st work.",
        ] {
            assert!(LEGACY_BOOT_PROMPTS.contains(&wording), "{wording}");
        }
    }

    #[test]
    fn a_stored_legacy_boot_prompt_is_removed_for_every_harness() {
        for prompt in LEGACY_BOOT_PROMPTS {
            for driver in ["claude", "codex", "pi", "omp"] {
                let mut stored = argv(&[driver, "--model", "m", prompt]);
                strip_legacy_prompt(driver, &mut stored);
                assert_eq!(
                    stored,
                    argv(&[driver, "--model", "m"]),
                    "{driver}: {prompt}"
                );
            }
        }
    }

    #[test]
    fn an_opencode_prompt_option_carrying_the_boot_prompt_is_removed() {
        for prompt in LEGACY_BOOT_PROMPTS {
            let mut opencode = argv(&["opencode", "--model", "m", "--prompt", prompt]);
            strip_legacy_prompt("opencode", &mut opencode);
            assert_eq!(opencode, argv(&["opencode", "--model", "m"]), "{prompt}");

            let composed = format!("Do the task.\n\n{prompt}");
            let mut opencode = argv(&["opencode", "--model", "m", "--prompt", &composed]);
            strip_legacy_prompt("opencode", &mut opencode);
            assert_eq!(opencode, argv(&["opencode", "--model", "m"]), "{prompt}");
        }
    }

    #[test]
    fn an_authored_prompt_composed_with_the_boot_prompt_is_removed() {
        for prompt in LEGACY_BOOT_PROMPTS {
            let composed = format!("Do the task.\n\n{prompt}");
            for driver in ["claude", "codex", "pi", "omp"] {
                let mut stored = argv(&[driver, "--thinking", "low", &composed]);
                strip_legacy_prompt(driver, &mut stored);
                assert_eq!(
                    stored,
                    argv(&[driver, "--thinking", "low"]),
                    "{driver}: {prompt}"
                );
            }
        }
    }

    #[test]
    fn a_codex_resume_argv_loses_the_boot_prompt_but_keeps_its_selection() {
        // The driver strips before the Codex app-server path inserts its automatic
        // `resume <thread>`, so only an authored resume can precede a stored prompt.
        for prompt in LEGACY_BOOT_PROMPTS {
            let mut stored = argv(&["codex", "--model", "m", "resume", "0199", prompt]);
            strip_legacy_prompt("codex", &mut stored);
            assert_eq!(
                stored,
                argv(&["codex", "--model", "m", "resume", "0199"]),
                "{prompt}"
            );
        }
    }

    #[test]
    fn an_argv_without_the_legacy_prompt_is_unchanged() {
        let [.., current] = LEGACY_BOOT_PROMPTS else {
            unreachable!("at least one legacy prompt");
        };
        let unseparated = format!("Do the task. {current}");
        for arguments in [
            argv(&["claude", "--model", "opus"]),
            argv(&["codex", "resume", "0199"]),
            argv(&["opencode", "--prompt", "person text"]),
            argv(&["pi", "Read @.st3/boot.md completely."]),
            argv(&["pi", &unseparated]),
            argv(&["claude", current, "--model", "opus"]),
        ] {
            let mut unchanged = arguments.clone();
            strip_legacy_prompt("opencode", &mut unchanged);
            assert_eq!(unchanged, arguments);
        }
    }
}
