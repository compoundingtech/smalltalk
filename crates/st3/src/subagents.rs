//! A seat driver's record of its harness's subagents.
//!
//! The harness's hooks or events keep `st_drivers::subagents`'s ledger beside the seat's other
//! harness files. Each driver tick turns what changed into claims on the seat: a
//! `subagent.appeared` for each new subagent, with the step the seat holds, a `subagent.renewed`
//! once half of a running subagent's lease has passed, and a `subagent.ended` with its tokens
//! for each end. A subagent leaves the ledger only once its end is recorded, so an st outage
//! delays these claims and loses none. The store answers a repeated appearance or end with the
//! claim it has, so a retry after a lost answer records nothing twice.
//!
//! Claude records a subagent's responses in its parent's usage itself. A Codex subagent's are
//! counted here from its own rollout once its run has settled, added to the parent's harness
//! timeline as one response, and recorded with its end.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use st_drivers::subagents::{self as ledger, Ended, Ledger, Subagent, Tokens};

use crate::client::{Client, api_error_code};
use crate::model::{ClaimInput, ClaimRecord, StepRunView};
use crate::store::SUBAGENT_LEASE_MS;

const PUBLISHED_FILE: &str = ".harness-subagents-published";

/// Codex writes a run's last token count just before the parent hears that it completed, so its
/// tokens are counted this long after the end.
const SETTLE_MS: u64 = 2_000;
/// A run whose rollout cannot be found by then ends without tokens.
const ROLLOUT_WAIT_MS: u64 = 60_000;

/// A running subagent's lease. `ST3_SUBAGENT_LEASE_MS` can only shorten it, so tests of an
/// unrenewed lease need not wait out the default.
fn lease_ms() -> u64 {
    std::env::var("ST3_SUBAGENT_LEASE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|cap| *cap > 0)
        .map_or(SUBAGENT_LEASE_MS, |cap| cap.min(SUBAGENT_LEASE_MS))
}

/// What this driver has recorded, kept beside the ledger so a driver replaced in place carries on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Published {
    incarnation: String,
    /// Subagents recorded as appeared and not yet ended, with their lease.
    #[serde(default)]
    open: BTreeMap<String, u64>,
    /// Subagents st ended while the harness still ran them, which this driver records no more.
    #[serde(default)]
    closed: BTreeSet<String>,
}

pub struct Publisher {
    subject: String,
    driver: String,
    incarnation: String,
    agent_dir: PathBuf,
    home: Option<PathBuf>,
    codex_home: Option<PathBuf>,
    /// The incarnation stamped on the harness timeline, which a subagent's usage joins.
    timeline_incarnation: Option<String>,
    lease_ms: u64,
    published: Published,
}

/// Errors that mean the store will never take this claim, so retrying is pointless: the subagent
/// ended or never appeared there, the claim is malformed, or the daemon predates subagents.
fn refused(error: &anyhow::Error) -> bool {
    matches!(
        api_error_code(error),
        Some(
            "unknown-subagent"
                | "subagent-ended"
                | "invalid-subagent"
                | "invalid-claim-field"
                | "unknown-claim-field"
                | "missing-claim-field"
                | "invalid-subject-reference"
                | "unknown-claim-kind"
                | "claim-write-forbidden"
        )
    )
}

