//! In-process input for native protocol pumps. st owns the durable mailbox; these queues contain
//! no files and are repopulated by a daemon subscription after reexec. st2 retains its file bus.
use crate::message::Message;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, mpsc};

#[derive(Default)]
struct Queue {
    messages: Option<Vec<Message>>,
    active: BTreeSet<String>,
    wakes: Vec<mpsc::Sender<()>>,
}
fn queues() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<Queue>>>> {
    static QUEUES: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<Queue>>>>> = OnceLock::new();
    QUEUES.get_or_init(Default::default)
}

pub fn register(agent_dir: &Path) {
    queues()
        .lock()
        .unwrap()
        .entry(agent_dir.to_owned())
        .or_default();
}
pub fn managed(agent_dir: &Path) -> bool {
    queues().lock().unwrap().contains_key(agent_dir)
}
pub fn replace(agent_dir: &Path, messages: Vec<Message>) {
    let active = messages
        .iter()
        .map(|message| message.filename.clone())
        .collect();
    replace_active(agent_dir, messages, active);
}
/// Mailbox membership is independent of body readiness: an unavailable document must never
/// prune an uncertain native handoff while other messages remain deliverable.
pub fn replace_active(agent_dir: &Path, messages: Vec<Message>, active: BTreeSet<String>) {
    let queue = queues()
        .lock()
        .unwrap()
        .get(agent_dir)
        .cloned()
        .expect("registered native mailbox");
    let mut queue = queue.lock().unwrap();
    queue.messages = Some(messages);
    queue.active = active;
    queue.wakes.retain(|wake| wake.send(()).is_ok());
}
pub fn watch(agent_dir: &Path, wake: mpsc::Sender<()>) {
    if let Some(queue) = queues().lock().unwrap().get(agent_dir).cloned() {
        queue.lock().unwrap().wakes.push(wake);
    }
}
pub fn messages(agent_dir: &Path, inbox: &Path) -> anyhow::Result<Vec<Message>> {
    let queue = queues().lock().unwrap().get(agent_dir).cloned();
    match queue {
        Some(queue) => queue
            .lock()
            .unwrap()
            .messages
            .clone()
            .ok_or_else(|| anyhow::anyhow!("waiting for the daemon mailbox replay")),
        None => crate::message::list_inbox(inbox),
    }
}
pub fn is_unread(agent_dir: &Path, key: &str, messages: &[Message]) -> bool {
    match queues().lock().unwrap().get(agent_dir).cloned() {
        Some(queue) => queue.lock().unwrap().active.contains(key),
        None => messages.iter().any(|message| message.filename == key),
    }
}

pub fn is_delivery_key(key: &str) -> bool {
    crate::message::is_message_filename(key)
        || key.strip_prefix("message/").is_some_and(|id| {
            !id.is_empty()
                && id.len() <= 256
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replay_and_unavailable_bodies_cannot_prune_uncertain_native_handoffs() {
        let root = tempfile::tempdir().unwrap();
        register(root.path());
        assert!(messages(root.path(), &root.path().join("resources/inbox")).is_err());
        replace_active(
            root.path(),
            Vec::new(),
            BTreeSet::from(["message/quartz".into()]),
        );
        assert!(
            messages(root.path(), &root.path().join("resources/inbox"))
                .unwrap()
                .is_empty()
        );
        assert!(is_unread(root.path(), "message/quartz", &[]));
        replace(root.path(), Vec::new());
        assert!(!is_unread(root.path(), "message/quartz", &[]));
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}
