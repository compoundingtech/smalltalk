use super::*;

impl Store {
    /// Classify the bounded claim interval by the dependencies that cards actually read.
    /// Unknown or structural claims still cold-build; queue changes are diffed separately.
    pub fn changed_agent_resources(
        &self,
        after: u64,
        through: u64,
        previous: &[Value],
    ) -> Result<AgentResourcesChange> {
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT subject, kind, actor FROM claims WHERE store_index>?1 AND store_index<=?2",
        )?;
        let mut delta = AgentResourcesDelta::default();
        let mut owners = BTreeSet::new();
        for row in statement.query_map(params![after, through], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?))
        })? {
            let (subject, kind, actor) = row?;
            // A card reads claims about its agent, the runs, generations and steps that own or
            // queue agents, and messages; work activity is read by actor on step claims. A
            // claim about any other subject, of any kind, changes no card.
            if !["agent/", "mission-run/", "run-generation/", "step-run/", "message/"]
                .iter().any(|prefix| subject.starts_with(prefix))
            {
                continue;
            }
            if self.smalltalk.claim_registry().claim(&kind).is_none() {
                return Ok(AgentResourcesChange::Invalidated {
                    reason: "unknown_kind", detail: format!("unregistered {kind}"),
                });
            }
            if subject.starts_with("agent/") {
                // All registered agent claims can affect the subject's actual reduction or
                // undeclared fallback revision, even kinds not shown as individual fields.
                delta.queues |= kind == "agent.queue.moved";
                delta.membership |= matches!(kind.as_str(), "intent.desired" | "runtime.observed" | "agent.retired");
                delta.subjects.insert(subject);
                continue;
            }
            let owner = match kind.as_str() {
                "mission-run.created" | "mission-run.state" if subject.starts_with("mission-run/") => true,
                "run-generation.created" | "run-generation.state" | "run-generation.superseded"
                    if subject.starts_with("run-generation/") => true,
                "intent.desired" if subject.starts_with("mission-run/") || subject.starts_with("run-generation/") => true,
                _ => false,
            };
            let step = subject.starts_with("step-run/") && matches!(kind.as_str(),
                "step-run.state" | "step-run.retried" | "step-run.carried"
                | "work.claimed" | "work.renewed" | "work.progress" | "work.submitted"
                | "work.failed" | "work.released" | "work.extended"
                | "work.person-asked" | "work.person-done" | "work.person-cancelled");
            // Any other kind on a run, generation or step (a fault, a nudge): a card sees these
            // subjects only through its owner and its queue, so refold the agents they own or
            // queue, and every card's queue and membership, never the whole fleet.
            let other = !owner && !step && ["mission-run/", "run-generation/", "step-run/"]
                .iter().any(|prefix| subject.starts_with(prefix));
            if owner || step || other {
                delta.queues = true;
                delta.membership |= owner || other;
                owners.insert(subject.clone());
                if step || (other && subject.starts_with("step-run/")) {
                    let mut agents = connection.prepare_cached(
                        "SELECT assignee,lease_owner FROM step_runs WHERE subject=?1",
                    )?;
                    for row in agents.query_map([&subject], |row| {
                        Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?))
                    })? {
                        let (assignee, claimant) = row?;
                        delta.subjects.extend(assignee.into_iter().chain(claimant)
                            .filter(|agent| agent.starts_with("agent/")));
                    }
                    // Progress/submission activity is keyed by actor, not the current assignee.
                    if matches!(kind.as_str(), "work.progress" | "work.submitted") {
                        delta.subjects.extend(actor.filter(|actor| actor.starts_with("agent/")));
                    }
                }
                continue;
            }
            if subject.starts_with("message/") && matches!(kind.as_str(),
                "message.sent" | "message.staged" | "message.delivered" | "message.read" | "message.closed") {
                // Sends affect activity; all lifecycle states affect a draining rollout's
                // pending-delivery blocker, including replies through message ancestors.
                // Receipts lack endpoints, so use the same message projection blockers read.
                let Some(message) = self.message(&subject)? else {
                    return Ok(AgentResourcesChange::Invalidated {
                        reason: "missing_message", detail: format!("{kind} without its message"),
                    });
                };
                delta.subjects.extend([message.from, message.to].into_iter()
                    .filter(|party| party.starts_with("agent/")));
                continue;
            }
            // Any other claim about a card input subject: refold conservatively.
            let namespace = subject.split('/').next().unwrap_or_default();
            return Ok(AgentResourcesChange::Invalidated {
                reason: "structural_claim", detail: format!("{kind} on {namespace}/"),
            });
        }
        if !owners.is_empty() {
            // Existing cards carry their historical ownership; current declarations also
            // find agents newly made visible by an owner transition.
            delta.subjects.extend(previous.iter().filter(|item| {
                item["owner_run_id"].as_str().is_some_and(|owner| owners.contains(owner))
                    || item["operational"]["owner_generation"].as_str().is_some_and(|owner| owners.contains(owner))
            }).filter_map(|item| item["id"].as_str().map(str::to_owned)));
            let owners = serde_json::to_string(&owners)?;
            let mut statement = connection.prepare_cached(
                "SELECT subject FROM desired WHERE subject LIKE 'agent/%'
                 AND (owner_run IN (SELECT value FROM json_each(?1))
                   OR owner_generation IN (SELECT value FROM json_each(?1))
                   OR owner_step IN (SELECT value FROM json_each(?1)))",
            )?;
            delta.subjects.extend(statement.query_map([owners], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<BTreeSet<_>>>()?);
        }
        Ok(AgentResourcesChange::Delta(delta))
    }

    /// An allow-list for shallow refs, narrower than card-local invalidation: runtime status
    /// can move an undeclared or stopped agent into history, so runtime.observed is not safe.
    pub fn agent_page_refs_unchanged(&self, after: u64, through: u64, refs: &[Value]) -> Result<bool> {
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT subject, kind FROM claims WHERE store_index>?1 AND store_index<=?2",
        )?;
        for row in statement.query_map(params![after, through], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (subject, kind) = row?;
            let unchanged = match kind.as_str() {
                "daemon.diagnostic" => true,
                "harness.observed" | "harness.diagnostic" | "harness.timeline"
                    | "harness.todo.observed" | "harness.session-file" | "harness.usage" => {
                    subject.starts_with("agent/")
                        && refs.iter().any(|item| item["id"].as_str() == Some(subject.as_str()))
                }
                _ => false,
            };
            if !unchanged { return Ok(false); }
        }
        Ok(true)
    }

    /// The last claim that can change an agent's status: one about an agent, or about the run
    /// or generation that owns it, whose row decides the agent's projection layer. Steps,
    /// gates, subscriptions and diagnostics commit far more often and change no agent status.
    pub fn agent_status_index(&self, snapshot_index: u64) -> Result<u64> {
        // Walk back from the snapshot: about one recent claim in ten matches, so this stops
        // after a few rows instead of scanning every agent observation. The unary `+` keeps
        // SQLite from answering the three patterns from the subject index instead, which reads
        // every agent, run and generation claim the store holds and sorts them.
        let connection = self.readers.get();
        Ok(connection
            .query_row(AGENT_STATUS_INDEX_QUERY, [snapshot_index], |row| row.get(0))
            .optional()?
            .unwrap_or_default())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn agent_resources_builds_for_test(&self) -> usize {
        self.smalltalk.agent_resources_builds.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The most cards one projection call folded since the store opened.
    #[cfg(any(test, feature = "test-support"))]
    pub fn agent_resources_largest_fold_for_test(&self) -> usize {
        self.smalltalk.agent_resources_largest_fold.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn cached_agent_resources(
        &self,
        index: u64,
        history: bool,
        build: impl FnOnce(Option<(&BTreeSet<String>, &[Value])>) -> Result<Vec<Value>>,
    ) -> Result<Vec<Value>> {
        self.cached_agent_resources_for(index, history, None, build)
    }

    pub fn begin_agent_roster_fault_retries(&self, history: bool) {
        let mut retries = self.smalltalk.agent_roster_fault_retries.lock().expect("fault retries poisoned");
        let mode = usize::from(history);
        let mut pending = std::mem::take(&mut retries.pending[mode]);
        retries.due[mode].append(&mut pending);
    }

    pub fn agent_roster_fault_retries(&self, history: bool) -> BTreeSet<String> {
        self.smalltalk.agent_roster_fault_retries.lock().expect("fault retries poisoned")
            .due[usize::from(history)].clone()
    }

    pub fn agent_roster_faults_pending(&self, history: bool) -> bool {
        let retries = self.smalltalk.agent_roster_fault_retries.lock().expect("fault retries poisoned");
        let mode = usize::from(history);
        !retries.due[mode].is_empty() || !retries.pending[mode].is_empty()
    }

    pub fn record_agent_roster_fault_read(
        &self, history: bool, subjects: &[String], deadline: bool,
        selected: Option<&BTreeSet<String>>,
    ) {
        let mut retries = self.smalltalk.agent_roster_fault_retries.lock().expect("fault retries poisoned");
        let mode = usize::from(history);
        let answered = |subject: &String| selected.is_none_or(|selected| selected.contains(subject));
        // Even a deadline answers this attempt: retry only on the next refresh, otherwise
        // a chunked roster would refold the same degraded cards forever and never publish.
        retries.due[mode].retain(|subject| !answered(subject));
        if deadline {
            retries.pending[mode].extend(subjects.iter().cloned());
        } else {
            retries.pending[mode].retain(|subject| !answered(subject));
        }
    }

    /// Only a failed latest attempt with no newer attempt in flight permits immediate fallback.
    pub fn agent_roster_refresh_failed(&self) -> bool {
        self.smalltalk.agent_roster_attempt.load(std::sync::atomic::Ordering::Acquire) == 2
    }

    pub fn previous_agent_faults(
        &self, index: u64, history: bool, subjects: &[String],
    ) -> BTreeMap<String, Value> {
        let subjects = subjects.iter().map(String::as_str).collect::<HashSet<_>>();
        let cache = self.smalltalk.agent_resources_cache.lock().expect("agent resources cache poisoned");
        let mut faults = BTreeMap::new();
        for entry in cache.iter().rev().filter(|entry| entry.index <= index && entry.history == history) {
            for card in entry.items.iter() {
                if let Some(id) = card["id"].as_str()
                    && subjects.contains(id)
                    && !faults.contains_key(id) {
                    faults.insert(id.to_owned(), card["fault"].clone());
                }
            }
            if faults.len() == subjects.len() { break; }
        }
        faults
    }

    /// An agents WS window inherits the queue deadline from its selected or complete projection.
    pub fn agent_roster_valid_until(&self, index: u64) -> Option<u128> {
        self.smalltalk.agent_resources_cache.lock()
            .expect("agent resources cache poisoned").iter().rev()
            .find(|entry| entry.index == index && !entry.history)
            .and_then(|entry| entry.valid_until_unix_ms)
    }

    /// Register the one task that keeps the complete roster published, and return its wake.
    /// `None` when one is already registered.
    pub fn start_agent_roster_refresher(&self) -> Option<Arc<tokio::sync::Notify>> {
        let wake = Arc::new(tokio::sync::Notify::new());
        self.smalltalk.agent_roster_refresh.set(Arc::clone(&wake)).ok()?;
        Some(wake)
    }

    /// `None` when a complete roster at `index` takes folding at most `bound` cards: the
    /// newest rows, complete or partial, say which cards changed since, and together with the
    /// agents they do not cover yet those are no more than `bound`. Otherwise why not. Read in
    /// the snapshot that folds.
    pub fn agent_roster_unbounded_because(
        &self,
        index: u64,
        history: bool,
        bound: usize,
    ) -> Result<Option<String>> {
        let previous = self.smalltalk.agent_resources_cache.lock()
            .expect("agent resources cache poisoned").iter()
            .filter(|entry| entry.history == history && entry.index <= index)
            .max_by_key(|entry| (entry.index, entry.local)).cloned();
        // Without rows to start from, every agent is missing: a small roster, or an empty
        // one, still completes in one bounded fold.
        let (changed, membership) = match &previous {
            None => (0, true),
            Some(previous) if previous.index == index => (self.agent_roster_fault_retries(history).len(), false),
            Some(previous) => match self.changed_agent_resources(previous.index, index, &previous.items)? {
                AgentResourcesChange::Delta(mut delta) => {
                    delta.subjects.extend(self.agent_roster_fault_retries(history));
                    (delta.subjects.len(), delta.membership)
                }
                AgentResourcesChange::Invalidated { detail, .. } => return Ok(Some(detail)),
            },
        };
        // Complete rows whose membership no claim moved miss no agent: skip listing them all.
        if previous.as_ref().is_some_and(|previous| previous.covered.is_none()) && !membership {
            return Ok((changed > bound).then(|| "cards changed".to_owned()));
        }
        let connection = self.readers.get();
        let names = connection.prepare_cached(RANGE_SUBJECTS)?
            .query_map(params![index, "agent/", "agent0"], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<BTreeSet<_>>>()?;
        let names = if history { names } else {
            self.current_view_candidates(&connection, names, index, true)?
        };
        drop(connection);
        let folded = previous.iter().flat_map(|previous| previous.items.iter())
            .filter_map(|item| item["id"].as_str()).collect::<HashSet<_>>();
        let missing = names.iter().filter(|name| !folded.contains(name.as_str())).count();
        Ok((changed.saturating_add(missing) > bound).then(|| {
            if missing > changed { "cards missing" } else { "cards changed" }.to_owned()
        }))
    }

    /// Count one roster assembled in chunks because no short fold could complete it, by why.
    pub fn note_agent_roster_chunked(&self, reason: &str) {
        crate::performance::record_request("roster/chunked-assembly", None, std::time::Duration::ZERO);
        *self.smalltalk.agent_roster_chunked.lock().expect("roster counts poisoned")
            .entry(reason.to_owned()).or_default() += 1;
    }

    /// How many rosters were assembled in chunks, by why: each folded every card again.
    pub fn agent_roster_chunked_assemblies(&self) -> BTreeMap<String, u64> {
        self.smalltalk.agent_roster_chunked.lock().expect("roster counts poisoned").clone()
    }

    /// Whether the newest complete roster already is the one at `index`: same graph cut,
    /// same local activity, and no queue deadline passed since. A refresh would fold nothing.
    pub fn agent_roster_current(&self, index: u64, history: bool) -> Result<bool> {
        if self.agent_roster_faults_pending(history) { return Ok(false); }
        let local = roster_local_frontier(&self.readers.get(), index)?;
        let now = now_ms();
        Ok(self.smalltalk.agent_resources_cache.lock()
            .expect("agent resources cache poisoned").iter()
            .any(|entry| entry.history == history && entry.covered.is_none()
                && entry.index == index && entry.local == local
                && entry.valid_until_unix_ms.is_none_or(|expiry| now < expiry)))
    }

    /// The unchanged-card proof for the exact complete publication a response selected,
    /// rather than the newest one when the proof happens to run. No fold or write occurs.
    /// Keep the original wait predicate above unchanged: this is an additional observation
    /// for the served rows, not a new freshness/refresh admission policy.
    pub fn served_agent_roster_unchanged_through(
        &self,
        through: u64,
        history: bool,
        cut: u64,
        published_at: u128,
        cards: &Arc<Vec<Value>>,
    ) -> Result<bool> {
        if self.smalltalk.agent_roster_refresh.get().is_none()
            || self.agent_roster_faults_pending(history)
        {
            return Ok(false);
        }
        self.read_snapshot(|physical| {
            if cut > through || through > physical {
                return Ok(false);
            }
            let Some((local, valid_until)) = self
                .smalltalk
                .agent_resources_cache
                .lock()
                .expect("agent resources cache poisoned")
                .iter()
                .find(|entry| {
                    entry.history == history
                        && entry.covered.is_none()
                        && entry.index == cut
                        && entry.published_at_unix_ms == published_at
                        && Arc::ptr_eq(&entry.items, cards)
                })
                .map(|entry| (entry.local, entry.valid_until_unix_ms))
            else {
                return Ok(false);
            };
            if valid_until.is_some_and(|expiry| now_ms() >= expiry)
                || roster_local_frontier(&self.readers.get(), through)? != local
            {
                return Ok(false);
            }
            if cut == through {
                return Ok(true);
            }
            Ok(match self.changed_agent_resources(cut, through, cards)? {
                AgentResourcesChange::Delta(delta) => {
                    delta.subjects.is_empty() && !delta.queues && !delta.membership
                }
                AgentResourcesChange::Invalidated { .. } => false,
            })
        })
    }

    /// Note that an agents list read asked for the roster now, so a forget refolds it at once.
    pub fn note_agent_roster_read(&self) {
        self.smalltalk.agent_roster_read_at.store(now_ms() as u64, std::sync::atomic::Ordering::Release);
    }

    /// Ask the refresher, if one runs, to publish the roster at the newest cut. Requests made
    /// while it folds coalesce into one more refresh.
    pub fn request_agent_roster_refresh(&self) {
        if let Some(wake) = self.smalltalk.agent_roster_refresh.get() {
            let _ = self.smalltalk.agent_roster_requested_at.compare_exchange(
                0, now_ms() as u64, std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Relaxed,
            );
            wake.notify_one();
        }
    }

    /// Ask the refresher, if one runs, to also publish the history roster.
    pub fn request_agent_roster_history(&self) {
        self.smalltalk.agent_roster_history_wanted.store(true, std::sync::atomic::Ordering::Release);
        self.request_agent_roster_refresh();
    }

    /// Ask the refresher, if one runs, for a roster at the newest cut on behalf of a reader that
    /// waits for it: a refresher pausing between refreshes stops pausing once it has paused as
    /// long as its last refresh took, and at least a tenth of a second.
    pub fn request_fresh_agent_roster(&self, history: bool) {
        if self.smalltalk.agent_roster_refresh.get().is_none() {
            return;
        }
        self.smalltalk.agent_roster_fresh_wanted.notify_one();
        if history {
            self.request_agent_roster_history();
        } else {
            self.request_agent_roster_refresh();
        }
    }

    /// Resolves once a reader waits for a fresh roster, including one that asked before this
    /// was called and was not yet answered by a refresh's pause.
    pub async fn fresh_agent_roster_wanted(&self) {
        self.smalltalk.agent_roster_fresh_wanted.notified().await;
    }

    /// Whether a reader asked for the history roster since the last time this was taken.
    pub fn take_agent_roster_history_request(&self) -> bool {
        self.smalltalk.agent_roster_history_wanted.swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    /// Say once when a refresh request has gone unanswered so long that the refresher must be
    /// slow, failing or stopped. Readers keep serving the newest publication, which carries its
    /// own cut and publication time, rather than refusing rows that exist.
    pub fn warn_if_agent_roster_overdue(&self) {
        let requested = self.smalltalk.agent_roster_requested_at.load(std::sync::atomic::Ordering::Acquire);
        let waited = (now_ms() as u64).saturating_sub(requested);
        if requested != 0 && waited > AGENT_ROSTER_OVERDUE_MS
            && !self.smalltalk.agent_roster_overdue_warned.swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            eprintln!("st3: WARN an agents roster refresh asked for {} s ago is still unanswered; \
                serving the newest published roster", waited / 1000);
        }
    }

    /// Follows the revision of complete current roster publications: it rises with every one,
    /// including a replacement at the same graph index after local activity or a deadline.
    pub fn subscribe_agent_roster(&self) -> tokio::sync::watch::Receiver<u64> {
        self.smalltalk.agent_roster_published.subscribe()
    }

    /// The revision `subscribe_agent_roster` follows, now.
    pub fn agent_roster_revision(&self) -> u64 {
        *self.smalltalk.agent_roster_published.borrow()
    }

    /// Queue selection changes at lease expiry even when the claim frontier is unchanged.
    /// Scan only on a cache miss, inside the same SQLite snapshot as the queue projection.
    pub fn agent_queue_valid_until(&self, now: u128) -> Result<Option<u128>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT lease_expires_at_unix_ms FROM step_runs
             WHERE agentless=0 AND status IN ('claimed','working','verifying')
               AND status NOT IN ('completed','failed','cancelled')
               AND lease_expires_at_unix_ms IS NOT NULL
               AND generation_id=(SELECT current_generation_id FROM mission_runs WHERE id=step_runs.run_id)",
        )?;
        let mut deadline = None;
        for row in statement.query_map([], |row| row.get::<_, String>(0))? {
            if let Some(expiry) = row?.parse::<u128>().ok().filter(|expiry| *expiry > now) {
                deadline = Some(deadline.map_or(expiry, |previous: u128| previous.min(expiry)));
            }
        }
        Ok(deadline)
    }

    /// Membership/order/queue refs contain no local timeline data. Reuse them for unchanged
    /// graph cuts and agent-only observations, so a warm HTTP page does not scan fleet work.
    pub fn cached_agent_page_refs(
        &self,
        index: u64,
        history: bool,
        build: impl FnOnce() -> Result<Vec<Value>>,
    ) -> Result<Vec<Value>> {
        let now = now_ms();
        let valid = |entry: &&runtime::AgentResourcesEntry| {
            entry.valid_until_unix_ms.is_none_or(|expiry| now < expiry)
        };
        let cache = self.smalltalk.agent_page_refs_cache.lock()
            .expect("agent page refs cache poisoned");
        if let Some(entry) = cache.iter().filter(valid).find(|entry| entry.index == index && entry.history == history) {
            let items = Arc::clone(&entry.items);
            drop(cache);
            return Ok((*items).clone());
        }
        let previous = cache.iter().filter(valid).filter(|entry| entry.index < index && entry.history == history)
            .max_by_key(|entry| entry.index).cloned();
        drop(cache);
        let (items, valid_until_unix_ms) = match previous {
            Some(previous) if self.agent_page_refs_unchanged(previous.index, index, &previous.items)? => {
                (previous.items, previous.valid_until_unix_ms)
            }
            _ => (Arc::new(build()?), self.agent_queue_valid_until(now)?),
        };
        let mut cache = self.smalltalk.agent_page_refs_cache.lock()
            .expect("agent page refs cache poisoned");
        cache.push_back(runtime::AgentResourcesEntry {
            index, local: 0, history, covered: None, valid_until_unix_ms, items: Arc::clone(&items),
            published_at_unix_ms: now,
        });
        let evicted = if cache.len() > 8 { cache.pop_front() } else { None };
        drop(cache);
        drop(evicted);
        Ok((*items).clone())
    }

    /// Bounded immutable projections shared by pages and streams. Delta claims, membership
    /// differences, local activity and queue deadlines refold only affected cards.
    pub fn cached_agent_resources_for(
        &self,
        index: u64,
        history: bool,
        selected: Option<&BTreeSet<String>>,
        build: impl FnOnce(Option<(&BTreeSet<String>, &[Value])>) -> Result<Vec<Value>>,
    ) -> Result<Vec<Value>> {
        self.cached_agent_resources_folding(index, history, selected, false, build)
    }

    /// [`Self::cached_agent_resources_for`] for one chunk of a roster assembled in short folds:
    /// it refolds only the `selected` agents. Changed cards outside them leave the published
    /// rows' coverage and fold with their own chunk, so one call folds at most `selected`.
    pub fn cached_agent_resources_chunk(
        &self,
        index: u64,
        history: bool,
        selected: &BTreeSet<String>,
        build: impl FnOnce(Option<(&BTreeSet<String>, &[Value])>) -> Result<Vec<Value>>,
    ) -> Result<Vec<Value>> {
        self.cached_agent_resources_folding(index, history, Some(selected), true, build)
    }

    pub fn cached_agent_resources_folding(
        &self,
        index: u64,
        history: bool,
        selected: Option<&BTreeSet<String>>,
        chunk: bool,
        build: impl FnOnce(Option<(&BTreeSet<String>, &[Value])>) -> Result<Vec<Value>>,
    ) -> Result<Vec<Value>> {
        // Cold presentation reads current desired/queue tables even for historical status
        // cuts. Do not reuse rows from an older physical projection for those requests.
        if index < current_index(&self.readers.get())? {
            let _rebuild = self.start_roster_rebuild("historical_snapshot", index);
            let mut items = crate::performance::task("roster/card-projection",
                || build(selected.map(|names| (names, &[][..]))))?;
            #[cfg(any(test, feature = "test-support"))]
            self.count_refolded_cards_for_test(items.len());
            items.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str())
                .then_with(|| a["id"].as_str().cmp(&b["id"].as_str())));
            crate::otel::record_roster("cold", items.len());
            record_roster_counts(items.len(), items.len(), items.len());
            crate::otel::roster_attribute("st.roster.publication_cut", index as i64);
            return Ok(items);
        }
        let now = now_ms();
        let valid = |entry: &&runtime::AgentResourcesEntry| {
            entry.valid_until_unix_ms.is_none_or(|expiry| now < expiry)
        };
        // Local agent timeline rows do not advance the graph index, but do change cards; the
        // frontier ignores heartbeats and every other local kind, none of which a card reads.
        // Read it inside the caller's SQLite snapshot, never from a future atomic generation
        // that could race this cut.
        let local = crate::performance::task("roster/frontier-read", || {
            roster_local_frontier(&self.readers.get(), index)
        })?;
        let select = |items: &[Value]| items.iter().filter(|item| {
            selected.is_none_or(|names| names.contains(item["id"].as_str().unwrap_or_default()))
        }).cloned().collect::<Vec<_>>();
        let fault_retries = self.agent_roster_fault_retries(history);
        let cache = self.smalltalk.agent_resources_cache.lock()
            .expect("agent resources cache poisoned");
        if let Some(entry) = cache.iter().rev().filter(valid)
            .find(|entry| entry.index == index && entry.local == local && entry.history == history)
            && agent_resources_entry_hits(entry, now, index, local, history, selected)
            && selected.map_or(fault_retries.is_empty(), |names| names.is_disjoint(&fault_retries)) {
            let items = Arc::clone(&entry.items);
            drop(cache);
            return crate::performance::task("roster/cache-hit", || {
                let items = select(&items);
                crate::otel::record_roster("hit", items.len());
                Ok(items)
            });
        }
        // Expired entries are diff sources only, never hits: refreshing their queue metadata
        // is sufficient to discover the cards moved by a deadline without dropping the fleet.
        let previous = cache.iter()
            .filter(|entry| entry.index <= index && entry.local <= local && entry.history == history)
            .max_by_key(|entry| (entry.index, entry.local)).cloned();
        drop(cache);
        let entry = crate::performance::task("roster/build",
        || -> Result<runtime::AgentResourcesEntry> {
        let (previous, invalidation) = match previous {
            Some(entry) if entry.index == index => (Some((entry, AgentResourcesDelta::default())), "safe_delta"),
            Some(entry) => match self.changed_agent_resources(entry.index, index, &entry.items)? {
                AgentResourcesChange::Delta(delta) => (Some((entry, delta)), "safe_delta"),
                AgentResourcesChange::Invalidated { reason, .. } => (None, reason),
            },
            None => (None, "cold"),
        };
        match previous {
            Some((previous, delta)) => {
                if crate::otel::export_enabled() {
                let reason = if delta.membership { "membership" }
                    else if previous.local != local { "local_frontier" }
                    else if previous.valid_until_unix_ms.is_some_and(|expiry| now >= expiry) { "queue_deadline" }
                    else if previous.covered.as_ref().is_some_and(|covered| {
                        selected.is_none_or(|names| !names.is_subset(covered))
                    }) { "coverage_gap" }
                    else { "safe_delta" };
                crate::otel::roster_attribute("st.roster.invalidation", reason);
                }
                let mut changed = delta.subjects;
                changed.extend(fault_retries.iter().cloned());
                if delta.membership {
                    crate::otel::roster_phase("membership", || -> Result<()> {
                    let connection = self.readers.get();
                    let names = connection.prepare_cached(RANGE_SUBJECTS)?
                        .query_map(params![index, "agent/", "agent0"], |row| row.get::<_, String>(0))?
                        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
                    let mut names = if history { names } else {
                        self.current_view_candidates(&connection, names, index, true)?
                    };
                    if let Some(covered) = &previous.covered {
                        names.retain(|name| covered.contains(name));
                    }
                    let old = previous.items.iter().filter_map(|item| item["id"].as_str().map(str::to_owned))
                        .collect::<BTreeSet<_>>();
                    changed.extend(names.symmetric_difference(&old).cloned());
                    Ok(())
                    })?;
                }
                if previous.local != local {
                    let connection = self.readers.get();
                    let mut statement = connection.prepare_cached(
                        "SELECT DISTINCT subject FROM local_observations WHERE id>?1 AND id<=?2
                         AND after_store_index<=?3 AND kind='harness.timeline' AND subject LIKE 'agent/%'",
                    )?;
                    changed.extend(statement.query_map(params![previous.local, local, index],
                        |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<BTreeSet<_>>>()?);
                }
                let mut covered = crate::otel::roster_phase("membership", || -> Result<Option<BTreeSet<String>>> {
                // A chunk treats complete rows as covering exactly the agents they hold.
                let previous_covered: Option<BTreeSet<String>> = match &previous.covered {
                    None if chunk => Some(previous.items.iter()
                        .filter_map(|item| item["id"].as_str().map(str::to_owned)).collect()),
                    covered => covered.clone(),
                };
                Ok(match (&previous_covered, selected) {
                    (None, _) => None,
                    (Some(covered), Some(names)) => {
                        changed.retain(|name| covered.contains(name));
                        changed.extend(names.difference(covered).cloned());
                        Some(covered.union(names).cloned().collect())
                    }
                    (Some(covered), None) => {
                        let connection = self.readers.get();
                        let names = connection.prepare_cached(RANGE_SUBJECTS)?
                            .query_map(params![index, "agent/", "agent0"], |row| row.get::<_, String>(0))?
                            .collect::<rusqlite::Result<BTreeSet<_>>>()?;
                        let names = if history { names } else {
                            self.current_view_candidates(&connection, names, index, true)?
                        };
                        changed.extend(names.difference(covered).cloned());
                        None
                    }
                }) })?;
                let refresh_queues = delta.queues
                    || previous.valid_until_unix_ms.is_some_and(|expiry| now >= expiry)
                    || changed.iter().any(|name| !previous.items.iter().any(|item| item["id"].as_str() == Some(name.as_str())));
                let mut queue_metadata = None;
                let valid_until_unix_ms = if refresh_queues {
                    let names = previous.items.iter().filter_map(|item| item["id"].as_str().map(str::to_owned))
                        .chain(changed.iter().cloned()).collect::<BTreeSet<_>>();
                    let metadata = crate::performance::task("roster/queue-diff",
                        || agent_queue_values::agent_queue_metadata(self, &names))?;
                    let by_id = metadata.iter().map(|item| (item["id"].as_str().unwrap_or_default(), item))
                        .collect::<BTreeMap<_, _>>();
                    for old in previous.items.iter() {
                        let name = old["id"].as_str().unwrap_or_default();
                        if let Some(new) = by_id.get(name)
                            && agent_queue_values::AGENT_QUEUE_FIELDS.iter().any(|field| old[*field] != new[*field]) {
                            changed.insert(name.to_owned());
                        }
                    }
                    queue_metadata = Some(metadata);
                    self.agent_queue_valid_until(now)?
                } else {
                    previous.valid_until_unix_ms
                };
                // A chunk defers changed cards outside it: they leave the coverage and the rows.
                let mut deferred = BTreeSet::new();
                if chunk && let (Some(names), Some(covered)) = (selected, covered.as_mut()) {
                    deferred = changed.difference(names).cloned().collect::<BTreeSet<_>>();
                    changed.retain(|name| names.contains(name));
                    covered.retain(|name| !deferred.contains(name));
                }
                // Coverage gaps join `changed`, so an empty set means no card this cut can see
                // moved at all: the previous rows already are this cut's projection, and the
                // heartbeat or fleet-only claim between two reads costs no clone, sort or build.
                record_roster_counts(changed.len() + deferred.len(), covered.as_ref().map_or(previous.items.len(), BTreeSet::len), 0);
                if changed.is_empty() && deferred.is_empty() {
                    crate::otel::record_roster("incremental", previous.items.len());
                    return Ok(runtime::AgentResourcesEntry {
                        index, local, history, covered,
                        valid_until_unix_ms,
                        items: Arc::clone(&previous.items),
                        published_at_unix_ms: now_ms(),
                    });
                }
                #[cfg(any(test, feature = "test-support"))]
                self.smalltalk.agent_resources_builds.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut items = previous.items.iter()
                    .filter(|item| {
                        let id = item["id"].as_str().unwrap_or_default();
                        !changed.contains(id) && !deferred.contains(id)
                    })
                    .cloned().collect::<Vec<_>>();
                let rebuilt = crate::performance::task("roster/card-projection",
                    || build(Some((&changed, queue_metadata.as_deref().unwrap_or(&previous.items)))))?;
                record_roster_counts(changed.len() + deferred.len(), covered.as_ref().map_or(items.len() + rebuilt.len(), BTreeSet::len), rebuilt.len());
                #[cfg(any(test, feature = "test-support"))]
                self.count_refolded_cards_for_test(rebuilt.len());
                items.extend(rebuilt);
                items.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str())
                    .then_with(|| a["id"].as_str().cmp(&b["id"].as_str())));
                crate::otel::record_roster("incremental", items.len());
                Ok(runtime::AgentResourcesEntry {
                    index, local, history, covered,
                    valid_until_unix_ms,
                    items: Arc::new(items),
                    published_at_unix_ms: now_ms(),
                })
            }
            None => {
                let _rebuild = self.start_roster_rebuild(invalidation, index);
                #[cfg(any(test, feature = "test-support"))]
                self.smalltalk.agent_resources_builds.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut items = crate::performance::task("roster/card-projection",
                    || build(selected.map(|names| (names, &[][..]))))?;
                #[cfg(any(test, feature = "test-support"))]
                self.count_refolded_cards_for_test(items.len());
                items.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str())
                    .then_with(|| a["id"].as_str().cmp(&b["id"].as_str())));
                crate::otel::record_roster("cold", items.len());
                record_roster_counts(items.len(), items.len(), items.len());
                crate::otel::roster_attribute("st.roster.publication_cut", index as i64);
                Ok(runtime::AgentResourcesEntry {
                    index, local, history, covered: selected.cloned(),
                    valid_until_unix_ms: self.agent_queue_valid_until(now)?,
                    items: Arc::new(items),
                    published_at_unix_ms: now_ms(),
                })
            }
        }
        })?;
        let mut cache = self.smalltalk.agent_resources_cache.lock()
            .expect("agent resources cache poisoned");
        // All endpoint callers hold admission. Direct internal readers may still race; never
        // replace a complete published projection with a partial one.
        let published = cache.iter().rev().filter(valid)
            .find(|entry| entry.index == index && entry.local == local && entry.history == history)
            .filter(|entry| agent_resources_entry_hits(entry, now, index, local, history, selected)
                && selected.map_or(fault_retries.is_empty(), |names| names.is_disjoint(&fault_retries)))
            .map(|entry| Arc::clone(&entry.items));
        let complete = entry.covered.is_none();
        let items = if let Some(published) = published { published } else {
            // Fault retries can assemble partial chunks at the publication's exact cut.
            // Keep that complete publication readable until the replacement is complete.
            cache.retain(|old| old.index != index || old.local != local || old.history != history
                || (!complete && old.covered.is_none()));
            let items = Arc::clone(&entry.items);
            cache.push_back(entry);
            if cache.len() > 8 {
                // Keep the newest complete publication of each mode, even while the other
                // mode republishes or this one is still assembling partial chunks.
                let oldest = cache.iter().position(|entry| entry.covered.is_some()
                    || cache.iter().any(|newer| newer.history == entry.history
                        && newer.covered.is_none()
                        && (newer.index, newer.local) > (entry.index, entry.local)))
                    .expect("only two complete roster publications can be protected");
                cache.remove(oldest);
            }
            items
        };
        drop(cache);
        if complete {
            self.smalltalk.agent_roster_published.send_modify(|revision| *revision += 1);
        }
        Ok(select(&items))
    }

    /// The warm pin a shared roster read checks before it admits a builder: the exact cached
    /// rows for this graph cut, roster-relevant local frontier, history mode and selected
    /// coverage, or `None` when only a build can answer. Never builds; a miss must drop its
    /// SQLite snapshot before waiting for admission. The rows are the complete unfiltered
    /// projection at the cut, so a page still selects its own subjects from them.
    pub fn agent_resources_cached_at(
        &self,
        index: u64,
        history: bool,
        selected: Option<&BTreeSet<String>>,
    ) -> Result<Option<Arc<Vec<Value>>>> {
        Ok(self.agent_resources_published_at(index, history, selected)?.map(|(items, _)| items))
    }

    /// The matching warm pin for page refs: membership, ordering and queue metadata for exactly
    /// this graph cut and history mode while the queue lease window still holds, or `None`.
    /// Refs carry no local timeline data, so no frontier is read. Never builds.
    pub fn agent_page_refs_cached_at(
        &self,
        index: u64,
        history: bool,
    ) -> Option<Arc<Vec<Value>>> {
        let now = now_ms();
        self.smalltalk.agent_page_refs_cache.lock()
            .expect("agent page refs cache poisoned")
            .iter()
            .find(|entry| entry.index == index && entry.history == history
                && entry.valid_until_unix_ms.is_none_or(|expiry| now < expiry))
            .map(|entry| Arc::clone(&entry.items))
    }

    /// One seat's current claim, its queued runs in order, and its move history.
    pub fn seat_queue(&self, agent: &str) -> Result<SeatQueueView> {
        let agent = normalize_seat(agent);
        let (inputs, order) = {
            let connection = self.readers.get();
            let mut inputs = seat_queue_inputs_tx(&connection, Some(&agent))?;
            let inputs = inputs.remove(&agent).unwrap_or_default();
            let order = inputs.live_order();
            (inputs, order)
        };
        let work = self
            .open_work_for_seat(&agent)?
            .into_iter()
            .filter(|step| !matches!(step.status.as_str(), "completed" | "failed" | "cancelled"))
            .collect::<Vec<_>>();
        let steps = work.iter().map(SeatStep::from).collect::<Vec<_>>();
        let selection = seat_queue::select(&agent, &steps, &order);
        let runs = order
            .iter()
            .enumerate()
            .map(|(index, run)| {
                let in_run = |step: &&StepRunView| step.run == *run;
                let claimed_work_ids = work
                    .iter()
                    .filter(in_run)
                    .filter(|step| selection.held.contains(&step.subject.as_str()))
                    .map(|step| step.subject.clone())
                    .collect::<Vec<_>>();
                let ready_work_ids = selection
                    .ready
                    .iter()
                    .filter(|subject| {
                        work.iter()
                            .any(|step| step.subject == **subject && step.run == *run)
                    })
                    .map(|subject| (*subject).to_owned())
                    .collect::<Vec<_>>();
                let waiting_work_ids = work
                    .iter()
                    .filter(in_run)
                    .filter(|step| {
                        step.assigned_to.as_deref() == Some(agent.as_str())
                            && matches!(step.status.as_str(), "pending" | "blocked")
                    })
                    .map(|step| step.subject.clone())
                    .collect::<Vec<_>>();
                let state = if !claimed_work_ids.is_empty() {
                    "claimed"
                } else if !ready_work_ids.is_empty() {
                    "ready"
                } else {
                    "waiting"
                };
                let waiting_for = if state == "waiting" {
                    awaited_mission_run_tx(&self.readers.get(), run)?
                } else {
                    None
                };
                Ok(SeatQueueRunView {
                    run: run.clone(),
                    position: u32::try_from(index + 1).unwrap_or(u32::MAX),
                    state: state.into(),
                    run_status: inputs.statuses.get(run).cloned().unwrap_or_default(),
                    joined_at_unix_ms: inputs
                        .joins
                        .iter()
                        .find(|join| join.run == *run)
                        .map_or(0, |join| join.at_unix_ms),
                    claimed_work_ids,
                    ready_work_ids,
                    waiting_work_ids,
                    waiting_for,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let moves = inputs
            .moves
            .iter()
            .rev()
            .take(SEAT_QUEUE_MOVE_HISTORY_LIMIT)
            .map(|recorded| recorded.view.clone())
            .collect();
        Ok(SeatQueueView {
            agent: agent.clone(),
            current_work_ids: selection.held.iter().map(|id| (*id).to_owned()).collect(),
            next_work_id: selection.next().map(str::to_owned),
            runs,
            moves,
            move_count: inputs.moves.len() as u64,
        })
    }

    /// A reconciler may defer an agent it parsed from an owned mission declaration.
    /// Messages can still address that declared seat before it enters the desired graph.
    /// This evidence is local to admission; it adds no field to any persisted intent.
    pub fn apply_internal_with_declared_agents(
        &self,
        intent: &NormalizedIntent,
        key: &str,
        declared_agents: &BTreeSet<String>,
    ) -> Result<ApplyResponse, St3Error> {
        let source = IntentInput {
            kdl: String::new(),
            source_name: Some("st reconciler".into()),
        };
        let mission = self.mission_at_with_declared_agents(intent, source, None, declared_agents)?;
        if !mission.blockers.is_empty() {
            return Err(St3Error::new(
                "internal-mission-blocked",
                mission.blockers.join("; "),
            ));
        }
        // A repair can remove a desired claim from the effective leaf set. The leaf set can
        // then equal an older snapshot whose cached response reported `changed: true`. Include
        // the monotonic store index so convergence never replays that stale success response.
        let materialization = serde_json::to_vec(&(
            key,
            mission.store_index,
            &mission.normalized,
            &mission.subject_tokens,
        ))
        .map_err(internal)?;
        let key = format!(
            "internal:{key}:{}",
            hex::encode(Sha256::digest(materialization))
        );
        self.apply(intent, &mission.subject_tokens, &key)
    }

    /// Append a native driver's observation, as `POST /v1/harness-events` does once it has
    /// bound the caller to its seat. The daemon tests call it directly, without a driver.
    pub fn append_harness_event(
        &self,
        publication: &crate::harness_events::Publication,
    ) -> Result<(ClaimRecord, bool), St3Error> {
        self.append_harness_event_publication(publication)
            .map(|(record, appended, _)| (record, appended))
    }

    /// A replicated snapshot of the local response totals affected by one timeline entry.
    /// The response itself stays in the local observation log.
    pub fn usage_rollup_for_timeline(
        &self,
        observation: &ClaimRecord,
    ) -> Result<Option<ClaimInput>> {
        if observation.kind != "harness.timeline" {
            return Ok(None);
        }
        let fields = &observation.body["fields"];
        if fields["entry_type"] != "usage" || fields["body"]["semantics"] != "response" {
            return Ok(None);
        }
        let incarnation = fields["incarnation_id"].as_str().unwrap_or("");
        let model = fields["body"]["model"].as_str().unwrap_or("unknown");
        let account = fields["body"]["account"].as_str().unwrap_or("");
        let owner_run = fields["attribution"]["mission_run_id"]
            .as_str()
            .unwrap_or("");
        let owner_step = fields["attribution"]["step_id"].as_str().unwrap_or("");
        let host = fields["host"].as_str().unwrap_or(&self.origin);
        let connection = self.readers.get();
        let totals = connection
            .query_row(
                "SELECT input_tokens, output_tokens, cache_write_tokens, cache_write_1h_tokens,
                    cached_tokens, total_tokens, cost_microusd, reported_cost_microusd,
                    unpriced_tokens, observed_at_unix_ms
                 FROM local_usage_spend WHERE subject=?1 AND incarnation_id=?2 AND model=?3
                    AND account=?4 AND owner_run=?5 AND owner_step=?6 AND host=?7",
                params![
                    observation.subject,
                    incarnation,
                    model,
                    account,
                    owner_run,
                    owner_step,
                    host
                ],
                |row| {
                    (0..10)
                        .map(|index| row.get::<_, u64>(index))
                        .collect::<Result<Vec<_>, _>>()
                },
            )
            .optional()?;
        let Some(totals) = totals else {
            return Ok(None);
        };
        let mut fields = BTreeMap::from([
            ("semantics".into(), Value::String("response_rollup".into())),
            ("driver".into(), fields["driver"].clone()),
            ("incarnation_id".into(), Value::String(incarnation.into())),
            ("model".into(), Value::String(model.into())),
            ("owner_run".into(), Value::String(owner_run.into())),
            ("owner_step".into(), Value::String(owner_step.into())),
            ("host".into(), Value::String(host.into())),
            (
                "pricing".into(),
                Value::String(crate::pricing::PRICING_REVISION.into()),
            ),
        ]);
        if !account.is_empty() {
            fields.insert("account".into(), Value::String(account.into()));
        }
        for (name, value) in [
            "input_tokens",
            "output_tokens",
            "cache_write_tokens",
            "cache_write_1h_tokens",
            "cached_tokens",
            "total_tokens",
            "cost_microusd",
            "reported_cost_microusd",
            "unpriced_tokens",
            "observed_at_unix_ms",
        ]
        .into_iter()
        .zip(totals)
        {
            fields.insert(name.into(), Value::from(value));
        }
        let provenance: Option<String> = connection
            .query_row(
                "SELECT pricing FROM local_usage_provenance WHERE subject=?1 AND slot=?2",
                params![observation.subject, usage_slot(&fields)],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(provenance) = provenance {
            fields.insert(
                "pricing_provenance".into(),
                serde_json::from_str(&provenance)?,
            );
        }
        // Never use the seat's current binding for a response from an older incarnation.
        if let Some(binding) = usage_native_session(&connection, &observation.subject, &fields)? {
            fields.insert("native_session_id".into(), Value::String(binding));
        }
        let digest = hex::encode(Sha256::digest(serde_json::to_vec(&fields)?));
        Ok(Some(ClaimInput {
            subject: observation.subject.clone(),
            kind: "harness.usage".into(),
            actor: Some(observation.subject.clone()),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("usage-rollup:{}:{digest}", observation.subject)),
        }))
    }

    /// Forget the responses counted before `older_than_unix_ms`, in short transactions of at
    /// most `chunk` rows using background writer loans. Ingest refuses responses that old, so
    /// none can be counted twice.
    pub fn trim_usage_responses(&self, older_than_unix_ms: u128, chunk: usize) -> Result<usize> {
        let chunk = chunk.max(1).min(i64::MAX as usize) as i64;
        let mut deleted = 0;
        loop {
            let connection = self.connection.write_background();
            let removed = connection.execute(
                "DELETE FROM local_usage_responses WHERE rowid IN (
                    SELECT rowid FROM local_usage_responses WHERE observed_at_unix_ms < ?1 LIMIT ?2
                 )",
                params![older_than_unix_ms.min(i64::MAX as u128) as i64, chunk],
            )?;
            drop(connection);
            deleted += removed;
            if (removed as i64) < chunk {
                return Ok(deleted);
            }
        }
    }

    /// Only a seat's current harness, as `status_at` reports it, for the one caller that reads
    /// nothing else of it: a driver renewing its seat's work leases each minute. The full status
    /// also returns every claim id of the subject, 18,000 ids (1.2 MB) for the busiest seat.
    pub fn status_harness_only(&self, subject: &str) -> Result<StatusResponse> {
        let connection = self.readers.get();
        let store_index = current_index(&connection)?;
        let harness = current_harness_at(&connection, subject, None)?;
        Ok(StatusResponse {
            store_index,
            subjects: vec![SubjectStatus {
                subject: subject.to_owned(),
                kind: None,
                desired_token: None,
                desired_revision: None,
                desired: None,
                actual: None,
                actual_claim: None,
                actual_origin: None,
                harness,
                conflicts: Vec::new(),
                claims: Vec::new(),
                claims_summary: ClaimsSummary::default(),
                owner_run: None,
                gap: None,
                reachability: "unknown".into(),
                reason: None,
                under: Vec::new(),
                projection: OperationalAnnotation::default(),
            }],
            pending_actions: Vec::new(),
        })
    }

    /// The terminals collection consumes the same authority fields as full status, but
    /// discards harness history, claim provenance and desired conflicts. Reuse the capped
    /// card reduction for this read only; runtime control still uses full status.
    pub fn terminal_resource_status_at(
        &self,
        owner: Option<&str>,
        index: u64,
        history: bool,
    ) -> Result<StatusResponse> {
        let connection = self.readers.get();
        let index =
            selected_index(current_index(&connection)?, Some(index)).map_err(anyhow::Error::new)?;
        let mut names = match owner {
            Some(owner) => BTreeSet::from([owner.to_owned()]),
            None => connection
                .prepare_cached(RUNTIME_SUBJECTS)?
                .query_map([index], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<BTreeSet<_>>>()?,
        };
        // Match the public status reducer's exclusions, including exact-owner reads.
        names.retain(|name| !name.starts_with("glass/") && !name.starts_with("arrangement/"));
        drop(connection);
        self.agent_card_status_at(Some(&names), index, history)
    }

    pub fn desired_harness_login_candidates(&self) -> Result<Vec<DesiredSubject>> {
        smallclaims::touched::note_read(|| "desired-kind:agent".to_owned());
        smallclaims::touched::note_read(|| "kind:harness.observed".to_owned());
        smallclaims::touched::note_read(|| "kind:harness.diagnostic".to_owned());
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT subject, kind, body, member, owner_run, owner_generation, owner_step
             FROM desired WHERE kind='agent'
               AND EXISTS (SELECT 1 FROM claims INDEXED BY claims_harness_login_candidate_index
                 WHERE claims.subject=desired.subject AND (
                   (kind='harness.observed' AND (
                     json_type(body, '$.fields.provider_auth')='false'
                     OR json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
                         THEN '$.reason' ELSE '$.fields.reason' END)='providerAuth'
                     OR json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
                         THEN '$.state' ELSE '$.fields.state' END)='needs-login'))
                   OR (kind='harness.diagnostic'
                     AND json_extract(body, '$.fields.code')='provider-auth-expired')))
             ORDER BY subject",
        )?;
        statement
            .query_map([], desired_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Read an exact immutable declaration, or the revision selected by the desired projection.
    pub fn agent_declaration(
        &self,
        subject: &str,
        revision: Option<&str>,
    ) -> Result<Option<(String, Value)>> {
        let connection = self.readers.get();
        let row: Option<(String, String)> = connection
            .query_row(
                "SELECT c.id, c.body FROM claims c
                 WHERE c.subject=?1 AND c.kind='intent.desired'
                   AND c.id=COALESCE(?2, (SELECT d.claim_id FROM desired d WHERE d.subject=?1))",
                params![subject, revision],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        row.map(|(id, body)| {
            let desired: DesiredSubject = serde_json::from_str(&body)?;
            if desired.kind != "agent" {
                anyhow::bail!("declaration `{subject}` is not an agent");
            }
            Ok((id, desired.desired))
        })
        .transpose()
    }

    pub fn agent_declaration_revisions(&self, subject: &str) -> Result<Vec<String>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(&format!(
            "SELECT claims.id FROM claims WHERE claims.subject=?1 AND claims.kind='intent.desired'
             ORDER BY {CANONICAL_ORDER_DESC}"
        ))?;
        let rows = statement.query_map([subject], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Publish a presentation-only revision of a seat through the normal durable intent log.
    pub fn rename_agent(
        &self,
        subject: &str,
        name: Option<&str>,
        idempotency_key: &str,
    ) -> Result<ApplyResponse, St3Error> {
        if name == Some("") {
            return Err(St3Error::new("invalid-agent-name", "a seat label must be a non-empty string"));
        }
        let (mut desired, heads, writer) = {
            let connection = self.readers.get();
            let transaction = connection.unchecked_transaction().map_err(internal)?;
            let desired = transaction
                .query_row(
                    "SELECT desired.subject, desired.kind, desired.body, desired.member,
                            desired.owner_run, desired.owner_generation, desired.owner_step, claims.actor
                     FROM desired LEFT JOIN claims ON claims.id=desired.claim_id
                     WHERE desired.subject=?1 AND desired.kind='agent'",
                    [subject],
                    |row| Ok((
                        desired_from_row(row)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, Option<String>>(3)?,
                    )),
                )
                .optional()
                .map_err(internal)?
                .ok_or_else(|| St3Error::new("missing-agent", format!("no agent `{subject}`")))?;
            // Republishing a member this build cannot read would erase it for every peer.
            if desired.2.is_some() && desired.0.member.is_none() {
                return Err(St3Error::new(
                    "unreadable-agent-member",
                    format!("agent `{subject}` has a launch this build cannot read"),
                ));
            }
            let heads = intent_leaves_tx(&transaction, subject).map_err(internal)?;
            (desired.0, heads, desired.1)
        };
        desired.set_display_name(name)?;
        let normalized = json!({ "agent": subject, "display_name": name });
        let intent = NormalizedIntent {
            direct_message_registrations: BTreeSet::new(),
            schema: "st3.v1".into(),
            source_hash: canonical_hash(&normalized).map_err(internal)?,
            subjects: BTreeMap::from([(subject.to_owned(), desired)]),
            missions: BTreeMap::new(),
            mission_provenance: BTreeMap::new(),
            mission_runs: BTreeMap::new(),
            planning_sessions: BTreeMap::new(),
            resource_refreshes: Vec::new(),
            replica_repairs: Vec::new(),
            document_refs: BTreeSet::new(),
            deprecated_syntax: BTreeSet::new(),
            normalized,
        };
        self.apply_as(
            &intent,
            &BTreeMap::from([(subject.to_owned(), heads)]),
            idempotency_key,
            writer.as_deref(),
        )
    }

    /// The seat declaration that the stop selected for `subject` ended, or `None` when the
    /// selected declaration is not a stop. A stop that replaced another stop is followed back
    /// to the declaration they ended; a merge of several is ambiguous and also gives `None`.
    pub fn declaration_ended_by_stop(&self, subject: &str) -> Result<Option<EndedDeclaration>> {
        let connection = self.readers.get();
        let Some((stop, writer)) = connection
            .query_row(
                "SELECT desired.subject, desired.kind, desired.body, desired.member,
                        desired.owner_run, desired.owner_generation, desired.owner_step,
                        desired.claim_id, claims.actor
                 FROM desired LEFT JOIN claims ON claims.id = desired.claim_id
                 WHERE desired.subject = ?1 AND desired.kind = 'stop'",
                [subject],
                |row| {
                    Ok((
                        (desired_from_row(row)?, row.get::<_, String>(7)?),
                        row.get::<_, Option<String>>(8)?,
                    ))
                },
            )
            .optional()?
        else {
            return Ok(None);
        };
        let (stop, token) = stop;
        let mut claim = claim_by_id_tx(&connection, &token)?;
        // A seat is stopped and started a few times at most; the bound only guards a cycle.
        for _ in 0..64 {
            let Some(current) = claim else {
                return Ok(None);
            };
            let desired: DesiredSubject = serde_json::from_value(current.body)?;
            if desired.kind == "agent" {
                return Ok(Some(EndedDeclaration {
                    stop,
                    token,
                    writer,
                    declaration: desired,
                }));
            }
            let [predecessor] = current.predecessors.as_slice() else {
                return Ok(None);
            };
            if desired.kind != "stop" {
                return Ok(None);
            }
            claim = claim_by_id_tx(&connection, predecessor)?;
        }
        Ok(None)
    }

    /// Observers, subscriptions, and schedules whose owner run is terminal or whose owner
    /// generation is no longer current. They must not observe, deliver, or start work.
    pub fn retired_owned_intake_subjects(&self) -> Result<BTreeSet<String>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(RETIRED_OWNED_INTAKE_SUBJECTS)?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<BTreeSet<_>, _>>()
            .map_err(Into::into)
    }

    pub fn terminal_statuses(&self, include_history: bool) -> Result<Vec<SubjectStatus>> {
        let subjects = {
            let connection = self.readers.get();
            let mut statement = connection.prepare(
                "SELECT subject FROM desired
                 WHERE member IS NOT NULL
                   AND json_extract(member, '$.terminal')=1
                 ORDER BY subject",
            )?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut statuses = Vec::with_capacity(subjects.len());
        for subject in subjects {
            if let Some(status) = self.status(Some(&subject))?.subjects.into_iter().next()
                && (include_history || status.projection.actionable)
            {
                statuses.push(status);
            }
        }
        Ok(statuses)
    }

    pub fn retire_eval_owned_desired(&self, run: &str) -> Result<Vec<String>> {
        let run = if run.starts_with("mission-run/") {
            run.to_owned()
        } else {
            format!("mission-run/{run}")
        };
        let run_id = run.strip_prefix("mission-run/").unwrap_or(&run);
        let mut connection = self.connection.write();
        let transaction = connection.transaction()?;
        let mode: Option<String> = transaction
            .query_row(
                "SELECT mode FROM mission_runs WHERE id=?1",
                [run_id],
                |row| row.get(0),
            )
            .optional()?;
        anyhow::ensure!(
            mode.as_deref() == Some("eval"),
            "only eval runs can retire their desired graph"
        );
        transaction.execute(
            "DELETE FROM desired
             WHERE owner_run IN (
               SELECT 'mission-run/' || id FROM mission_runs WHERE root_run_id=?1
             )",
            [run_id],
        )?;
        let residue = {
            let mut statement = transaction.prepare(
                "SELECT desired.subject
                 FROM desired
                 JOIN mission_runs
                   ON desired.owner_run='mission-run/' || mission_runs.id
                 WHERE mission_runs.root_run_id=?1
                 ORDER BY desired.subject",
            )?;
            statement
                .query_map([run_id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        transaction.commit()?;
        Ok(residue)
    }

    /// Read replicated todo snapshots and their native-session bindings together at a graph
    /// snapshot. Each subject/kind lookup stops at the newest indexed claim.
    pub fn agent_todo_observations_for(
        &self,
        subjects: &[String],
        at_index: u64,
    ) -> Result<BTreeMap<String, BTreeMap<String, ClaimRecord>>> {
        if subjects.is_empty() {
            return Ok(BTreeMap::new());
        }
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(&format!(
            "SELECT {CLAIM_COLUMNS} FROM json_each(?1) subjects
             CROSS JOIN (
                 SELECT 'harness.todo.observed' AS kind
                 UNION ALL SELECT 'harness.session-file'
             ) kinds
             JOIN claims ON claims.id=(
                 SELECT claims.id FROM claims INDEXED BY claims_subject_kind_accepted_index
                 JOIN batches ON batches.id=claims.batch_id
                 WHERE claims.subject=subjects.value AND claims.kind=kinds.kind
                   AND +claims.store_index<=?2
                 ORDER BY {CANONICAL_ORDER_DESC} LIMIT 1
             )
             JOIN batches ON batches.id=claims.batch_id"
        ))?;
        let rows = statement.query_map(
            params![serde_json::to_string(subjects)?, at_index.min(i64::MAX as u64)],
            claim_from_row,
        )?;
        let mut observations = BTreeMap::<String, BTreeMap<String, ClaimRecord>>::new();
        for claim in rows {
            let claim = claim?;
            observations.entry(claim.subject.clone()).or_default()
                .insert(claim.kind.clone(), claim);
        }
        Ok(observations)
    }

    /// Suspension ancestry selected at a client snapshot; stopped declarations have none.
    pub fn suspension_lineage_at(&self, subject: &str, at: Option<u64>) -> Result<Vec<String>> {
        let connection = self.readers.get();
        let Some(row) = desired_row_at(&connection, subject, at)?.filter(|row| row.kind == "agent") else {
            return Ok(Vec::new());
        };
        desired_lineage_from_tx(&connection, row.claim_id, same_declared_launch)
    }

    /// Suspension follows all non-launch edits, independently of process retry budgets.
    pub fn suspension_lineage(&self, subject: &str) -> Result<Vec<String>> {
        let connection = self.readers.get();
        let Some(row) = current_desired_row(&connection, subject)? else {
            return Ok(Vec::new());
        };
        desired_lineage_from_tx(&connection, row.claim_id, same_declared_launch)
    }

    /// Suspension ancestry from `token`, over the claims accepted at or before `at`.
    pub fn suspension_lineage_from_at(&self, token: &str, at: Option<u64>) -> Result<Vec<String>> {
        desired_lineage_from_at_tx(&self.readers.get(), token.to_owned(), same_declared_launch, at)
    }

    /// The kind of the declaration selected at a client snapshot.
    pub fn selected_desired_kind_at(&self, subject: &str, at: Option<u64>) -> Result<Option<String>> {
        smallclaims::touched::note_read(|| subject.to_owned());
        let connection = self.readers.get();
        desired_row_at(&connection, subject, at).map(|row| row.map(|row| row.kind))
    }

    /// `latest_actual_value` at a client snapshot.
    pub fn latest_actual_value_at(&self, subject: &str, at: Option<u64>) -> Result<Option<Value>> {
        if at.is_none() {
            return self.latest_actual_value(subject);
        }
        smallclaims::touched::note_read(|| subject.to_owned());
        latest_actual_at(&self.readers.get(), subject, at)
    }

    /// Commit one restored move against the exact declaration its resume request named.
    /// The evidence keeps that suspension alive across this one placement change.
    pub fn place_resumed_seat(
        &self,
        subject: &str,
        expected: &str,
        operation: &str,
        host: &str,
        actor: &str,
    ) -> Result<ClaimRecord, St3Error> {
        self.connection.batched(|transaction| {
            let current = current_desired_row_tx(transaction, subject).map_err(internal)?
                .ok_or_else(|| St3Error::new("stale-fence", "seat declaration disappeared"))?;
            let current_claim = claim_by_id_tx(transaction, &current.claim_id).map_err(internal)?.unwrap();
            if current_claim.body["evidence"].as_array().is_some_and(|items| items.iter().any(|id| id.as_str() == Some(operation))) {
                return Ok(current_claim);
            }
            if current.claim_id != expected { return Err(St3Error::new("stale-fence", "seat declaration changed during transfer")); }
            let latest: String = transaction.query_row(
                &canonical_sql("SELECT id FROM claims WHERE subject=?1 AND kind='runtime.action.requested' AND json_extract(body,'$.fields.action') IN ('suspend','resume') ORDER BY CANONICAL_DESC(claims) LIMIT 1"),
                [subject], |row| row.get(0)).map_err(internal)?;
            if latest != operation { return Err(St3Error::new("stale-fence", "another suspension request replaced this move")); }
            let mut desired: DesiredSubject = serde_json::from_value(current_claim.body.clone()).map_err(internal)?;
            let member = desired.member.as_mut().ok_or_else(|| St3Error::new("stale-fence", "seat has no member"))?;
            member.host = host.into();
            let children = desired.desired["children"].as_array_mut().ok_or_else(|| St3Error::new("invalid-agent-declaration", "seat has no body"))?;
            if let Some(child) = children.iter_mut().find(|child| child["name"] == "host") {
                child["arguments"] = json!([host]);
            } else { children.push(json!({"name": "host", "arguments": [host]})); }
            let identity = subject.strip_prefix("agent/").unwrap_or(subject);
            if let Some(child) = children.iter_mut().find(|child| child["name"] == "identity") {
                child["arguments"] = json!([identity]);
            } else { children.push(json!({"name": "identity", "arguments": [identity]})); }
            let mut body = serde_json::to_value(&desired).map_err(internal)?;
            body["evidence"] = json!([operation]);
            let predecessors = intent_leaves_tx(transaction, subject).map_err(internal)?;
            let claim = append_claim_tx(transaction, &self.origin, subject, "intent.desired", Some(actor), &body, &predecessors, None).map_err(internal)?;
            select_replicated_desired(transaction, &claim, &desired)?;
            Ok(claim)
        }).map_err(|error| St3Error::new("internal", error))?
    }

    /// Record facts and deliveries atomically. Scheduler completions provide a predicate checked
    /// inside the writer transaction before work and before releasing its savepoint; direct
    /// consumers pass `None`. The predicate must release its scheduler lock before returning.
    #[allow(clippy::too_many_arguments)]
    pub fn record_resource_observation(
        &self,
        observer: &str,
        desired_revision: &str,
        attempt: Option<&str>,
        resource: &str,
        cursor: Option<&str>,
        facts: &Value,
        next_check_unix_ms: u128,
        subscriptions: &[(String, SubscriptionSpec)],
        current: Option<&(dyn Fn() -> bool + Sync)>,
    ) -> Result<ResourceObservationOutcome, St3Error> {
        let fence = resource_preparation::Fence::new(self);
        for pass in 0..2 {
            let prepared = fence.prepare_with_actuals(self, |connection, serial| {
                resource_preparation::inputs_with_actuals(
                    connection,
                    resource_preparation::Observation {
                        observer,
                        desired_revision,
                        attempt,
                        resource,
                        facts: &canonical_json_value(facts),
                        subscriptions,
                    },
                    &mut |subject| match serial {
                        Some(serial) => self.smalltalk.observer_actuals.read(connection, subject, serial),
                        None => latest_actual(connection, subject),
                    },
                )
            });
            #[cfg(any(test, feature = "test-support"))]
            resource_preparation::before_validate(self, pass);
            #[cfg(not(test))]
            let _ = pass;
            let outcome = self.record_resource_observation_attempt(
                observer,
                desired_revision,
                attempt,
                resource,
                cursor,
                facts,
                next_check_unix_ms,
                subscriptions,
                current,
                Some((&fence, prepared)),
            )?;
            if let Some(outcome) = outcome {
                return Ok(outcome);
            }
            #[cfg(any(test, feature = "test-support"))]
            resource_preparation::COUNTS.with(|counts| counts.borrow_mut().invalidated += 1);
        }
        // Exactly two unusable/stale preparations, then ONE invocation of the existing
        // canonical writer path. This preserves API progress, not a writer-hold cap.
        #[cfg(any(test, feature = "test-support"))]
        resource_preparation::COUNTS.with(|counts| counts.borrow_mut().fallback += 1);
        self.record_resource_observation_attempt(
            observer,
            desired_revision,
            attempt,
            resource,
            cursor,
            facts,
            next_check_unix_ms,
            subscriptions,
            current,
            None,
        )?
        .ok_or_else(|| St3Error::new("internal", "canonical observation returned no outcome"))
    }

    /// Resume an agent's kept usage fold with only the usage claims after it, when every one of
    /// them sorts after the last claim it folded. `None` when one does not, such as a replicated
    /// claim accepted earlier, or two new claims tie on accepted time; the caller folds afresh.
    pub fn usage_fold_after(
        connection: &rusqlite::Connection,
        subject: &str,
        cached: &CachedUsageFold,
        through: u64,
    ) -> Result<Option<CachedUsageFold>> {
        let mut statement = connection.prepare_cached(
            "SELECT store_index, body, accepted_at_unix_ms
             FROM claims INDEXED BY claims_subject_kind_index
             WHERE subject=?1 AND kind='harness.usage' AND store_index>?2 AND store_index<=?3",
        )?;
        let mut rows = statement
            .query_map(params![subject, cached.through, through], |row| {
                Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.sort_by(|a, b| accepted_order(&a.2).cmp(&accepted_order(&b.2)));
        let mut last = cached.last_accepted.as_str();
        for row in &rows {
            if accepted_order(&row.2) <= accepted_order(last) {
                return Ok(None);
            }
            last = &row.2;
        }
        let mut fold = cached.fold.clone();
        for (store_index, body, accepted) in &rows {
            fold.apply(*store_index, body, accepted, None)?;
        }
        Ok(Some(CachedUsageFold { through, last_accepted: last.to_owned(), fold }))
    }

    /// Period spend from replicated cumulative snapshots. The snapshot immediately before the
    /// start is the baseline, so a long-lived incarnation only contributes spend in the period.
    /// Each row is one agent incarnation's spend on one model and account for one mission run
    /// and step on one host, with its API-equivalent cost in millionths of a dollar. Tokens no
    /// price covers are counted in `unpriced_tokens`, never as free.
    pub fn usage_period_rows(&self, since_ms: u64, until_ms: u64) -> Result<Vec<Value>> {
        Ok(self.usage_period_data(since_ms, until_ms)?.0)
    }

    /// Daily denominators share the existing snapshot pass; message counts are indexed rollups.
    pub fn usage_period_report(&self, since_ms: u64, until_ms: u64) -> Result<(Vec<Value>, Value)> {
        let (rows, days) = self.usage_period_data(since_ms, until_ms)?;
        Ok((rows, self.agent_message_estimate(&days)?))
    }

    pub fn usage_period_data(
        &self,
        since_ms: u64,
        until_ms: u64,
    ) -> Result<(Vec<Value>, Vec<agent_messages::DailyUsage>)> {
        self.read_snapshot(|_| {
            let mut boundaries = BTreeSet::from([since_ms, until_ms]);
            for (since, until) in agent_messages::windows(since_ms, until_ms) {
                boundaries.extend([since, until]);
            }
            let rows = usage_period::boundary_rows(&self.readers.get(), &boundaries)?;
            Self::usage_period_data_from_rows(rows.into_iter().map(Ok), since_ms, until_ms)
        })
    }

    pub fn usage_period_data_from_rows(
        rows: impl IntoIterator<Item = rusqlite::Result<(String, u64, String)>>,
        since_ms: u64,
        until_ms: u64,
    ) -> Result<(Vec<Value>, Vec<agent_messages::DailyUsage>)> {
        #[derive(Clone, Default)]
        struct Snapshot {
            at: u64,
            buckets: [u64; USAGE_BUCKETS.len()],
            pricing: String,
            native_session_id: Option<String>,
            provenance: Option<Vec<Value>>,
        }
        #[derive(Clone, Copy, Default)]
        struct DailySnapshot {
            at: u64,
            buckets: [u64; USAGE_BUCKETS.len()],
        }
        type Key = (String, String, String, String, String, String, String);
        let mut days = agent_messages::windows(since_ms, until_ms)
            .into_iter()
            .map(|(since, until)| agent_messages::DailyUsage {
                since,
                until,
                cost: 0,
                unpriced: 0,
            })
            .collect::<Vec<_>>();
        #[derive(Default)]
        struct Group {
            baseline: Option<Snapshot>,
            latest: Option<Snapshot>,
            days: Vec<(Option<DailySnapshot>, Option<DailySnapshot>)>,
        }
        let mut groups = BTreeMap::<Key, Group>::new();
        for row in rows {
            let (subject, _index, body) = row?;
            let body: Value = serde_json::from_str(&body)?;
            let fields = &body["fields"];
            let at = fields["observed_at_unix_ms"].as_u64().unwrap_or(0);
            if at > until_ms {
                continue;
            }
            let mut snapshot = Snapshot {
                at,
                pricing: fields["pricing"].as_str().unwrap_or("").to_owned(),
                native_session_id: fields["native_session_id"].as_str().map(str::to_owned),
                provenance: fields["pricing_provenance"].as_array().cloned(),
                ..Snapshot::default()
            };
            for (bucket, name) in snapshot.buckets.iter_mut().zip(USAGE_BUCKETS) {
                *bucket = fields[name].as_u64().unwrap_or(0);
            }
            // A rollup from before costs were recorded priced nothing: its tokens are unpriced.
            if fields.get("cost_microusd").is_none() {
                snapshot.buckets[8] = snapshot.buckets[0];
            }
            let text = |name| fields[name].as_str().unwrap_or("").to_owned();
            let key = (
                subject,
                text("incarnation_id"),
                text("model"),
                text("account"),
                text("owner_run"),
                text("owner_step"),
                text("host"),
            );
            let group = groups.entry(key).or_insert_with(|| Group {
                days: vec![(None, None); days.len()],
                ..Group::default()
            });
            for (day, (baseline, latest)) in days.iter().zip(&mut group.days) {
                if at > day.until {
                    continue;
                }
                let target = if at <= day.since { baseline } else { latest };
                if target.is_none_or(|previous| at >= previous.at) {
                    *target = Some(DailySnapshot {
                        at,
                        buckets: snapshot.buckets,
                    });
                }
            }
            let target = if at <= since_ms {
                &mut group.baseline
            } else {
                &mut group.latest
            };
            if target.as_ref().is_none_or(|previous| at >= previous.at) {
                *target = Some(snapshot);
            }
        }
        let mut result = Vec::new();
        for ((agent, _incarnation, model, account, mission_run, step, host), group) in groups {
            for (day, (baseline, latest)) in days.iter_mut().zip(group.days) {
                let Some(latest) = latest else {
                    continue;
                };
                let baseline = baseline
                    .filter(|b| b.buckets[0] <= latest.buckets[0])
                    .unwrap_or_default();
                if latest.buckets[0] > baseline.buckets[0] {
                    day.cost = day
                        .cost
                        .saturating_add(latest.buckets[6].saturating_sub(baseline.buckets[6]));
                    day.unpriced = day
                        .unpriced
                        .saturating_add(latest.buckets[8].saturating_sub(baseline.buckets[8]));
                }
            }
            let Some(latest) = group.latest else {
                continue;
            };
            // A series that went backwards restarted from zero, for example when a node's local
            // totals were rebuilt; everything after the restart is spend in the period.
            let baseline = group
                .baseline
                .filter(|baseline| baseline.buckets[0] <= latest.buckets[0])
                .unwrap_or_default();
            let mut row = json!({
                "agent": agent, "mission_run": mission_run, "step": step,
                "model": model, "account": account, "host": host,
                "pricing": latest.pricing,
            });
            if let Some(session) = latest.native_session_id {
                row["native_session_id"] = json!(session);
            }
            if let Some(provenance) = latest.provenance {
                row["pricing_provenance"] = json!(pricing_period_delta(
                    provenance,
                    baseline.provenance.as_deref().unwrap_or(&[]),
                ));
            }
            for (index, name) in USAGE_BUCKETS.iter().enumerate() {
                row[*name] =
                    Value::from(latest.buckets[index].saturating_sub(baseline.buckets[index]));
            }
            if row["total_tokens"] == 0 {
                continue;
            }
            result.push(row);
        }
        result.sort_by(|a, b| b["total_tokens"].as_u64().cmp(&a["total_tokens"].as_u64()));
        Ok((result, days))
    }

    /// Resolve a terminal capability and its current-head fence with two indexed seeks, in
    /// one SQLite snapshot. Unrelated fleet history cannot hide an unexpired capability.
    pub fn terminal_attachment_for_capability_hash(
        &self,
        digest: &str,
    ) -> Result<Option<(ClaimRecord, bool)>> {
        let connection = self.readers.get();
        Ok(connection
            .prepare_cached(
                "SELECT id, store_index, batch_id, subject, kind, origin, actor, body,
                        predecessors, accepted_at_unix_ms,
                        NOT EXISTS (
                            SELECT 1 FROM claims AS newer
                            WHERE newer.subject=claims.subject
                              AND newer.store_index>claims.store_index
                        )
                 FROM claims
                 WHERE kind='custom.client.terminal-attached'
                   AND json_extract(body, '$.fields.capability_hash')=?1
                 ORDER BY store_index DESC LIMIT 1",
            )?
            .query_row([digest], |row| Ok((claim_from_row(row)?, row.get(10)?)))
            .optional()?)
    }

    /// Last substantive activity for an agent, excluding heartbeat and usage observations.
    pub fn agent_last_activity_at(
        &self,
        agent: &str,
        incarnation: Option<&str>,
        snapshot_index: u64,
    ) -> Result<Option<u128>> {
        let connection = self.readers.get();
        let mut times = Vec::new();
        if let Some(incarnation) = incarnation {
            let local: Option<i64> = connection.prepare_cached(
                "SELECT observed_at_unix_ms FROM local_observations
                 WHERE subject=?1 AND kind='harness.timeline'
                   AND json_extract(body, '$.fields.incarnation_id')=?2
                   AND json_extract(body, '$.fields.entry_type') IN ('message','content','tool_call','tool_result')
                   AND after_store_index<=?3 ORDER BY id DESC LIMIT 1")?.query_row(
                params![agent, incarnation, snapshot_index], |row| row.get(0),
            ).optional()?;
            if let Some(time) = local {
                times.push(time.max(0) as u128);
            }
            let replicated: Option<String> = connection.prepare_cached(
                "SELECT accepted_at_unix_ms FROM claims INDEXED BY claims_timeline_activity_time_index
                 WHERE subject=?1 AND kind='harness.timeline'
                   AND json_extract(body, '$.fields.incarnation_id')=?2
                   AND json_extract(body, '$.fields.entry_type') IN ('message','content','tool_call','tool_result')
                   AND store_index<=?3
                 ORDER BY length(accepted_at_unix_ms) DESC, accepted_at_unix_ms DESC LIMIT 1")?.query_row(
                params![agent, incarnation, snapshot_index], |row| row.get(0),
            ).optional()?;
            times.extend(replicated.and_then(|time| time.parse::<u128>().ok()));
        }
        // Only the timestamp is returned, so equal acceptance times need no identity tie
        // break. Length then decimal text preserves the full unsigned numeric range.
        for query in [
            "SELECT accepted_at_unix_ms FROM claims INDEXED BY claims_actor_activity_time_index WHERE actor=?1 AND kind IN ('work.progress','work.submitted') AND store_index<=?2 ORDER BY length(accepted_at_unix_ms) DESC, accepted_at_unix_ms DESC LIMIT 1",
            "SELECT accepted_at_unix_ms FROM claims INDEXED BY claims_message_from_activity_time_index WHERE kind='message.sent' AND json_extract(body, '$.fields.from')=?1 AND store_index<=?2 ORDER BY length(accepted_at_unix_ms) DESC, accepted_at_unix_ms DESC LIMIT 1",
            "SELECT accepted_at_unix_ms FROM claims INDEXED BY claims_message_to_activity_time_index WHERE kind='message.sent' AND json_extract(body, '$.fields.to')=?1 AND store_index<=?2 ORDER BY length(accepted_at_unix_ms) DESC, accepted_at_unix_ms DESC LIMIT 1",
        ] {
            let time: Option<String> = connection
                .prepare_cached(query)?
                .query_row(params![agent, snapshot_index], |row| row.get(0))
                .optional()?;
            times.extend(time.and_then(|time| time.parse::<u128>().ok()));
        }
        Ok(times.into_iter().max())
    }

    pub fn cached_placement_fence(
        &self,
        token: &str,
        build: impl FnOnce() -> Result<Option<crate::model::PlacementFence>>,
    ) -> Result<Option<Arc<crate::model::PlacementFence>>> {
        if let Some(fence) = self.smalltalk.placement_cache.lock()
            .unwrap_or_else(PoisonError::into_inner).get(token) {
            return Ok(fence.clone());
        }
        let fence = build()?.map(Arc::new);
        let mut cache = self.smalltalk.placement_cache.lock()
            .unwrap_or_else(PoisonError::into_inner);
        if cache.len() >= ACTUAL_CACHE_LIMIT { cache.clear(); }
        cache.insert(token.to_owned(), fence.clone());
        Ok(fence)
    }

    /// First observation of the current boot, including its index to distinguish messages
    /// accepted within the same millisecond. Late replicas must also pass the time boundary.
    pub fn native_mail_boot_floor(&self, subject: &str) -> Result<Option<(u128, u64)>> {
        let runtime = self.latest_claim(subject, Some("runtime.observed"))?;
        let Some(incarnation) = runtime.as_ref().and_then(|claim| {
            let fields = claim.body.get("fields").unwrap_or(&claim.body);
            fields["incarnation_id"].as_str()
        }) else {
            return Ok(None);
        };
        self.readers
            .get()
            .prepare_cached(&canonical_sql(&format!(
                "SELECT accepted_at_unix_ms, store_index
             FROM claims INDEXED BY claims_incarnation_accepted_index
             WHERE subject=?1 AND {INCARNATION_OF_CLAIM}=?2
             ORDER BY CANONICAL_ASC(claims)
             LIMIT 1"
            )))?
            .query_row(params![subject, incarnation], |row| {
                let time: String = row.get(0)?;
                Ok((time.parse::<u128>().unwrap_or(u128::MAX), row.get(1)?))
            })
            .optional()
            .map_err(Into::into)
    }

    /// The login directory a seat's harness runs with on `host`: its account's, or the pool
    /// account this node placed it on, else the harness's own default login on that host. Only
    /// the seat's own node knows which pool account it chose, so a pooled seat on another node
    /// stands alone rather than being merged with seats that may use another login.
    pub fn seat_login(&self, desired: &DesiredSubject, host: &str, driver: &str) -> Result<String> {
        let account = match crate::accounts::harness_binding(&desired.desired).map(|b| b.binding) {
            Some(crate::accounts::Binding::Account(name)) => name,
            Some(crate::accounts::Binding::Pool(_)) if host == self.origin => {
                match self.seat_account_choice(&desired.subject)? {
                    Some(name) => name,
                    None => return Ok(format!("the login of {}", desired.subject)),
                }
            }
            Some(crate::accounts::Binding::Pool(_)) => {
                return Ok(format!("the login of {}", desired.subject));
            }
            None => return Ok(format!("the default {driver} login")),
        };
        // One declaration read by its key, not a scan of every declared subject.
        Ok(self
            .desired_subject_with_writer(&format!("account/{account}"))?
            .and_then(|(declared, _)| {
                crate::accounts::parse_account(&declared.subject, &declared.desired)
            })
            .and_then(|declared| declared.login_for(host).map(str::to_owned))
            .unwrap_or(account))
    }

    pub fn harness_login_episode_key(
        &self,
        subject: &str,
        incarnation: &str,
    ) -> Result<(Option<ClaimRecord>, String)> {
        let base = format!("claude-auth-expired:{subject}:{incarnation}");
        let Some(latest) = self.harness_auth_episode(subject, incarnation, None)? else {
            return Ok((None, base));
        };
        if latest.body["fields"]["code"] == "provider-auth-restored" {
            return Ok((None, format!("{base}:{}", latest.id)));
        }
        let key = match latest.body["fields"]["auth_attention_key"].as_str() {
            Some(key) => key.to_owned(),
            None => self
                .harness_auth_episode(subject, incarnation, Some("provider-auth-restored"))?
                .map_or(base.clone(), |restored| format!("{base}:{}", restored.id)),
        };
        Ok((Some(latest), key))
    }

    pub fn harness_auth_episode(
        &self,
        subject: &str,
        incarnation: &str,
        code: Option<&str>,
    ) -> Result<Option<ClaimRecord>> {
        smallclaims::touched::note_read(|| subject.to_owned());
        let connection = self.readers.get();
        let id: Option<String> = connection.query_row(&canonical_sql(
            "SELECT id FROM claims INDEXED BY claims_harness_auth_diagnostic_index
             WHERE subject=?1 AND kind='harness.diagnostic'
               AND json_extract(body, '$.fields.incarnation_id')=?2
               AND json_extract(body, '$.fields.code') IN ('provider-auth-expired','provider-auth-restored')
               AND (?3 IS NULL OR json_extract(body, '$.fields.code')=?3)
             ORDER BY CANONICAL_DESC(claims) LIMIT 1"), params![subject, incarnation, code], |row| row.get(0)).optional()?;
        id.map(|id| self.claim_by_id(&id))
            .transpose()
            .map(Option::flatten)
    }

    /// Prefer a bound account person, then follow declaration authors and mission requesters.
    /// Missing or cyclic ownership remains unknown; it never selects a global operator.
    pub fn agent_person(&self, agent: &str) -> Result<Option<String>> {
        let mut actor = agent.to_owned();
        let mut seen = BTreeSet::new();
        for _ in 0..16 {
            if actor.starts_with("person/") {
                return Ok(Some(actor));
            }
            if !seen.insert(actor.clone()) {
                return Ok(None);
            }
            let Some((desired, writer)) = self.desired_subject_with_writer(&actor)? else {
                return Ok(None);
            };
            if actor == agent {
                let owner = match crate::accounts::harness_binding(&desired.desired)
                    .map(|binding| binding.binding)
                {
                    Some(crate::accounts::Binding::Pool(person)) => Some(person),
                    Some(crate::accounts::Binding::Account(account)) => self
                        .desired_subject_with_writer(&format!("account/{account}"))?
                        .and_then(|(account, _)| {
                            crate::accounts::parse_account(&account.subject, &account.desired)
                        })
                        .and_then(|account| account.owner),
                    None => None,
                };
                if let Some(owner) = owner.filter(|owner| owner.starts_with("person/")) {
                    return Ok(Some(owner));
                }
            }
            let requester = desired
                .owner_run
                .as_deref()
                .map(|run| self.mission_run(run))
                .transpose()?
                .flatten()
                .map(|run| run.requester);
            let Some(next) = requester.or(writer) else {
                return Ok(None);
            };
            actor = next;
        }
        Ok(None)
    }

    pub fn current_harness(
        &self,
        subject: &str,
    ) -> Result<Option<crate::model::CurrentHarnessView>> {
        // Its work claims and progress live on step subjects, written with this agent as actor.
        smallclaims::touched::note_read(|| subject.to_owned());
        smallclaims::touched::note_read(|| format!("actor:{subject}"));
        let connection = self.readers.get();
        current_harness_at(&connection, subject, None)
    }

    /// Incarnation-bound terminal evidence at the same snapshot as the client projection.
    pub fn terminal_harness_at(
        &self,
        subject: &str,
        at_index: u64,
    ) -> Result<Option<crate::model::CurrentHarnessView>> {
        // The ended harness is read from the seat's own claims, so the evaluation that reads
        // it must be revisited when one arrives.
        smallclaims::touched::note_read(|| subject.to_owned());
        let connection = self.readers.get();
        Ok(harness_fold_at(&connection, subject, Some(at_index), true, false, true)?
            .filter(|harness| harness.state == "ended"))
    }

    /// The seat's harness for the one question the login attention item asks: does it need a
    /// login? A seat that does not returns without the observation fold, which is what a
    /// full `current_harness` costs, so an attention read no longer folds every seat that ever
    /// had a login problem. The view a seat that does need one gets is `current_harness`'s,
    /// except that its `since` is not refined (attention reads the state, incarnation and
    /// observation time).
    pub fn current_harness_for_login(
        &self,
        subject: &str,
    ) -> Result<Option<crate::model::CurrentHarnessView>> {
        smallclaims::touched::note_read(|| subject.to_owned());
        smallclaims::touched::note_read(|| format!("actor:{subject}"));
        let connection = self.readers.get();
        current_harness_fold_at(&connection, subject, None, true, true)
    }

    pub fn harness_was_ready(&self, subject: &str, incarnation: &str) -> Result<bool> {
        let connection = self.readers.get();
        let found: i64 = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM claims
             WHERE subject=?1 AND kind='harness.observed'
               AND json_extract(body, '$.fields.incarnation_id')=?2
               AND json_extract(body, '$.fields.state') IN ('ready','working','idle')
               AND coalesce(json_extract(body, '$.fields.reason'),'')!='providerAuth')",
            params![subject, incarnation],
            |row| row.get(0),
        )?;
        Ok(found != 0)
    }
}

/// Canonical order's first components: accepted time as stored, shorter first.
fn accepted_order(accepted: &str) -> (usize, &str) {
    (accepted.len(), accepted)
}

/// How long a roster refresh request may go unanswered before the daemon warns about it.
const AGENT_ROSTER_OVERDUE_MS: u64 = 30_000;

/// A selected stop and the seat declaration it ended.
#[derive(Clone, Debug)]
pub struct EndedDeclaration {
    /// The selected stop.
    pub stop: DesiredSubject,
    /// The stop's claim, the seat's selected desired token.
    pub token: String,
    /// Who published the stop; the daemon's own stops record no writer.
    pub writer: Option<String>,
    /// The agent declaration the stop ended.
    pub declaration: DesiredSubject,
}

fn record_roster_counts(changed: usize, covered: usize, refolded: usize) {
    crate::otel::roster_attribute("st.roster.changed", changed as i64);
    crate::otel::roster_attribute("st.roster.covered", covered as i64);
    crate::otel::roster_attribute("st.roster.refolded", refolded as i64);
    crate::otel::roster_attribute("st.roster.cards", covered as i64);
}

/// The run that one run still waits for: its `after` run, until its `after-run` step completes.
fn awaited_mission_run_tx(connection: &Connection, run: &str) -> Result<Option<String>> {
    let run_id = run.strip_prefix("mission-run/").unwrap_or(run);
    Ok(connection
        .query_row(
            "SELECT mission_run_after.after_run
             FROM mission_run_after
             JOIN mission_runs ON mission_runs.id=mission_run_after.run_id
             JOIN step_runs ON step_runs.generation_id=mission_runs.current_generation_id
               AND step_runs.step_path=?2
             WHERE mission_run_after.run_id=?1 AND step_runs.status<>'completed'",
            params![run_id, crate::mission::AFTER_RUN_STEP],
            |row| row.get(0),
        )
        .optional()?)
}

fn pricing_period_delta(latest: Vec<Value>, baseline: &[Value]) -> Vec<Value> {
    latest
        .into_iter()
        .filter_map(|mut entry| {
            if let Some(before) = baseline
                .iter()
                .find(|before| pricing_identity(before) == pricing_identity(&entry))
            {
                for name in USAGE_BUCKETS {
                    entry[name] = json!(
                        entry[name]
                            .as_u64()
                            .unwrap_or(0)
                            .saturating_sub(before[name].as_u64().unwrap_or(0))
                    );
                }
            }
            // A zero-token response may still have a reported cost.
            (entry["total_tokens"] != 0 || entry["cost_microusd"] != 0).then_some(entry)
        })
        .collect()
}

const SEAT_QUEUE_MOVE_HISTORY_LIMIT: usize = 20;
