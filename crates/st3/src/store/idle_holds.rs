//! What the idle-hold nudge reads about a seat: whether mail waits for it, the GitHub watches it
//! holds, when each watch last woke it, whether a watched thread left the merge queue without
//! merging, the runs that report to it, and the nudges its steps already got. Each read is an
//! indexed lookup bounded by one seat's mailbox or watches, the open runs, or one thread's
//! observations since its seat was last woken.

use super::*;
use crate::github_watch::{self, ThreadRef};

/// The claim recorded on a step each time its holder is nudged.
pub const WORK_NUDGED_KIND: &str = "work.nudged";

/// How many of a thread's newest observations a merge-queue check reads at most. A thread is
/// observed only when it changes.
const THREAD_SCAN: u32 = 400;

/// One live GitHub watch a seat holds.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LiveWatch {
    pub subject: String,
    pub thread: ThreadRef,
    pub since_unix_ms: u128,
    pub until_unix_ms: Option<u128>,
}

impl Store {
    /// Whether a message to `agent` has not reached its harness yet: sent or staged, with no
    /// delivery, read or close.
    pub(crate) fn has_undelivered_message(&self, agent: &str) -> Result<bool> {
        let (recipient, bare_recipient) = recipients(agent);
        smallclaims::touched::note_read(|| format!("mailbox:{recipient}"));
        let connection = self.readers.get();
        let found: Option<i64> = connection
            .prepare_cached(
                "SELECT 1 FROM claims sent INDEXED BY claims_message_to_index
                 WHERE sent.kind='message.sent'
                   AND json_extract(sent.body, '$.fields.to') IN (?1, ?2)
                   AND NOT EXISTS (
                     SELECT 1 FROM claims later
                     WHERE later.subject=sent.subject
                       AND later.kind IN ('message.delivered', 'message.read', 'message.closed'))
                 LIMIT 1",
            )?
            .query_row(params![recipient, bare_recipient], |row| row.get(0))
            .optional()?;
        Ok(found.is_some())
    }

