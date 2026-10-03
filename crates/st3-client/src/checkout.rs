/// The simple seat name, restricted to characters that always form a Git branch component.
pub fn agent_branch(name: &str) -> String {
    let identity = name.trim_start_matches("agent/");
    let name = identity.rsplit('/').next().unwrap_or(identity);
    let name = if identity.contains('/') {
        name
    } else {
        name.split_once('.').map_or(name, |(_, simple)| simple)
    };
    let branch: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let branch = branch.trim_matches('-');
    if branch.is_empty() {
        "agent".into()
    } else {
        branch.into()
    }
}

/// The worktree context clients show beside an agent's conversation.
pub fn checkout_label(checkout: &crate::AgentCheckout) -> String {
    format!(
        "worktree · branch {} · {}",
        checkout.branch, checkout.repository
    )
}
