//! Statement budgets for a live mailbox stream's update, counted on the calling thread.
use super::*;
use smallclaims::sqlite::work::{SqliteWork, SqliteWorkScope};

struct Mailbox {
    store: Store,
    fence: Fence,
    floor: (u128, u64),
    admitted: std::collections::BTreeSet<String>,
    policies: std::collections::BTreeSet<String>,
    order: MailboxOrder,
    previous: Option<(crate::store::MailboxWatermark, Snapshot)>,
}

impl Mailbox {
    fn new() -> Self {
        let store = Store::open_memory("node").unwrap();
        crate::mailbox::tests::ready(&store, "session-1");
        let fence = store
            .bind_mailbox(&Fence::new("agent/eval.worker", "session-1", "delivery"))
            .unwrap();
        let floor = (client_now_ms(), store.index().unwrap());
        Self {
            store,
            fence,
            floor,
            admitted: Default::default(),
            policies: Default::default(),
            order: Default::default(),
            previous: None,
        }
    }

    fn append(&self, subject: &str, phase: &str, reply: Option<&str>) -> String {
        self.try_append(subject, phase, reply).unwrap()
    }

    /// A lifecycle claim; the graph refuses a transition the message has already passed.
    fn try_append(&self, subject: &str, phase: &str, reply: Option<&str>) -> anyhow::Result<String> {
        let mut fields = BTreeMap::from([("status".into(), json!(phase))]);
        if phase == "sent" {
            fields.insert("from".into(), json!("person/fixture"));
            fields.insert("to".into(), json!(self.fence.subject));
            fields.insert("content".into(), json!("A statement budget fixture."));
            if let Some(parent) = reply {
                fields.insert("in_reply_to".into(), json!(parent));
            }
        }
        self.store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: format!("message.{phase}"),
                actor: Some("person/fixture".into()),
                fields,
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some(format!("{subject}:{phase}")),
            })
            .map(|claim| claim.id)
            .map_err(Into::into)
    }

    /// One stream update, its statement cost, and whether it would send a frame.
    fn update(&mut self) -> (SqliteWork, bool) {
        let scope = SqliteWorkScope::start();
        let (mark, view, updated, _) = update_snapshot(
            &self.store,
            &self.fence,
            raw_snapshot,
            self.previous.take(),
            self.floor,
            &mut self.admitted,
            &mut self.policies,
            &mut self.order,
        )
        .unwrap();
        let cost = scope.finish();
        self.previous = Some((mark, view));
        (cost, updated)
    }

    /// The complete mailbox read through the same delivery policy, as a stream would see it
    /// if it reconnected now with this stream's admitted identities.
    fn oracle(&mut self) -> serde_json::Value {
        let mut oracle = snapshot(&self.store, &self.fence).unwrap();
        retain_live_mail(
            &self.store,
            &mut oracle.1,
            self.floor.0,
            Some(self.floor.1),
            &mut self.admitted,
        )
        .unwrap();
        MailboxOrder::default().bound_held(&mut oracle.1, true);
        serde_json::to_value(oracle).unwrap()
    }

    fn view(&self) -> serde_json::Value {
        serde_json::to_value(&self.previous.as_ref().unwrap().1).unwrap()
    }
}

#[derive(Debug)]
struct Costs {
    full: SqliteWork,
    change: SqliteWork,
    steady: SqliteWork,
    idle: SqliteWork,
}

fn costs(open: usize) -> Costs {
    let mut mailbox = Mailbox::new();
    for number in 0..open {
        mailbox.append(&format!("message/open-{number:03}"), "sent", None);
    }
    let (full, _) = mailbox.update();
    assert_eq!(mailbox.view(), mailbox.oracle());
    // One new message and one receipt on an old one arrive before the next cursor read.
    mailbox.append("message/new", "sent", None);
    mailbox.append("message/open-000", "staged", None);
    let (change, updated) = mailbox.update();
    assert!(updated);
    assert_eq!(mailbox.view(), mailbox.oracle());
    // Later changes reuse the order keys the first change read in one statement.
    mailbox.append("message/newer", "sent", None);
    mailbox.append("message/open-000", "delivered", None);
    let (steady, updated) = mailbox.update();
    assert!(updated);
    assert_eq!(mailbox.view(), mailbox.oracle());
    let (idle, updated) = mailbox.update();
    assert!(!updated);
    Costs { full, change, steady, idle }
}

