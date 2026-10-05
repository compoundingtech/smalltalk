//! Local, disposable routing of mailbox wakes. Durable claims remain the source of truth.
//! One dispatcher follows the existing post-commit feed, instead of every stream querying it.
use super::*;
use tokio::sync::watch;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum Dependency {
    Subject(String),
    Recipient(String),
    Owner(String, String),
}

#[derive(Default)]
struct Registry {
    next: u64,
    subscribers: HashMap<Dependency, HashMap<u64, watch::Sender<u64>>>,
}

#[derive(Default)]
pub(crate) struct Wakes {
    registry: Mutex<Registry>,
}

pub(crate) struct Subscription {
    wakes: Arc<Wakes>,
    id: u64,
    sender: watch::Sender<u64>,
    pub(crate) changed: watch::Receiver<u64>,
    base: HashSet<Dependency>,
    dependencies: HashSet<Dependency>,
}

impl Wakes {
    fn subscribe(self: &Arc<Self>, fence: &crate::mailbox::Fence) -> Subscription {
        let base = HashSet::from([
            Dependency::Subject(fence.subject.clone()),
            Dependency::Recipient(normalize_message_party(&fence.subject)),
            Dependency::Owner(fence.subject.clone(), fence.component.clone()),
        ]);
        let (sender, changed) = watch::channel(0);
        let mut registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        registry.next += 1;
        let id = registry.next;
        for key in &base {
            registry
                .subscribers
                .entry(key.clone())
                .or_default()
                .insert(id, sender.clone());
        }
        Subscription {
            wakes: self.clone(),
            id,
            sender,
            changed,
            dependencies: base.clone(),
            base,
        }
    }

    fn notify(&self, dependencies: impl IntoIterator<Item = Dependency>) {
        let registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        let mut notified = HashSet::new();
        for key in dependencies {
            if let Some(subscribers) = registry.subscribers.get(&key) {
                for (id, sender) in subscribers {
                    if notified.insert(*id) {
                        sender.send_modify(|generation| *generation = generation.wrapping_add(1));
                    }
                }
            }
        }
    }

    pub(crate) fn owner_changed(&self, subject: &str, component: &str) {
        self.notify([Dependency::Owner(subject.into(), component.into())]);
    }
}

impl Subscription {
    /// Replace message dependencies under the same lock used by the dispatcher. The recipient
    /// dependency also routes every message mutation, covering a newly read message whose
    /// subject has not been registered yet.
    pub(crate) fn messages(&mut self, subjects: &[String]) {
        let next: HashSet<_> = self
            .base
            .iter()
            .cloned()
            .chain(subjects.iter().cloned().map(Dependency::Subject))
            .collect();
        let mut registry = self
            .wakes
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for key in self.dependencies.difference(&next) {
            remove(&mut registry, key, self.id);
        }
        for key in next.difference(&self.dependencies) {
            registry
                .subscribers
                .entry(key.clone())
                .or_default()
                .insert(self.id, self.sender.clone());
        }
        self.dependencies = next;
    }
}

