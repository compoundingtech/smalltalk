//! Operator causes for failed Codex turns. A terminal thread status does not mean the
//! app-server exited; these are the causes retained from its turn result or rollout.

pub fn failure_detail(driver: &str, state: &str, reason: Option<&str>) -> Option<&'static str> {
    if driver != "codex" || !matches!(state, "ended" | "working" | "idle") {
        return None;
    }
    Some(match reason? {
        "policy" => {
            "Codex's provider refused the turn under its cyber policy. Review the provider refusal and the task before resuming; st does not automatically retry policy refusals."
        }
        "providerAuth" => {
            "Codex's provider rejected the credential. Restore the account's login before resuming."
        }
        "usageLimit" => {
            "Codex's provider reported a usage or rate limit. Wait for the allowance to recover before resuming."
        }
        "serverOverloaded" => {
            "The selected Codex model is at capacity. This failed turn needs a capacity retry in the existing session."
        }
        "providerCapacity" => {
            "The selected Codex model is temporarily at capacity. st retries the existing session with a bounded backoff; inspect the provider-capacity diagnostic for its next retry or exhausted retry budget."
        }
        "contextWindow" => {
            "Codex's provider rejected the turn because its context window is full. Review the context before resuming."
        }
        "connection" => {
            "Codex lost its provider connection after the turn's retries. Inspect the connection failure before resuming."
        }
        "rejected" => {
            "Codex rejected the turn request. Inspect the request failure before resuming."
        }
        "internal" => {
            "Codex reported an internal turn failure. Inspect the producer error before resuming."
        }
        "systemError" | "unclassified" => {
            "Codex reported a failed turn whose cause this build could not classify. Inspect the turn error in the session rollout; systemError alone does not prove an app-server crash or model capacity."
        }
        _ => return None,
    })
}
