//! Fixture tools that need the store. The process tools (temporary directories, the fixture shell,
//! command wrappers) live in st3-secrets and are re-exported here.

pub use st3_secrets::test_support::*;

use std::sync::OnceLock;

/// Exercise the real Store admission in isolated integration fixtures, without an API bypass
/// in the installed CLI. This module exists only for test/test-support builds.
pub fn bind_fixture_mailbox(store: &crate::store::Store, fence: &crate::mailbox::Fence)
    -> Result<crate::mailbox::Fence, crate::St3Error>
{
    store.bind_mailbox(fence)
}

pub fn check_fixture_mailbox(store: &crate::store::Store, fence: &crate::mailbox::Fence)
    -> Result<(), crate::St3Error>
{
    store.check_mailbox(fence)
}

/// Drive real roster admission with isolated spans, without exposing a daemon control.
pub async fn admit_fixture_roster(store: &crate::store::Store, class: &'static str) -> impl Send {
    store.admit_agent_resources(class).await
}

/// Try the same labeled admission used by production multi-gate readers.
pub fn try_admit_fixture_roster(
    store: &crate::store::Store,
    class: &'static str,
) -> Option<impl Send> {
    store.try_admit_agent_resources(class)
}

/// Force a synchronous holder handoff at the real waiter's enqueue boundary.
pub async fn admit_fixture_roster_after_handoff(
    store: &crate::store::Store,
    class: &'static str,
    before_enqueue: impl FnOnce(),
) -> impl Send {
    store.admit_agent_resources_before_enqueue(class, before_enqueue).await
}

/// Only this fixture thread records; production exporter state is not changed.
pub fn roster_export_scope() -> impl Drop {
    crate::otel::fixture_export_scope()
}

#[cfg(feature = "test-support")]
type MailboxTransportControls = std::sync::Mutex<
    std::collections::BTreeMap<(usize, String), tokio::sync::watch::Sender<bool>>,
>;

#[cfg(feature = "test-support")]
fn mailbox_transports() -> &'static MailboxTransportControls {
    static CONTROLS: OnceLock<MailboxTransportControls> = OnceLock::new();
    CONTROLS.get_or_init(Default::default)
}

/// Process-local transport loss in an invented integration fixture. No API route,
/// CLI flag or environment variable can select a production seat or activate it.
#[cfg(feature = "test-support")]
pub fn hold_fixture_mailbox(store: &std::sync::Arc<crate::store::Store>, subject: &str, held: bool) {
    assert!(subject.starts_with("agent/eval."), "mailbox loss controls require an invented fixture seat");
    mailbox_transports().lock().unwrap()
        .entry((std::sync::Arc::as_ptr(store) as usize, subject.into()))
        .or_insert_with(|| tokio::sync::watch::channel(false).0).send_replace(held);
}

#[cfg(feature = "test-support")]
pub fn fixture_mailbox_transport(store: &std::sync::Arc<crate::store::Store>, subject: &str) -> tokio::sync::watch::Receiver<bool> {
    mailbox_transports().lock().unwrap()
        .entry((std::sync::Arc::as_ptr(store) as usize, subject.into()))
        .or_insert_with(|| tokio::sync::watch::channel(false).0).subscribe()
}