    /// When st last sent `agent` a message whose body names every one of `fragments`, such as a
    /// watch's wakes (tagged with the watch) or a run's reports.
    pub(crate) fn last_message_to(&self, agent: &str, fragments: &[&str]) -> Result<Option<u128>> {
        let (recipient, bare_recipient) = recipients(agent);
        smallclaims::touched::note_read(|| format!("mailbox:{recipient}"));
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT sent.body, sent.accepted_at_unix_ms
             FROM claims sent INDEXED BY claims_message_to_index
             WHERE sent.kind='message.sent'
               AND json_extract(sent.body, '$.fields.to') IN (?1, ?2)
               AND instr(sent.body, ?3)>0",
        )?;
        let first = fragments.first().copied().unwrap_or_default();
        let mut latest = None::<u128>;
        for row in statement.query_map(params![recipient, bare_recipient, first], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (body, at) = row?;
            if fragments.iter().all(|fragment| body.contains(fragment))
                && let Ok(at) = at.parse::<u128>()
            {
                latest = Some(latest.map_or(at, |latest| latest.max(at)));
            }
        }
        Ok(latest)
    }

    /// The watches `agent` holds that have not ended. Watch subjects end with their seat, so
    /// this reads the declared watches' subject range, not the fleet's declarations.
    pub(crate) fn live_watches_of(&self, agent: &str) -> Result<Vec<LiveWatch>> {
        let suffix = format!("/{}", agent.trim_start_matches("agent/"));
        let subjects = {
            let connection = self.readers.get();
            let mut statement = connection.prepare_cached(
                "SELECT subject FROM desired
                 WHERE subject >= 'subscription/watch/' AND subject < 'subscription/watch0'
                   AND substr(subject, -length(?1)) = ?1",
            )?;
            statement
                .query_map([&suffix], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut watches = Vec::new();
        for subject in subjects {
            let Some((thread, owner)) = github_watch::watch_parts(&subject) else {
                continue;
            };
            if owner != agent {
                continue;
            }
            if let Some((_, watch)) = self.live_watch(&subject)? {
                watches.push(LiveWatch {
                    subject,
                    thread,
                    since_unix_ms: watch.since_unix_ms,
                    until_unix_ms: watch.until_unix_ms,
                });
            }
        }
        Ok(watches)
    }

    /// When a watched thread that is still open left the merge queue, if it is out of the queue
    /// now and left it after `after_unix_ms`: the first observation that saw it out.
    pub(crate) fn thread_left_merge_queue(
        &self,
        thread: &ThreadRef,
        after_unix_ms: u128,
    ) -> Result<Option<u128>> {
        let resource = self
            .desired_subjects_named(&[thread.observer()])?
            .into_iter()
            .next()
            .and_then(|observer| crate::graph::observer_spec(&observer.desired))
            .map_or_else(|| thread.resource(), |spec| spec.resource);
        let item = format!("{resource}/pull-request/{}", thread.number);
        smallclaims::touched::note_read(|| item.clone());
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT json_extract(body, '$.fields.facts'), accepted_at_unix_ms FROM claims
             WHERE subject=?1 AND kind='resource.observed'
               AND COALESCE(json_extract(body, '$.fields.attribution_only'), 0)=0
             ORDER BY length(accepted_at_unix_ms) DESC, accepted_at_unix_ms DESC, store_index DESC
             LIMIT ?2",
        )?;
        let mut rows = statement.query_map(params![item, THREAD_SCAN], |row| {
            Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?))
        })?;
        // Newest first: while the thread is out of the queue, the oldest such observation is when
        // it left; the one before that must have it queued.
        let mut left = None::<u128>;
        for row in &mut rows {
            let (facts, at) = row?;
            let facts = facts
                .map(|facts| serde_json::from_str::<Value>(&facts))
                .transpose()?
                .unwrap_or(Value::Null);
            let at = at.parse::<u128>().unwrap_or_default();
            if left.is_none() && !github_watch::open_outside_merge_queue(&facts) {
                return Ok(None);
            }
            if github_watch::in_merge_queue(&facts) {
                return Ok(left.filter(|left| *left > after_unix_ms));
            }
            if !github_watch::open_outside_merge_queue(&facts) || at <= after_unix_ms {
                return Ok(None);
            }
            left = Some(at);
        }
        Ok(None)
    }

    /// The runs that have not finished, by subject: a few of every run a fleet has made, read
    /// through the open-runs index.
    pub(crate) fn open_mission_runs(&self) -> Result<Vec<String>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(&format!(
            "SELECT id FROM mission_runs WHERE {OPEN_MISSION_RUN} ORDER BY created_at_unix_ms, id"
        ))?;
        let runs = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .map(|id| id.map(|id| format!("mission-run/{id}")))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(runs)
    }

    /// A step's newest nudge.
    pub(crate) fn latest_nudge(&self, step: &str) -> Result<Option<ClaimRecord>> {
        Ok(self.latest_claim(step, Some(WORK_NUDGED_KIND))?)
    }

    /// The newest `harness.observed` of a seat, for its quiescence report.
    pub(crate) fn latest_harness_report(&self, agent: &str) -> Result<Option<Value>> {
        Ok(self
            .latest_claim(agent, Some("harness.observed"))?
            .map(|claim| claim.body.get("fields").cloned().unwrap_or(claim.body)))
    }
}

/// A seat's mailbox names, as messages address it.
fn recipients(agent: &str) -> (String, String) {
    let recipient = normalize_message_party(agent);
    let bare_recipient = recipient
        .strip_prefix("agent/")
        .filter(|suffix| !suffix.contains('/'))
        .unwrap_or(&recipient)
        .to_owned();
    (recipient, bare_recipient)
}
