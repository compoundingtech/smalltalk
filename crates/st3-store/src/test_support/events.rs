//! Test-side waits on the daemon's event log, the Rust twin of `scripts/st3_events.py`
//! (design: doc/fleet/factory/test-speed/evented-design, section 2).
//!
//! Take a [`Mark`] for a subject before the action under test, then wait from it. The log keeps
//! every claim accepted after the mark, so a state that lasted a few milliseconds is still found.
//! Each wait is a long poll the daemon answers when it appends: no interval and no fixed
//! deadline. The ceiling only bounds failure. Waiting cannot fix a cursor gap or a daemon that is
//! gone, so both fail at once.
//!
//! A mark follows one subject. The daemon filters a page by subject after choosing its rows and
//! moves the cursor past all of them, so a cursor that followed one subject has skipped the other
//! subjects' events. To await several subjects, take one mark per subject, all before the action.

use std::collections::VecDeque;
use std::future::Future;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::client::Client;
use crate::model::EventRecord;

/// How many of the most recent events a ceiling failure prints.
const SHOWN: usize = 20;
/// The daemon's own bound on one long poll.
const LONGEST_POLL: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
struct Page {
    items: Vec<EventRecord>,
    next_after: Option<u64>,
}

#[derive(Deserialize)]
struct Health {
    store_index: u64,
}

/// A position in one subject's events, and the events seen since, for failure messages.
pub struct Mark {
    subject: String,
    cursor: u64,
    /// Fetched after the cursor but not yet consumed by a wait, in log order.
    pending: VecDeque<EventRecord>,
    seen: VecDeque<EventRecord>,
    polls: usize,
}

impl Mark {
    /// The daemon's current store index, for `subject`'s events. Take it before the action whose
    /// effect you await.
    pub async fn take(client: &Client, subject: &str) -> Result<Self> {
        let health: Health = client
            .get("/v1/health")
            .await
            .context("reading the daemon's store index for an event mark")?;
        Ok(Self {
            subject: subject.into(),
            cursor: health.store_index,
            pending: VecDeque::new(),
            seen: VecDeque::new(),
            polls: 0,
        })
    }

    /// The first event after the mark that `matches`. The mark moves past it, so the next wait
    /// continues with the event after it.
    pub async fn event(
        &mut self,
        client: &Client,
        matches: impl Fn(&EventRecord) -> bool,
        ceiling: Duration,
        what: &str,
    ) -> Result<EventRecord> {
        let deadline = tokio::time::Instant::now() + ceiling;
        loop {
            while let Some(event) = self.pending.pop_front() {
                if matches(&event) {
                    return Ok(event);
                }
            }
            self.page(client, deadline, what).await?;
        }
    }

    /// The first `Some` from `check`, a read of state the log does not carry directly. It checks
    /// once, then again only after a new event on the mark's subject. Every input of `check` must
    /// change through a claim on that subject: a state that changes without one is not
    /// rechecked. The events that woke a recheck are consumed, so a later [`Mark::event`]
    /// continues after them.
    pub async fn state<T, F, Fut>(
        &mut self,
        client: &Client,
        mut check: F,
        ceiling: Duration,
        what: &str,
    ) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<Option<T>>>,
    {
        let deadline = tokio::time::Instant::now() + ceiling;
        loop {
            if let Some(value) = check().await? {
                return Ok(value);
            }
            if self.pending.is_empty() {
                while !self.page(client, deadline, what).await? {}
            }
            self.pending.clear();
        }
    }

    /// How many long polls this mark has sent.
    pub fn polls(&self) -> usize {
        self.polls
    }

    /// One long poll from the cursor into `pending`; true when it brought events. An empty page
    /// still moves the cursor to `next_after`, because the daemon filters by subject after its
    /// row limit.
    async fn page(
        &mut self,
        client: &Client,
        deadline: tokio::time::Instant,
        what: &str,
    ) -> Result<bool> {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!("{what}: not seen within the ceiling\n{}", self.recent());
        }
        let wait = remaining.min(LONGEST_POLL).as_millis().max(10);
        let path = format!(
            "/v1/events/page?after={}&subject={}&wait=true&timeout_ms={wait}",
            self.cursor,
            urlencoding::encode(&self.subject),
        );
        self.polls += 1;
        let page: Page = match client.get(&path).await {
            Ok(page) => page,
            Err(error) => {
                if let Some((410, "cursor-gap", _, details)) =
                    crate::client::api_error_parts(&error)
                {
                    bail!(
                        "{what}: event cursor {} is outside the retained log ({}); a test must take its mark from this daemon",
                        self.cursor,
                        Value::Object(details.clone())
                    );
                }
                return Err(
                    error.context(format!("{what}: the daemon did not answer an event wait"))
                );
            }
        };
        if let Some(next) = page.next_after {
            self.cursor = next;
        }
        let fetched = !page.items.is_empty();
        for event in page.items {
            if self.seen.len() == SHOWN {
                self.seen.pop_front();
            }
            self.seen.push_back(event.clone());
            self.pending.push_back(event);
        }
        Ok(fetched)
    }

    fn recent(&self) -> String {
        let subject = &self.subject;
        if self.seen.is_empty() {
            return format!(
                "no events on {subject} after the mark (cursor {})",
                self.cursor
            );
        }
        let mut text = format!("last {} events on {subject}:", self.seen.len());
        for event in &self.seen {
            let body = event.body.to_string();
            let body: String = body.chars().take(400).collect();
            text.push_str(&format!("\n  {} {} {body}", event.store_index, event.kind));
        }
        text
    }
}

/// Match `kind` and every `fields` entry against the claim's fields.
pub fn claim<'a>(
    kind: &'a str,
    fields: &'a [(&'a str, Value)],
) -> impl Fn(&EventRecord) -> bool + 'a {
    move |event| {
        event.kind == kind
            && fields
                .iter()
                .all(|(name, value)| event.body.pointer(&format!("/fields/{name}")) == Some(value))
    }
}
