//! Seats' GitHub watches: declaring one on the repository's standing observer, listing them,
//! and ending one. The observation write decides each watch's wakes (`record_resource_observation`).

use super::*;
use crate::github_watch::{self, ThreadRef};

/// What ended watches `st gh ls` still shows, by how recently they ended.
const ENDED_SHOWN_FOR_MS: u128 = 24 * 60 * 60 * 1000;

impl Store {
    /// Declare a seat's watch on one thread, from now, and the repository's standing observer when
    /// it is not running. Watching a thread the seat already watches keeps that watch and takes
    /// the new deadline; watching one whose watch ended begins a new watch.
    pub fn declare_watch(
        &self,
        thread: &ThreadRef,
        agent: &str,
        until_unix_ms: Option<u128>,
    ) -> Result<Value, St3Error> {
        let now = now_ms();
        if until_unix_ms.is_some_and(|until| until <= now) {
            return Err(St3Error::new(
                "invalid-watch-deadline",
                "a watch's deadline must be in the future",
            ));
        }
        self.ensure_watch_observer(thread)?;
        let subject = thread.watch(agent);
        let current = self.live_watch(&subject)?;
        let since = current
            .as_ref()
            .map_or(now, |(_, watch)| watch.since_unix_ms);
        if current
            .as_ref()
            .is_none_or(|(_, watch)| watch.until_unix_ms != until_unix_ms)
        {
            let source = github_watch::watch_source(thread, agent, since, until_unix_ms);
            let intent = crate::graph::parse_internal_intent(&source, &self.origin)?;
            self.apply_internal(&intent, &format!("github-watch:{subject}:{since}"))?;
        }
        self.watch_view(&subject)?.ok_or_else(|| {
            St3Error::new(
                "watch-not-declared",
                format!("the watch `{subject}` was not declared"),
            )
        })
    }

    /// Declare the repository's standing observer on this host unless it is running.
    pub(crate) fn ensure_watch_observer(&self, thread: &ThreadRef) -> Result<bool, St3Error> {
        let running = self
            .desired_subjects_named(&[thread.observer()])
            .map_err(internal)?
            .into_iter()
            .next()
            .and_then(|observer| crate::graph::observer_spec(&observer.desired))
            .is_some_and(|spec| !spec.stopped);
        if running {
            return Ok(false);
        }
        let intent = crate::graph::parse_internal_intent(
            &github_watch::observer_source(thread),
            &self.origin,
        )?;
        self.apply_internal(
            &intent,
            &format!("github-watch-observer:{}", thread.locator()),
        )?;
        Ok(true)
    }

    /// A watch that is declared, not stopped, and not ended, with its declaration.
    pub(crate) fn live_watch(
        &self,
        subject: &str,
    ) -> Result<Option<(SubscriptionSpec, crate::model::WatchSpec)>, St3Error> {
        let Some(spec) = self
            .desired_subjects_named(&[subject.to_owned()])
            .map_err(internal)?
            .into_iter()
            .next()
            .and_then(|desired| crate::graph::subscription_spec(&desired.desired))
            .filter(|spec| !spec.stopped)
        else {
            return Ok(None);
        };
        let Some(watch) = spec.watch.clone() else {
            return Ok(None);
        };
        if self.watch_ended(subject, &watch)?.is_some() {
            return Ok(None);
        }
        Ok(Some((spec, watch)))
    }

    /// How a watch ended, if it did.
    pub fn watch_ended(
        &self,
        subject: &str,
        watch: &crate::model::WatchSpec,
    ) -> Result<Option<Value>, St3Error> {
        let connection = self.readers.get();
        watch_ended_tx(&connection, subject, watch)
    }

    /// End a watch and stop its declaration. A final wake, when one is given, reaches its seat in
    /// the same write. Ending a watch that already ended changes nothing.
    pub fn end_watch(
        &self,
        subject: &str,
        reason: &str,
        final_wake: Option<&github_watch::Wake>,
    ) -> Result<bool, St3Error> {
        let Some((spec, watch)) = self.live_watch(subject)? else {
            return Ok(false);
        };
        let origin = self.origin.clone();
        let ended = self
            .connection
            .batched(|transaction| -> Result<bool, St3Error> {
                if let Some(wake) = final_wake
                    && latest_claim_id_tx(transaction, &wake.subject)
                        .map_err(internal)?
                        .is_none()
                {
                    append_claim_tx(
                        transaction,
                        &origin,
                        &wake.subject,
                        "message.sent",
                        None,
                        &json!({"fields": {
                            "from": format!("daemon/{origin}"),
                            "to": spec.to,
                            "title": wake.title,
                            "content": wake.content,
                            "status": "sent",
                            "tags": wake.tags,
                        }}),
                        &[],
                        None,
                    )
                    .map_err(claim_append_error)?;
                }
                end_watch_tx(
                    transaction,
                    &origin,
                    None,
                    subject,
                    &watch,
                    reason,
                    final_wake.map(|wake| wake.subject.as_str()),
                )
            })
            .map_err(|error| St3Error::new("internal", error))??;
        self.stop_watch_declaration(subject)?;
        Ok(ended)
    }

