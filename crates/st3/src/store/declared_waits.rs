//! Declared waits: `st work wait STEP --on TARGET` says what a held step waits on. The target is
//! a pull request (`OWNER/REPO#N`, which covers its checks, reviews, merge queue and merge), another
//! step (`step-run/...`), or a time (a duration such as `45m`, or an RFC 3339 time).
//!
//! The wait is a `work.progress` claim with `wait_on` beside the paused clock. It stays open until
//! the holder declares another wait or resumes, the attempt ends, or st records `work.wait-ended`
//! on the step when the target moves: the pull request's watch ends because it merged or closed,
//! the step finishes, or the time comes. Each read here is a lookup of one step's newest claims.

use super::*;
use crate::github_watch::ThreadRef;
use crate::model::DeclaredWaitView;

/// The claim st records on a step when the target of its open declared wait moves.
pub const WORK_WAIT_ENDED_KIND: &str = "work.wait-ended";

/// What a declared wait waits on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WaitTarget {
    PullRequest(ThreadRef),
    Step(String),
    Time(u128),
}

impl WaitTarget {
    /// Read `OWNER/REPO#N` or a GitHub pull request URL, a `step-run/` subject, or a time: a
    /// duration from `now` such as `45m`, or an RFC 3339 time.
    pub fn parse(text: &str, now: u128) -> Result<Self, St3Error> {
        let text = text.trim();
        if text.starts_with("step-run/") {
            return Ok(Self::Step(text.to_owned()));
        }
        if let Some(at) = crate::github_watch::parse_rfc3339_ms(text) {
            return Self::time(text, at, now);
        }
        if let Ok(duration) = crate::graph::parse_duration(text, true) {
            return Self::time(text, now.saturating_add(u128::from(duration)), now);
        }
        ThreadRef::parse(text).map(Self::PullRequest).map_err(|_| {
            St3Error::new(
                "invalid-wait-target",
                format!(
                    "`{text}` is not a pull request (OWNER/REPO#N), a step (step-run/...), or a time (45m, or an RFC 3339 time)"
                ),
            )
        })
    }

    fn time(text: &str, at: u128, now: u128) -> Result<Self, St3Error> {
        if at <= now {
            return Err(St3Error::new(
                "invalid-wait-target",
                format!("`{text}` has already passed"),
            ));
        }
        Ok(Self::Time(at))
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::PullRequest(_) => "pull-request",
            Self::Step(_) => "step",
            Self::Time(_) => "time",
        }
    }

    pub fn subject(&self) -> String {
        match self {
            Self::PullRequest(thread) => thread.to_string(),
            Self::Step(step) => step.clone(),
            Self::Time(at) => crate::github_watch::rfc3339(*at),
        }
    }

    pub fn until_unix_ms(&self) -> Option<u128> {
        match self {
            Self::Time(at) => Some(*at),
            _ => None,
        }
    }

    /// The `wait_on` field of the wait's claim.
    pub fn field(&self) -> Value {
        let mut field = json!({"kind": self.kind(), "subject": self.subject()});
        if let Some(until) = self.until_unix_ms() {
            field["until_unix_ms"] = json!(until.to_string());
        }
        field
    }
}

/// How a declared wait reads in a summary or a message: `pull request o/r#7`.
pub fn describe_wait(wait: &DeclaredWaitView) -> String {
    match wait.kind.as_str() {
        "pull-request" => format!("pull request {}", wait.subject),
        "step" => format!("step {}", wait.subject),
        "time" => format!("time {}", wait.subject),
        _ => wait.subject.clone(),
    }
}