fn remove(registry: &mut Registry, key: &Dependency, id: u64) {
    if let Some(subscribers) = registry.subscribers.get_mut(key) {
        subscribers.remove(&id);
        if subscribers.is_empty() {
            registry.subscribers.remove(key);
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut registry = self
            .wakes
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for key in &self.dependencies {
            remove(&mut registry, key, self.id);
        }
    }
}

impl Store {
    pub(crate) fn subscribe_mailbox(
        self: &Arc<Self>,
        fence: &crate::mailbox::Fence,
        events: &watch::Sender<u64>,
    ) -> Subscription {
        // Subscribe to the post-commit feed before establishing the dispatcher's cursor, and
        // register this stream before its first durable read. No replay-to-live gap.
        let wakes = self.smalltalk.mailbox_wakes.get_or_init(|| {
            let events = events.subscribe();
            let cursor = self.mailbox_wake_cursor().unwrap_or((0, 0));
            let weak = Arc::downgrade(self);
            let wakes = Arc::new(Wakes::default());
            let routed = wakes.clone();
            tokio::spawn(async move {
                dispatch(weak, routed, events, cursor).await;
            });
            wakes
        });
        wakes.subscribe(fence)
    }

    fn mailbox_wake_cursor(&self) -> Result<(u64, u64)> {
        let connection = self.readers.get();
        Ok(connection
            .prepare_cached(
                "SELECT COALESCE((SELECT MAX(store_index) FROM claims),0),
                    COALESCE((SELECT MAX(id) FROM local_observations),0)",
            )?
            .query_row([], |r| Ok((r.get(0)?, r.get(1)?)))?)
    }

    fn mailbox_wake_batch(
        &self,
        cursor: (u64, u64),
    ) -> Result<((u64, u64), HashSet<Dependency>, bool)> {
        let connection = self.readers.get();
        let mut dependencies = HashSet::new();
        let mut next = cursor;
        // Bound each read and route once per batch. Only new subjects are read; this cost does
        // not multiply by the number of mailbox streams, or by the size of the graph.
        let rows = connection.prepare_cached(
            "SELECT store_index,subject FROM claims WHERE store_index>?1 ORDER BY store_index LIMIT 1024"
        )?.query_map([cursor.0], |r| Ok((r.get::<_, u64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let graph_full = rows.len() == 1024;
        let mut messages = HashSet::new();
        for (index, subject) in rows {
            next.0 = index;
            if subject.starts_with("message/") {
                messages.insert(subject.clone());
            }
            dependencies.insert(Dependency::Subject(subject));
        }
        for subject in messages {
            // Include historical sent recipients and the current declaration. Retargeting
            // wakes both owners; subject subscribers see removals as well as additions.
            let recipients = connection.prepare_cached(
                "SELECT json_extract(body,'$.fields.to') FROM claims WHERE subject=?1 AND kind='message.sent'
                 UNION SELECT json_extract(child.value,'$.arguments[0]')
                 FROM desired,json_each(desired.body,'$.children') child
                 WHERE desired.subject=?1 AND desired.kind='message' AND json_extract(child.value,'$.name')='to'"
            )?.query_map([&subject], |r| r.get::<_, Option<String>>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for recipient in recipients.into_iter().flatten() {
                dependencies.insert(Dependency::Recipient(normalize_message_party(&recipient)));
            }
        }
        let rows = connection
            .prepare_cached(
                "SELECT id,subject FROM local_observations WHERE id>?1 ORDER BY id LIMIT 1024",
            )?
            .query_map([cursor.1], |r| {
                Ok((r.get::<_, u64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let local_full = rows.len() == 1024;
        for (id, subject) in rows {
            next.1 = id;
            dependencies.insert(Dependency::Subject(subject));
        }
        Ok((next, dependencies, graph_full || local_full))
    }
}

async fn dispatch(
    store: std::sync::Weak<Store>,
    wakes: Arc<Wakes>,
    mut events: watch::Receiver<u64>,
    mut cursor: (u64, u64),
) {
    loop {
        if events.changed().await.is_err() {
            return;
        }
        events.borrow_and_update();
        loop {
            let Some(store) = store.upgrade() else {
                return;
            };
            let result = tokio::task::spawn_blocking(move || {
                crate::profile::task("task mailbox-wake-dispatch", || {
                    store.mailbox_wake_batch(cursor)
                })
            })
            .await;
            let Ok(Ok((next, dependencies, more))) = result else {
                // Keep the old cursor on failure. Streams' full safety reads still recover.
                break;
            };
            wakes.notify(dependencies);
            cursor = next;
            if !more {
                break;
            }
        }
    }
}

#[cfg(test)]
impl Store {
    pub(crate) fn miss_mailbox_owner_for_test(&self, subject: &str, component: &str) {
        self.smalltalk
            .mailbox_wakes
            .get()
            .unwrap()
            .registry
            .lock()
            .unwrap()
            .subscribers
            .remove(&Dependency::Owner(subject.into(), component.into()));
    }
    pub(crate) fn miss_mailbox_recipient_for_test(&self, subject: &str) {
        self.smalltalk
            .mailbox_wakes
            .get()
            .unwrap()
            .registry
            .lock()
            .unwrap()
            .subscribers
            .remove(&Dependency::Recipient(normalize_message_party(subject)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mailbox::Fence;

    #[tokio::test]
    async fn unrelated_writes_do_not_wake_a_stream_and_dependencies_are_released() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&root.path().join("graph.db"), "node").unwrap());
        let events = watch::channel(0).0;
        let fence = Fence::new("agent/quartz", "boot", "delivery");
        let mut subscription = store.subscribe_mailbox(&fence, &events);
        let other =
            store.subscribe_mailbox(&Fence::new("agent/other", "boot", "delivery"), &events);
        let append = |subject: &str, to: &str| {
            store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "message.sent".into(),
                    actor: Some("person/fixture".into()),
                    fields: BTreeMap::from([
                        ("status".into(), json!("sent")),
                        ("from".into(), json!("person/fixture")),
                        ("to".into(), json!(to)),
                        ("content".into(), json!("A note.")),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            events.send_modify(|generation| *generation += 1);
        };
        append("message/other", "agent/other");
        let mut other_changed = other.changed.clone();
        tokio::time::timeout(std::time::Duration::from_secs(2), other_changed.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(
            !subscription.changed.has_changed().unwrap(),
            "unrelated durable writes must leave the stream asleep"
        );
        append("message/mine", "agent/quartz");
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            subscription.changed.changed(),
        )
        .await
        .unwrap()
        .unwrap();
        subscription.changed.borrow_and_update();
        subscription.messages(&["message/mine".into()]);
        // An owner replacement is local (no graph claim) and wakes only its component.
        let title = store.subscribe_mailbox(&Fence::new("agent/quartz", "boot", "title"), &events);
        let hub = subscription.wakes.clone();
        hub.owner_changed("agent/quartz", "delivery");
        assert!(subscription.changed.has_changed().unwrap());
        assert!(!title.changed.has_changed().unwrap());
        drop(subscription);
        drop(other);
        drop(title);
        assert!(hub.registry.lock().unwrap().subscribers.is_empty());
    }

    #[tokio::test]
    async fn replicated_mail_and_local_observations_wake_the_recipient() {
        let root = tempfile::tempdir().unwrap();
        let target = Arc::new(Store::open(&root.path().join("graph.db"), "target").unwrap());
        let source = Store::open_memory("source").unwrap();
        let events = watch::channel(0).0;
        let mut subscription =
            target.subscribe_mailbox(&Fence::new("agent/quartz", "boot", "delivery"), &events);
        source
            .append_claim(&ClaimInput {
                subject: "message/replicated".into(),
                kind: "message.sent".into(),
                actor: Some("person/fixture".into()),
                fields: BTreeMap::from([
                    ("status".into(), json!("sent")),
                    ("from".into(), json!("person/fixture")),
                    ("to".into(), json!("agent/quartz")),
                    ("content".into(), json!("A note.")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        target
            .import_replication("source", &source.export_replication(0).unwrap())
            .unwrap();
        events.send_modify(|generation| *generation += 1);
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            subscription.changed.changed(),
        )
        .await
        .unwrap()
        .unwrap();
        subscription.changed.borrow_and_update();
        target
            .append_claim(&ClaimInput {
                subject: "agent/quartz".into(),
                kind: "harness.observed".into(),
                actor: Some("agent/quartz".into()),
                fields: BTreeMap::from([
                    ("state".into(), json!("idle")),
                    ("driver".into(), json!("codex")),
                    ("incarnation_id".into(), json!("boot")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        events.send_modify(|generation| *generation += 1);
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            subscription.changed.changed(),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[test]
    fn routing_covers_declarations_lifecycle_and_local_observations() {
        let store = Store::open_memory("node").unwrap();
        let cursor = store.mailbox_wake_cursor().unwrap();
        store.apply(&crate::graph::parse_intent(
            "version 2\nmessage \"declared\" { from \"person/fixture\"; to \"quartz\"; content \"A note.\"; }\n", "fixture").unwrap(), &BTreeMap::from([("message/declared".into(), vec![])]), "fixture").unwrap();
        let (next, deps, _) = store.mailbox_wake_batch(cursor).unwrap();
        assert!(deps.contains(&Dependency::Recipient("agent/quartz".into())));
        store
            .append_claim(&ClaimInput {
                subject: "message/declared".into(),
                kind: "message.delivered".into(),
                actor: Some("agent/quartz".into()),
                fields: BTreeMap::from([("status".into(), json!("delivered"))]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let (next, deps, _) = store.mailbox_wake_batch(next).unwrap();
        assert!(deps.contains(&Dependency::Subject("message/declared".into())));
        assert!(deps.contains(&Dependency::Recipient("agent/quartz".into())));
        store
            .append_claim(&ClaimInput {
                subject: "agent/quartz".into(),
                kind: "harness.observed".into(),
                actor: Some("agent/quartz".into()),
                fields: BTreeMap::from([
                    ("state".into(), json!("idle")),
                    ("driver".into(), json!("codex")),
                    ("incarnation_id".into(), json!("boot")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let (_, deps, _) = store.mailbox_wake_batch(next).unwrap();
        assert!(deps.contains(&Dependency::Subject("agent/quartz".into())));
    }
}