impl Publisher {
    /// The publisher of driver incarnation `incarnation`, which started at `started_at_ms`.
    /// Subagents an earlier harness of this seat started end as `harness-exited`.
    pub fn start(
        subject: &str,
        driver: &str,
        incarnation: &str,
        agent_dir: &Path,
        started_at_ms: u64,
    ) -> Self {
        if let Err(error) =
            ledger::update(agent_dir, |ledger| ledger.adopt(incarnation, started_at_ms))
        {
            tracing::warn!("st driver: subagent ledger adoption failed: {error:#}");
        }
        let published = std::fs::read(agent_dir.join(PUBLISHED_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Published>(&bytes).ok())
            .filter(|published| published.incarnation == incarnation)
            .unwrap_or_else(|| Published {
                incarnation: incarnation.into(),
                ..Published::default()
            });
        Self {
            subject: subject.into(),
            driver: driver.into(),
            incarnation: incarnation.into(),
            agent_dir: agent_dir.into(),
            home: std::env::var_os("HOME").map(PathBuf::from),
            codex_home: st_drivers::codex_app_server::codex_home(),
            timeline_incarnation: None,
            lease_ms: lease_ms(),
            published,
        }
    }

    /// Read transcripts beneath these homes instead of this process's (`HOME`, `CODEX_HOME`).
    pub fn with_homes(mut self, home: Option<PathBuf>, codex_home: Option<PathBuf>) -> Self {
        self.home = home;
        self.codex_home = codex_home;
        self
    }

    /// The harness timeline's current incarnation. A Codex subagent's tokens join the parent's
    /// usage only through it.
    pub fn set_timeline_incarnation(&mut self, incarnation: Option<String>) {
        self.timeline_incarnation = incarnation;
    }

    /// End every running subagent, as when the harness exits, and record the ends.
    pub async fn end_all(&mut self, client: &Client, outcome: &str, reason: &str) -> Result<()> {
        let now = ledger::now_ms();
        ledger::update(&self.agent_dir, |ledger| {
            ledger.end_all(outcome, reason, now)
        })?;
        self.tick(client).await
    }

    /// Record what changed in the ledger since the last tick.
    pub async fn tick(&mut self, client: &Client) -> Result<()> {
        let now = ledger::now_ms();
        let current = ledger::update(&self.agent_dir, |ledger| {
            ledger.end_unlisted(now);
            ledger.clone()
        })?;
        let before = self.published.clone();
        let result = self.publish(client, &current, now).await;
        if self.published != before {
            let bytes = serde_json::to_vec(&self.published)?;
            std::fs::write(self.agent_dir.join(PUBLISHED_FILE), bytes)?;
        }
        result
    }

    async fn publish(&mut self, client: &Client, current: &Ledger, now: u64) -> Result<()> {
        let mut failure = None;
        let unrecorded = |id: &String| {
            !self.published.open.contains_key(id) && !self.published.closed.contains(id)
        };
        let appearing = current
            .running
            .values()
            .filter(|subagent| unrecorded(&subagent.id))
            .chain(
                current
                    .ended
                    .iter()
                    .map(|ended| &ended.subagent)
                    .filter(|subagent| unrecorded(&subagent.id)),
            )
            .cloned()
            .collect::<Vec<_>>();
        if !appearing.is_empty() {
            let step = self.held_step(client).await;
            for subagent in appearing {
                match self.appear(client, &subagent, step.as_deref(), now).await {
                    Ok(lease) => {
                        self.published.open.insert(subagent.id.clone(), lease);
                    }
                    Err(error) if refused(&error) => {
                        self.published.closed.insert(subagent.id.clone());
                    }
                    Err(error) => {
                        failure.get_or_insert(error);
                    }
                }
            }
        }
        let mut recorded = BTreeSet::new();
        let mut counted = current.counted.clone();
        for ended in &current.ended {
            let id = &ended.subagent.id;
            let closed = self.published.closed.contains(id);
            if !closed && !self.published.open.contains_key(id) {
                continue;
            }
            // Its responses reach the parent's usage even when st already ended it.
            let ended = match self.settle(ended, &mut counted, now) {
                Ok(Some(ended)) => ended,
                Ok(None) => continue,
                Err(error) => {
                    failure.get_or_insert(error);
                    continue;
                }
            };
            if !closed {
                match self.end(client, &ended).await {
                    Ok(()) => {}
                    Err(error) if refused(&error) => {}
                    Err(error) => {
                        failure.get_or_insert(error);
                        continue;
                    }
                }
                self.published.open.remove(id);
            }
            recorded.insert(id.clone());
        }
        let renewing = self
            .published
            .open
            .iter()
            .filter(|(id, lease)| {
                current.running.contains_key(*id) && lease.saturating_sub(now) <= self.lease_ms / 2
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in renewing {
            match self.renew(client, &id, now).await {
                Ok(lease) => {
                    self.published.open.insert(id, lease);
                }
                // st ended it (its lease ran out while this driver could not renew): leave it.
                Err(error) if refused(&error) => {
                    self.published.open.remove(&id);
                    self.published.closed.insert(id);
                }
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        // A subagent recorded as open that the ledger no longer has lost its end on this host.
        let lost = self
            .published
            .open
            .keys()
            .filter(|id| {
                !current.running.contains_key(*id)
                    && !current.ended.iter().any(|ended| &ended.subagent.id == *id)
            })
            .cloned()
            .collect::<Vec<_>>();
        for id in lost {
            let ended = Ended {
                subagent: Subagent {
                    id: id.clone(),
                    ..Subagent::default()
                },
                outcome: "interrupted".into(),
                reason: Some("the driver lost its record of this subagent".into()),
                ended_at_ms: now,
                tokens: None,
                parent_usage: false,
            };
            match self.end(client, &ended).await {
                Ok(()) => {}
                Err(error) if refused(&error) => {}
                Err(error) => {
                    failure.get_or_insert(error);
                    continue;
                }
            }
            self.published.open.remove(&id);
        }
        if !recorded.is_empty() {
            ledger::update(&self.agent_dir, |ledger| {
                ledger
                    .ended
                    .retain(|ended| !recorded.contains(&ended.subagent.id));
            })?;
        }
        self.published
            .closed
            .retain(|id| current.running.contains_key(id) && !recorded.contains(id));
        failure.map_or(Ok(()), Err)
    }

    /// Count a Codex run's tokens from its rollout, add them to the parent's usage, and keep them
    /// with its end, so a retried end records the same tokens. `None` until the run has settled.
    fn settle(
        &self,
        ended: &Ended,
        counted: &mut BTreeMap<String, Tokens>,
        now: u64,
    ) -> Result<Option<Ended>> {
        if !ended.parent_usage {
            return Ok(Some(ended.clone()));
        }
        let waited = now.saturating_sub(ended.ended_at_ms);
        if waited < SETTLE_MS {
            return Ok(None);
        }
        let id = ended.subagent.id.clone();
        let thread = ledger::subagent_thread(&id).to_owned();
        let rollout = match &self.codex_home {
            Some(home) => st_drivers::codex_app_server::latest_codex_transcript_in(home, &thread)?,
            None => None,
        };
        let Some(rollout) = rollout else {
            if waited < ROLLOUT_WAIT_MS {
                return Ok(None);
            }
            // Without its rollout the run's tokens are unknown, never zero.
            ledger::update(&self.agent_dir, |ledger| {
                if let Some(entry) = ledger
                    .ended
                    .iter_mut()
                    .find(|entry| entry.subagent.id == id)
                {
                    entry.parent_usage = false;
                }
            })?;
            return Ok(Some(Ended {
                parent_usage: false,
                ..ended.clone()
            }));
        };
        let Some(incarnation) = self.timeline_incarnation.as_deref() else {
            return Ok(None);
        };
        let (total, model) = ledger::codex_rollout_tokens(&rollout)?;
        let run = total.since(counted.get(&thread).copied().unwrap_or_default());
        ledger::record_parent_usage(
            &self.agent_dir,
            &self.driver,
            incarnation,
            &id,
            model.as_deref(),
            run,
        )?;
        counted.insert(thread.clone(), total);
        ledger::update(&self.agent_dir, |ledger| {
            ledger.counted.insert(thread, total);
            if let Some(entry) = ledger
                .ended
                .iter_mut()
                .find(|entry| entry.subagent.id == id)
            {
                entry.tokens = Some(run);
                entry.parent_usage = false;
            }
        })?;
        Ok(Some(Ended {
            tokens: Some(run),
            parent_usage: false,
            ..ended.clone()
        }))
    }

    /// The step this seat holds now, newest first among more than one.
    async fn held_step(&self, client: &Client) -> Option<String> {
        let work: Vec<StepRunView> = client
            .get(&format!(
                "/v1/work?actor={}",
                urlencoding::encode(&self.subject)
            ))
            .await
            .ok()?;
        work.into_iter()
            .filter(|step| {
                matches!(
                    step.status.as_str(),
                    "claimed" | "working" | "verifying" | "blocked"
                ) && step.claimant.as_deref() == Some(self.subject.as_str())
            })
            .max_by_key(|step| {
                (
                    step.claim_incarnation.as_deref() == Some(self.incarnation.as_str()),
                    step.execution_started_at_unix_ms,
                    step.updated_at_unix_ms,
                )
            })
            .map(|step| step.subject)
    }

    fn claim(&self, kind: &str, fields: BTreeMap<String, Value>) -> ClaimInput {
        ClaimInput {
            subject: self.subject.clone(),
            kind: kind.into(),
            actor: Some(self.subject.clone()),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        }
    }

    async fn appear(
        &self,
        client: &Client,
        subagent: &Subagent,
        step: Option<&str>,
        now: u64,
    ) -> Result<u64> {
        let mut subagent_type = subagent.subagent_type.clone();
        let mut description = subagent.description.clone();
        // Claude writes the launching call's description beside the transcript just after the
        // subagent starts; it is exact where the hook's match by type may not be.
        if let Some((meta_description, meta_type)) = subagent
            .transcript
            .as_deref()
            .and_then(ledger::claude_subagent_meta)
        {
            description = meta_description.or(description);
            subagent_type = meta_type.or(subagent_type);
        }
        let lease = now.saturating_add(self.lease_ms);
        let mut fields = BTreeMap::from([
            ("subagent_id".into(), Value::String(subagent.id.clone())),
            ("driver".into(), Value::String(self.driver.clone())),
            (
                "incarnation_id".into(),
                Value::String(self.incarnation.clone()),
            ),
            ("lease_expires_at_unix_ms".into(), Value::from(lease)),
        ]);
        if subagent.started_at_ms > 0 {
            fields.insert(
                "started_at_unix_ms".into(),
                Value::from(subagent.started_at_ms),
            );
        }
        for (name, value) in [
            ("subagent_type", subagent_type),
            ("description", description),
            ("session_id", subagent.session_id.clone()),
            ("step_run", step.map(str::to_owned)),
        ] {
            if let Some(value) = value {
                fields.insert(name.into(), Value::String(value));
            }
        }
        let input = self.claim("subagent.appeared", fields);
        let _: ClaimRecord = client.post("/v1/claims", &input).await?;
        Ok(lease)
    }

    async fn renew(&self, client: &Client, id: &str, now: u64) -> Result<u64> {
        let lease = now.saturating_add(self.lease_ms);
        let input = self.claim(
            "subagent.renewed",
            BTreeMap::from([
                ("subagent_id".into(), Value::String(id.into())),
                (
                    "incarnation_id".into(),
                    Value::String(self.incarnation.clone()),
                ),
                ("lease_expires_at_unix_ms".into(), Value::from(lease)),
            ]),
        );
        let _: ClaimRecord = client.post("/v1/claims", &input).await?;
        Ok(lease)
    }

    async fn end(&self, client: &Client, ended: &Ended) -> Result<()> {
        let mut fields = BTreeMap::from([
            (
                "subagent_id".into(),
                Value::String(ended.subagent.id.clone()),
            ),
            ("outcome".into(), Value::String(ended.outcome.clone())),
            ("ended_at_unix_ms".into(), Value::from(ended.ended_at_ms)),
        ]);
        if let Some(reason) = &ended.reason {
            fields.insert("reason".into(), Value::String(reason.clone()));
        }
        if ended.subagent.started_at_ms > 0 {
            fields.insert(
                "duration_ms".into(),
                Value::from(
                    ended
                        .ended_at_ms
                        .saturating_sub(ended.subagent.started_at_ms),
                ),
            );
        }
        if let Some(tokens) = self.tokens(ended) {
            for (name, value) in [
                ("input_tokens", tokens.input_tokens),
                ("output_tokens", tokens.output_tokens),
                ("cache_write_tokens", tokens.cache_write_tokens),
                ("cached_tokens", tokens.cached_tokens),
                ("total_tokens", tokens.total_tokens),
            ] {
                fields.insert(name.into(), Value::from(value));
            }
        }
        let input = self.claim("subagent.ended", fields);
        let _: ClaimRecord = client.post("/v1/claims", &input).await?;
        Ok(())
    }

    fn tokens(&self, ended: &Ended) -> Option<Tokens> {
        if ended.tokens.is_some() {
            return ended.tokens;
        }
        let transcript = ended.subagent.transcript.as_deref()?;
        match ledger::claude_transcript_tokens(transcript, self.home.as_deref()?) {
            Ok(tokens) => Some(tokens),
            Err(error) => {
                tracing::debug!(
                    "st driver: no tokens for subagent {}: {error:#}",
                    ended.subagent.id
                );
                None
            }
        }
    }
}
