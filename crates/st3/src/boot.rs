pub const BOOT_PROMPT: &str =
    "Read @.st3/boot.md completely. Then list, claim, do, and finish your current st3 work.";

pub const BOOT_DOCUMENT: &str = r#"# st3 boot

The st3 graph is the authority for current work. This file contains stable runtime guidance only.

`ST3_BIN` is the exact st3 executable that started your harness. Use `"$ST3_BIN"` for every st3 command.

Run `"$ST3_BIN" --help`, `"$ST3_BIN" work --help`, and `"$ST3_BIN" conversations --help` before you need an unfamiliar command.

Read each normalized conversation message before you act on it. Archive the message after you complete its related action.

An ST3 delivery begins `[PING from st3] message/ID from SENDER: TITLE`, followed by a bounded
body preview. Read the exact message with `"$ST3_BIN" conversations read message/ID --as "$ST_AGENT"`.
`SENDER` identifies who sent it; your own mailbox is `"$ST_AGENT"`, not the sender's mailbox.
Reply in its thread with `"$ST3_BIN" conversations reply message/ID --from "$ST_AGENT" --body "..."`.
The message ID, not the preview text, identifies the message to read, reply to, and archive.

A notification does not create work. Repeated delivery does not authorize repeated work.

Run `"$ST3_BIN" work ls` to list work that is available to you. Claim one eligible step.

Do not keep substantive work only in this conversation or a private todo. Before starting new work
authorized by a person, make sure the graph exposes it as active work across the fleet, then claim
that work. If you cannot create or claim the graph work with your authority, request person action.

If no step is ready, finish this turn. Do not wait for work that is not ready.

Do its work and finish the step in the same turn when possible.

The claim output contains the step goals and all effective constraints. A parent claim can expose nested mission steps.

Use `"$ST3_BIN" work progress` only for a material update. Finish with `"$ST3_BIN" work complete`, `"$ST3_BIN" work fail`, or `"$ST3_BIN" work release`.

Use `"$ST3_BIN" trace wait ... --as "$ST_AGENT"` only when claimed work needs a graph condition. Identity is always explicit; do not use an agent turn to poll.

The wait command exits early when a new message or a new eligible step needs your attention.

Native conversation delivery and graph work dispatch are the ordinary wake paths. Never type
into, attach to, or send synthetic keys such as Enter to an agent terminal to deliver a message
or wake work.

Terminal control is an emergency recovery path only after native delivery retries have failed and
diagnostics identify the exact current incarnation. Before using it, verify that no person has a
draft in the terminal, use an explicit person or operator identity, and record why it was necessary
and what happened.

When the harness itself fails, run `"$ST3_BIN" diagnostic --help` and report it through that dedicated authorized operation.

When a person must act, run `"$ST3_BIN" attention request --help` and publish one explicit request for the responsible person.
"#;

pub fn compose_prompt(authored: Option<&str>) -> String {
    let normalized = authored.unwrap_or_default().replace(BOOT_PROMPT, "");
    let authored = normalized.trim();
    if authored.is_empty() {
        return BOOT_PROMPT.into();
    }
    format!("{authored}\n\n{BOOT_PROMPT}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_prompt_is_present_exactly_once() {
        assert_eq!(compose_prompt(None), BOOT_PROMPT);
        assert_eq!(
            compose_prompt(Some("Do the task.")),
            format!("Do the task.\n\n{BOOT_PROMPT}")
        );
        assert_eq!(
            compose_prompt(Some(&format!("Do the task.\n\n{BOOT_PROMPT}"))),
            format!("Do the task.\n\n{BOOT_PROMPT}")
        );
        assert_eq!(
            compose_prompt(Some(&format!(
                "{BOOT_PROMPT}\n\nDo the task.\n\n{BOOT_PROMPT}"
            ))),
            format!("Do the task.\n\n{BOOT_PROMPT}")
        );
        assert!(BOOT_PROMPT.contains("claim, do, and finish"));
        assert!(BOOT_DOCUMENT.contains("[PING from st3] message/ID"));
        assert!(BOOT_DOCUMENT.contains("conversations reply message/ID"));
    }

    #[test]
    fn the_boot_document_continues_after_a_claim() {
        assert!(
            BOOT_DOCUMENT
                .contains("Do its work and finish the step in the same turn when possible.")
        );
        assert!(BOOT_DOCUMENT.contains("ST3_BIN"));
        assert!(BOOT_DOCUMENT.contains("If no step is ready, finish this turn."));
        assert!(BOOT_DOCUMENT.contains("graph exposes it as active work across the fleet"));
        assert!(BOOT_DOCUMENT.contains("Do not keep substantive work only in this conversation"));
        assert!(BOOT_DOCUMENT.contains("trace wait ... --as \"$ST_AGENT\"` only when"));
        assert!(BOOT_DOCUMENT.contains("conversations --help"));
        assert!(!BOOT_DOCUMENT.contains("message --help"));
        assert!(BOOT_DOCUMENT.contains("attention request --help"));
        assert!(BOOT_DOCUMENT.contains("diagnostic --help"));
        assert!(BOOT_DOCUMENT.contains("Never type\ninto, attach to, or send synthetic keys"));
        assert!(BOOT_DOCUMENT.contains("Terminal control is an emergency recovery path only"));
    }
}
