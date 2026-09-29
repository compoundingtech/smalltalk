//! st no longer starts a seat with a prompt: a started or restarted seat takes no turn until a
//! person types or a graph message is posted. Declarations published before that change still
//! carry the old boot prompt at the end of their stored launch argv, so every driver removes it
//! before it starts the provider.

/// The boot prompt that older st releases appended to every typed harness argv.
pub const LEGACY_BOOT_PROMPT: &str =
    "Read @.st3/boot.md completely. Then list, claim, do, and finish your current st work.";

/// Remove a legacy startup prompt from a typed harness argv. The prompt was the last argument,
/// alone or after an authored prompt (`AUTHORED\n\nBOOT`); OpenCode received it as `--prompt`.
/// Both forms go, because an authored startup prompt would also start a turn.
pub fn strip_legacy_prompt(driver: &str, argv: &mut Vec<String>) {
    let legacy = |argument: &str| {
        argument == LEGACY_BOOT_PROMPT || argument.ends_with(&format!("\n\n{LEGACY_BOOT_PROMPT}"))
    };
    if !argv.last().is_some_and(|argument| legacy(argument)) {
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
    fn a_stored_legacy_boot_prompt_is_removed_for_every_harness() {
        for driver in ["claude", "codex", "pi", "omp"] {
            let mut stored = argv(&[driver, "--model", "m", LEGACY_BOOT_PROMPT]);
            strip_legacy_prompt(driver, &mut stored);
            assert_eq!(stored, argv(&[driver, "--model", "m"]), "{driver}");
        }
        let mut opencode = argv(&["opencode", "--model", "m", "--prompt", LEGACY_BOOT_PROMPT]);
        strip_legacy_prompt("opencode", &mut opencode);
        assert_eq!(opencode, argv(&["opencode", "--model", "m"]));
    }

    #[test]
    fn an_authored_prompt_composed_with_the_boot_prompt_is_removed() {
        let composed = format!("Do the task.\n\n{LEGACY_BOOT_PROMPT}");
        let mut stored = argv(&["pi", "--thinking", "low", &composed]);
        strip_legacy_prompt("pi", &mut stored);
        assert_eq!(stored, argv(&["pi", "--thinking", "low"]));
    }

    #[test]
    fn an_argv_without_the_legacy_prompt_is_unchanged() {
        for arguments in [
            argv(&["claude", "--model", "opus"]),
            argv(&["codex", "resume", "0199"]),
            argv(&["opencode", "--prompt", "person text"]),
            argv(&["pi", "Read @.st3/boot.md completely."]),
        ] {
            let mut unchanged = arguments.clone();
            strip_legacy_prompt("opencode", &mut unchanged);
            assert_eq!(unchanged, arguments);
        }
    }
}
