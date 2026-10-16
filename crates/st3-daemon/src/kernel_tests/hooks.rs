use crate::hooks::*;

/// Claude ends a hook at its timeout, so the permission hook's timeout outlasts the wait for a
/// person's answer: an answer st accepts from a client always finds its hook.
#[test]
fn the_permission_hook_outlasts_the_wait_for_an_answer() {
    let settings = claude_settings_registration();
    let timeout_ms = settings["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"]
        .as_u64()
        .unwrap() as u128
        * 1000;
    assert!(timeout_ms > crate::store::CLAUDE_PROMPT_ANSWER_WAIT_MS);
}
