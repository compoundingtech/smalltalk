//! Fresh fences and unique action keys for terminal viewers.

use anyhow::Result;
use st3_client::{Client, Fence};

/// A unique action ID and matching idempotency key for one terminal operation.
pub fn action_pair() -> (String, String) {
    let id = format!("action/{}", uuid::Uuid::now_v7());
    (id.clone(), id)
}
/// Read the terminal owner's current sequence and reject a changed runtime incarnation.
pub async fn terminal_fence(
    client: &Client,
    terminal_id: &str,
    incarnation: &str,
) -> Result<Fence> {
    let screen = client.terminal_screen(terminal_id).await?;
    terminal_screen_fence(&screen, incarnation)
}
/// Build a fence from the owner's screen sequence, including when the screen was relayed.
pub fn terminal_screen_fence(
    screen: &st3_client::Envelope<st3_client::TerminalScreen>,
    incarnation: &str,
) -> Result<Fence> {
    anyhow::ensure!(
        screen.value.runtime_incarnation == incarnation,
        "terminal incarnation changed; reattach before sending input"
    );
    Ok(Fence {
        snapshot_id: screen.snapshot.id.clone(),
        runtime_incarnation: Some(incarnation.to_owned()),
        terminal_sequence: Some(screen.value.next_sequence),
        ..Fence::default()
    })
}