    /// Stop an ended watch's declaration, or the standing observer's.
    pub(crate) fn stop_watch_declaration(&self, subject: &str) -> Result<(), St3Error> {
        let Some(source) = github_watch::stop_source(subject) else {
            return Ok(());
        };
        let intent = crate::graph::parse_internal_intent(&source, &self.origin)?;
        self.apply_internal(&intent, &format!("github-watch-stop:{subject}"))?;
        Ok(())
    }

    /// Whether a seat's declaration is live: declared, not stopped, and its run still running.
    pub fn seat_live(&self, agent: &str) -> Result<bool, St3Error> {
        let connection = self.readers.get();
        person_work::declaration_live(&connection, agent).map_err(internal)
    }

    /// A stopped seat still owns its conversation and watches. Only removing the declaration
    /// or explicitly starting a fresh conversation ends them silently.
    pub(crate) fn watch_seat_ended(
        &self,
        subject: &str,
        agent: &str,
        watch: &crate::model::WatchSpec,
    ) -> Result<bool, St3Error> {
        if self
            .desired_subjects_named(&[agent.to_owned()])
            .map_err(internal)?
            .is_empty()
        {
            return Ok(true);
        }
        let resets = self
            .observations_for(agent, "runtime.action.requested")
            .map_err(internal)?
            .into_iter()
            .filter(|claim| {
                claim.body["fields"]["action"] == "fresh-context"
                    && claim.accepted_at_unix_ms >= watch.since_unix_ms
            })
            .collect::<Vec<_>>();
        if resets.is_empty() {
            return Ok(false);
        }
        // A deadline edit keeps the original conversation. Use the first declaration of this
        // watch, rather than the latest edit; log order also distinguishes writes in one ms.
        let began = self
            .claims_for(subject, Some("intent.desired"))
            .map_err(internal)?
            .into_iter()
            .filter(|claim| {
                crate::graph::subscription_spec(&claim.body["desired"])
                    .and_then(|spec| spec.watch)
                    .is_some_and(|prior| prior.since_unix_ms == watch.since_unix_ms)
            })
            .map(|claim| claim_log_order(&claim))
            .min();
        Ok(resets.iter().any(|claim| {
            claim.accepted_at_unix_ms > watch.since_unix_ms
                || began.is_some_and(|began| claim_log_order(claim) > began)
        }))
    }

    /// One watch as `st gh ls` shows it, if it was ever declared.
    pub fn watch_view(&self, subject: &str) -> Result<Option<Value>, St3Error> {
        let Some(desired) = self
            .desired_subjects_named(&[subject.to_owned()])
            .map_err(internal)?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        self.watch_view_of(subject, &desired.desired)
    }

    fn watch_view_of(&self, subject: &str, desired: &Value) -> Result<Option<Value>, St3Error> {
        let Some(spec) = crate::graph::subscription_spec(desired) else {
            return Ok(None);
        };
        // A stopped declaration keeps no item; the view reads the watch that ended from its claims.
        let watch = match spec.watch.clone() {
            Some(watch) => watch,
            None => match self.last_ended_watch(subject)? {
                Some(watch) => watch,
                None => return Ok(None),
            },
        };
        let ended = self.watch_ended(subject, &watch)?;
        let Some((thread, _)) = github_watch::watch_parts(subject) else {
            return Ok(None);
        };
        let resource = thread.resource();
        let mut facts = None;
        for segment in ["pull-request", "issue"] {
            facts = self
                .latest_actual_value(&format!("{resource}/{segment}/{}", thread.number))
                .map_err(internal)?
                .and_then(|actual| actual.get("facts").cloned());
            if facts.is_some() {
                break;
            }
        }
        let observer_state = self
            .claims_for(&thread.observer(), Some("observer.state"))
            .map_err(internal)?
            .last()
            .map(|claim| claim.body["fields"].clone());
        Ok(Some(github_watch::watch_view(
            subject,
            &watch,
            ended.as_ref(),
            facts.as_ref(),
            observer_state.as_ref(),
        )))
    }

    /// The watch that most recently ended under this subject, read from its ending.
    fn last_ended_watch(&self, subject: &str) -> Result<Option<crate::model::WatchSpec>, St3Error> {
        let Some((thread, _)) = github_watch::watch_parts(subject) else {
            return Ok(None);
        };
        Ok(self
            .claims_for(subject, Some("subscription.watch-ended"))
            .map_err(internal)?
            .last()
            .and_then(|claim| {
                Some(crate::model::WatchSpec {
                    item: thread.number,
                    since_unix_ms: claim.body["fields"]["since_unix_ms"]
                        .as_str()?
                        .parse()
                        .ok()?,
                    until_unix_ms: None,
                })
            }))
    }