#[test]
fn a_change_costs_the_same_statements_whatever_the_open_mailbox_size() {
    let sizes = [1, 8, 64];
    let measured = sizes.map(costs);
    for (open, cost) in sizes.iter().zip(&measured) {
        println!(
            "mailbox-update open={open} full={} change={} steady={} idle={} (vm change={} steady={})",
            cost.full.statements,
            cost.change.statements,
            cost.steady.statements,
            cost.idle.statements,
            cost.change.vm_steps,
            cost.steady.vm_steps,
        );
    }
    let [small, _, large] = &measured;
    assert_eq!(
        small.change.statements, large.change.statements,
        "a change must read only its changed identities: {measured:?}"
    );
    assert_eq!(small.steady.statements, large.steady.statements, "{measured:?}");
    assert_eq!(small.idle.statements, large.idle.statements, "{measured:?}");
    assert_eq!(large.change.fullscan_steps, 0, "{measured:?}");
}

/// The kept order keys and the once-per-update hold must not change what a stream shows:
/// after every update, the incremental mailbox equals a complete read of the same graph.
#[test]
fn incremental_updates_with_kept_order_match_a_full_read_at_every_step() {
    let mut mailbox = Mailbox::new();
    let (_, _) = mailbox.update();
    // A fixed pseudo-random schedule: sends (some replies), receipts in any order, closes,
    // several changes coalesced into one update, and a global repair that forces a resync.
    let mut seed = 0x5eed_u64;
    let mut next = |bound: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % bound
    };
    let mut sent = Vec::<String>::new();
    let mut last = String::new();
    for round in 0..40 {
        for _ in 0..=next(3) {
            let choice = next(10);
            if sent.is_empty() || choice < 4 {
                let subject = format!("message/parity-{:03}", sent.len());
                let reply = (!sent.is_empty() && choice == 0)
                    .then(|| sent[next(sent.len() as u64) as usize].clone());
                last = mailbox.append(&subject, "sent", reply.as_deref());
                sent.push(subject);
            } else {
                let subject = sent[next(sent.len() as u64) as usize].clone();
                let phase = ["staged", "delivered", "read", "closed"][next(4) as usize];
                let _ = mailbox.try_append(&subject, phase, None);
            }
        }
        if round == 20 {
            mailbox
                .store
                .append_claim(&ClaimInput {
                    subject: "repair/parity-fixture".into(),
                    kind: "record.repaired".into(),
                    actor: Some("person/fixture".into()),
                    fields: BTreeMap::from([
                        ("record".into(), json!("record/parity-fixture")),
                        ("replacement".into(), json!(last)),
                        ("reason".into(), json!("A parity resync control.")),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        mailbox.update();
        assert_eq!(mailbox.view(), mailbox.oracle(), "round {round}");
        let (_, updated) = mailbox.update();
        assert!(!updated, "round {round}: an idle update must not produce a frame");
    }
    assert!(
        mailbox.order.keys.len() == mailbox.previous.as_ref().unwrap().1.1.len()
            || mailbox.order.keys.is_empty(),
        "kept keys describe only the current mailbox"
    );
}

#[test]
fn incremental_held_backlog_above_eight_matches_the_bounded_full_oracle() {
    let mut mailbox = Mailbox::new();
    mailbox.update();
    for number in 0..20 {
        mailbox
            .store
            .append_claim(&ClaimInput {
                subject: format!("message/held-{number:03}"),
                kind: "message.sent".into(),
                actor: Some("agent/example/writer".into()),
                fields: BTreeMap::from([
                    ("status".into(), json!("sent")),
                    ("from".into(), json!("agent/example/writer")),
                    ("to".into(), json!(mailbox.fence.subject)),
                    ("content".into(), json!("held")),
                    ("tags".into(), json!([crate::fyi::FYI_TAG])),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    mailbox.update();
    assert_eq!(mailbox.view(), mailbox.oracle());
    assert_eq!(mailbox.previous.as_ref().unwrap().1.1.len(), 8);
    mailbox.append("message/waking", "sent", None);
    mailbox.update();
    assert_eq!(mailbox.view(), mailbox.oracle());
}