/// The open declared wait of `subject`'s attempt as of `snapshot_unix_ms`: the newest record that
/// moves its clock or ends its wait is a declared wait. A new claim, a resume, a wait that names no
/// target, a release, a submission, a failure and `work.wait-ended` each close it.
pub(crate) fn declared_wait_at(
    connection: &Connection,
    subject: &str,
    attempt: u32,
    snapshot_unix_ms: u128,
) -> rusqlite::Result<Option<DeclaredWaitView>> {
    let snapshot = snapshot_unix_ms.to_string();
    let row = connection
        .prepare_cached(&canonical_sql(
            "SELECT id, kind, json_extract(body, '$.fields.wait_on'), accepted_at_unix_ms FROM claims
             WHERE subject=?1
               AND kind IN ('work.claimed','work.progress','work.wait-ended','work.released','work.submitted','work.failed')
               AND (kind!='work.progress' OR json_extract(body, '$.fields.clock') IS NOT NULL)
               AND json_extract(body, '$.fields.attempt')=?2
               AND (length(accepted_at_unix_ms), accepted_at_unix_ms) <= (length(?3), ?3)
             ORDER BY CANONICAL_DESC(claims) LIMIT 1",
        ))?
        .query_row(params![subject, attempt, snapshot], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .optional()?;
    let Some((claim, kind, Some(wait_on), at)) = row else {
        return Ok(None);
    };
    if kind != "work.progress" {
        return Ok(None);
    }
    let Ok(wait_on) = serde_json::from_str::<Value>(&wait_on) else {
        return Ok(None);
    };
    let (Some(kind), Some(target)) = (
        wait_on.get("kind").and_then(Value::as_str),
        wait_on.get("subject").and_then(Value::as_str),
    ) else {
        return Ok(None);
    };
    Ok(Some(DeclaredWaitView {
        kind: kind.to_owned(),
        subject: target.to_owned(),
        claim,
        since_unix_ms: at.parse().unwrap_or_default(),
        until_unix_ms: wait_on
            .get("until_unix_ms")
            .and_then(|until| until.as_str().and_then(|text| text.parse().ok()).or(until.as_u64().map(u128::from))),
    }))
}

/// Copies a held step's open declared wait into its view.
pub(super) fn enrich_declared_wait_at(
    connection: &Connection,
    view: &mut StepRunView,
    snapshot_unix_ms: u128,
) -> rusqlite::Result<()> {
    if !matches!(view.status.as_str(), "claimed" | "working") {
        return Ok(());
    }
    view.waiting_on = declared_wait_at(connection, &view.subject, view.attempt, snapshot_unix_ms)?;
    Ok(())
}

/// The wake st sends a seat when the target of its declared wait moves without waking it.
pub struct WaitWake {
    pub agent: String,
    pub title: String,
    pub content: String,
}

impl Store {
    /// Refuse unless `actor`'s incarnation holds a live lease on `subject`, as a work action
    /// would, before a wait sets anything up outside the step.
    pub(crate) fn check_work_holder(
        &self,
        subject: &str,
        actor: &str,
        incarnation: Option<&str>,
    ) -> Result<(), St3Error> {
        let subject = normalize_step_run(subject);
        let actor = normalize_actor(actor, "agent");
        let step = self
            .step_run(&subject)
            .map_err(internal)?
            .ok_or_else(|| St3Error::new("work-not-found", format!("no step run `{subject}`")))?;
        let holds = matches!(step.status.as_str(), "claimed" | "working")
            && step.claimant.as_deref() == Some(actor.as_str())
            && step.claim_expires_at_unix_ms.is_some_and(|expiry| expiry > now_ms());
        if !holds {
            return Err(St3Error::new(
                "work-not-claimed",
                format!("`{actor}` does not hold an active lease for `{subject}`"),
            ));
        }
        if incarnation.is_some_and(|incarnation| step.claim_incarnation.as_deref() != Some(incarnation)) {
            return Err(St3Error::new(
                "wrong-work-incarnation",
                format!("another `{actor}` incarnation holds `{subject}`"),
            ));
        }
        Ok(())
    }

    /// The open declared wait of a held step's attempt.
    pub(crate) fn declared_wait(
        &self,
        subject: &str,
        attempt: u32,
    ) -> Result<Option<DeclaredWaitView>> {
        smallclaims::touched::note_read(|| subject.to_owned());
        let connection = self.readers.get();
        Ok(declared_wait_at(&connection, subject, attempt, now_ms())?)
    }

    /// Why a watch last ended: merged, closed, deadline, unwatched or seat-ended.
    pub(crate) fn last_watch_end(&self, watch: &str) -> Result<Option<String>> {
        smallclaims::touched::note_read(|| watch.to_owned());
        let connection = self.readers.get();
        Ok(connection
            .prepare_cached(&canonical_sql(
                "SELECT json_extract(body, '$.fields.reason') FROM claims
                 WHERE subject=?1 AND kind='subscription.watch-ended'
                 ORDER BY CANONICAL_DESC(claims) LIMIT 1",
            ))?
            .query_row([watch], |row| row.get::<_, Option<String>>(0))
            .optional()?
            .flatten())
    }

    /// Record that the target of `step`'s declared wait `wait` moved, and send `wake` when the
    /// target did not wake the seat itself. Nothing is written unless `wait` is still the step's
    /// open wait, so a wait the holder replaced meanwhile stays open. Returns whether it ended.
    pub(crate) fn end_declared_wait(
        &self,
        step: &str,
        attempt: u32,
        wait: &str,
        reason: &str,
        wake: Option<&WaitWake>,
    ) -> Result<bool> {
        let origin = self.origin.clone();
        let ended = self.connection.batched(|transaction| -> Result<bool> {
            let current = declared_wait_at(transaction, step, attempt, u128::MAX)?;
            if current.is_none_or(|current| current.claim != wait) {
                return Ok(false);
            }
            let key = format!("st3-wait-ended:{step}:{wait}");
            let mut message = None;
            if let Some(wake) = wake {
                let subject = format!(
                    "message/{}",
                    &hex::encode(Sha256::digest(key.as_bytes()))[..16]
                );
                append_receipt_claim_tx(
                    transaction,
                    &origin,
                    &subject,
                    "message.sent",
                    Some("daemon/runtime"),
                    &json!({"fields": {
                        "from": "daemon/runtime",
                        "to": wake.agent,
                        "content": wake.content,
                        "status": "sent",
                        "title": wake.title,
                        "in_reply_to": null,
                        "tags": ["st3-wait-ended", format!("st3-work:{step}")],
                    }}),
                    &[],
                    None,
                    &format!("{key}:message"),
                )?;
                message = Some(subject);
            }
            let mut fields = json!({"attempt": attempt, "wait": wait, "reason": reason});
            if let Some(message) = message {
                fields["message"] = json!(message);
            }
            append_receipt_claim_tx(
                transaction,
                &origin,
                step,
                WORK_WAIT_ENDED_KIND,
                Some("daemon/runtime"),
                &json!({"fields": fields}),
                &[],
                None,
                &key,
            )?;
            Ok(true)
        })
        .map_err(anyhow::Error::msg)??;
        Ok(ended)
    }
}