    /// The watches of one seat, or of every seat: each running watch, and each that ended within a
    /// day.
    pub fn watches(&self, agent: Option<&str>) -> Result<Vec<Value>, St3Error> {
        let now = now_ms();
        let mut views = Vec::new();
        // Use the subject primary key to read only watches: unrelated fleet declarations must
        // not make listing a seat's watches more expensive.
        let desired = {
            let connection = self.readers.get();
            let mut statement = connection
                .prepare_cached(
                    "SELECT subject, kind, body, member, owner_run, owner_generation, owner_step
                     FROM desired WHERE subject >= 'subscription/watch/'
                       AND subject < 'subscription/watch0' ORDER BY subject",
                )
                .map_err(internal)?;
            statement
                .query_map([], desired_from_row)
                .map_err(internal)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(internal)?
        };
        for desired in desired {
            if let Some(agent) = agent
                && github_watch::watch_parts(&desired.subject)
                    .is_none_or(|(_, owner)| owner != agent)
            {
                continue;
            }
            let Some(view) = self.watch_view_of(&desired.subject, &desired.desired)? else {
                continue;
            };
            if view["state"] == "ended" {
                let ended_at = self
                    .claims_for(&desired.subject, Some("subscription.watch-ended"))
                    .map_err(internal)?
                    .last()
                    .map(|claim| claim.accepted_at_unix_ms)
                    .unwrap_or_default();
                if now.saturating_sub(ended_at) > ENDED_SHOWN_FOR_MS {
                    continue;
                }
            }
            views.push(view);
        }
        Ok(views)
    }

    /// Record that a seat posted a comment or review, by its GitHub ID. The first seat to record
    /// an ID keeps it; recording it again as that seat changes nothing.
    pub fn record_github_post(
        &self,
        agent: &str,
        thread: &ThreadRef,
        kind: &str,
        id: u64,
        url: &str,
        login: &str,
    ) -> Result<Value, St3Error> {
        let locator = thread.locator();
        if let Some(owner) = self.github_post_agent(&locator, kind, id)? {
            if owner != agent {
                return Err(St3Error::new(
                    "github-post-registered",
                    format!("{kind} {id} on {thread} is already recorded as {owner}'s"),
                ));
            }
        } else {
            self.append_claim(&ClaimInput {
                subject: github_watch::github_post_subject(&locator, kind, id),
                kind: "github.posted".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("agent".into(), Value::String(agent.to_owned())),
                    ("repository".into(), Value::String(locator.clone())),
                    ("item".into(), Value::from(thread.number)),
                    ("kind".into(), Value::String(kind.to_owned())),
                    ("id".into(), Value::from(id)),
                    ("url".into(), Value::String(url.to_owned())),
                    ("login".into(), Value::String(login.to_owned())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("github-posted:{locator}:{kind}:{id}")),
            })?;
        }
        self.close_own_post_wakes(agent)?;
        Ok(json!({
            "agent": agent,
            "thread": thread.to_string(),
            "kind": kind,
            "id": id,
            "url": url,
        }))
    }

    /// The seat that recorded a comment or review, by its GitHub ID.
    pub fn github_post_agent(
        &self,
        locator: &str,
        kind: &str,
        id: u64,
    ) -> Result<Option<String>, St3Error> {
        let connection = self.readers.get();
        github_post_agent_tx(&connection, locator, kind, id)
    }

    /// Withdraw each undelivered wake of a seat about a comment or review it recorded as its own,
    /// so the seat never hears about what it posted. Such a wake exists when another host observed
    /// the comment before the seat's record reached it.
    pub fn close_own_post_wakes(&self, agent: &str) -> Result<Vec<String>, St3Error> {
        let mut closed = Vec::new();
        for message in self
            .messages(Some(agent), false)
            .map_err(internal)?
            .into_iter()
            .filter(|message| matches!(message.status.as_str(), "sent" | "staged"))
        {
            let Some((locator, kind, id)) = github_watch::named_object(&message) else {
                continue;
            };
            if self.github_post_agent(&locator, &kind, id)?.as_deref() != Some(agent) {
                continue;
            }
            self.append_claim(&ClaimInput {
                subject: message.subject.clone(),
                kind: "message.closed".into(),
                actor: Some("daemon/runtime".into()),
                fields: BTreeMap::from([("status".into(), Value::String("closed".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("message-closed:{}", message.subject)),
            })?;
            closed.push(message.subject);
        }
        Ok(closed)
    }
}
