//! One agent metric and one system metric per machine, from two st reads:
//! `machines.list` (graph-derived, includes `occupancy.running_runtimes`) and
//! `host-facts.read` (sampled by the answering member on each read).

use crate::{Card, CardKind, Tone};
use st3_client::{HostFacts, HostLoad, Machine};

/// Clients read host facts at this cadence while a metrics surface is visible.
pub const POLL_INTERVAL_MS: u64 = 2_000;
/// A sample older than three polls is shown as stale rather than current.
pub const STALE_AFTER_MS: u64 = 3 * POLL_INTERVAL_MS;

/// The cards for `machines`, in their order: agents running, then load, for each machine.
/// `facts` is the answering member's sample; it describes at most one machine, matched by
/// `host_id`. Other machines' load is unknown until st relays their facts.
pub fn cards(machines: &[Machine], facts: Option<&HostFacts>, now_ms: i64) -> Vec<Card> {
    let mut cards = Vec::with_capacity(machines.len() * 2);
    for machine in machines {
        let subject = machine.header.id.clone();
        let reachable = matches!(machine.state.as_str(), "local" | "reachable");
        cards.push(Card {
            id: format!("{subject}#agents-running"),
            kind: CardKind::AgentsRunning,
            subject: subject.clone(),
            label: "agents".into(),
            value: format!("{} running", machine.occupancy.running_runtimes),
            tone: if reachable { Tone::Normal } else { Tone::Stale },
        });
        let sample = facts.filter(|facts| facts.host_id == machine.host_id);
        let (value, tone) = match sample.map(|facts| (&facts.load, age_ms(facts, now_ms))) {
            None => ("not reported".into(), Tone::Unknown),
            Some((_, None)) => ("unreadable time".into(), Tone::Unknown),
            Some((HostLoad::Unknown { reason }, _)) => (reason.clone(), Tone::Unknown),
            Some((HostLoad::Reported { one_minute, cpus }, Some(age))) => (
                format!("{one_minute:.2} / {cpus} cpu"),
                if age > STALE_AFTER_MS as i64 {
                    Tone::Stale
                } else if *one_minute >= f64::from(*cpus) {
                    Tone::Busy
                } else {
                    Tone::Normal
                },
            ),
        };
        cards.push(Card {
            id: format!("{subject}#load-1m"),
            kind: CardKind::HostLoad,
            subject,
            label: "load 1m".into(),
            value,
            tone,
        });
    }
    cards
}

/// Milliseconds since the sample, or `None` when its time is unreadable. A sample from the
/// future (clock skew) counts as current, never as negative age.
fn age_ms(facts: &HostFacts, now_ms: i64) -> Option<i64> {
    let observed = chrono::DateTime::parse_from_rfc3339(&facts.observed_at).ok()?;
    Some((now_ms - observed.timestamp_millis()).max(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Vector {
        name: String,
        now_ms: i64,
        machines: Vec<Machine>,
        facts: Option<HostFacts>,
        cards: Vec<Card>,
    }

    /// The web kit's TypeScript projection checks itself against these same vectors.
    #[test]
    fn cards_match_the_shared_client_vectors() {
        let vectors: Vec<Vector> = serde_json::from_str(include_str!(
            "../../../fixtures/clients/metric-cards.json"
        ))
        .unwrap();
        assert!(vectors.len() >= 4);
        for vector in vectors {
            assert_eq!(
                cards(&vector.machines, vector.facts.as_ref(), vector.now_ms),
                vector.cards,
                "{}",
                vector.name
            );
        }
    }
}
