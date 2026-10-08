//! Agent harness rows maintained by canonical, affected-key edits.
//!
//! The ordered operator handles sparse legacy fields and status episodes without replaying
//! a seat's history. This module does not certify the surrounding declaration, work queue,
//! placement or person joins: the production registry must cover those dependencies before
//! using this source as a complete public card.
use super::*;
use serde::Deserialize;
use smallclaims::ivm::{Contribution, Definition, View};

mod ordered;

pub const VIEW: &str = "st3.agents.harness.v1";
const KINDS: &[&str] = &["runtime.observed", "harness.observed", "harness.diagnostic"];
const OPTIONAL: &[&str] = &[
    "driver",
    "transport",
    "reason",
    "blocked_on",
    "ask",
    "input_buffer",
    "exit",
];
const STATES: &[&str] = &[
    "",
    "starting",
    "ready",
    "idle",
    "working",
    "blocked",
    "ended",
    "indeterminate",
    "needs-login",
];
const MACHINE_STATES: usize = 1 + STATES.len() * 3 * 2 * 2 * 2 * 2;

pub struct AgentView;
pub fn definitions() -> Vec<Box<dyn View>> {
    vec![Box::new(AgentView)]
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
struct Event {
    claim: String,
    subject: String,
    kind: String,
    rank: Vec<u8>,
    accepted: u128,
    fields: Value,
    nested: bool,
}
impl Event {
    fn observed(&self) -> u128 {
        self.fields
            .get("observed_at_ms")
            .and_then(Value::as_u64)
            .map_or(self.accepted, u128::from)
            .min(self.accepted)
    }
}
#[derive(Clone, Serialize, Deserialize, Default)]
struct Field {
    rank: Vec<u8>,
    value: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Run {
    first: Option<String>,
    last: Option<String>,
    uniform: bool,
    since: u128,
}
#[derive(Clone, Serialize, Deserialize, Default)]
struct Summary {
    state: Option<Event>,
    fields: BTreeMap<String, Field>,
    auth: Option<Event>,
    diagnostics: BTreeMap<String, Event>,
    raw_run: Option<Run>,
    #[serde(skip)]
    machine: Machine,
}
fn newer<'a>(left: Option<&'a Event>, right: Option<&'a Event>) -> Option<&'a Event> {
    match (left, right) {
        (Some(l), Some(r)) => Some(if l.rank > r.rank { l } else { r }),
        (l, None) => l,
        (None, r) => r,
    }
}
fn diagnostic(code: &str) -> Option<&'static str> {
    match code {
        "harness-admission-failed" => Some("admission"),
        "provider-update-prompt" | "provider-update-restored" => Some("update"),
        "provider-auth-expired" | "provider-auth-restored" | "provider-trust-prompt" => {
            Some("prompt")
        }
        "claude-channel-unattached" | "claude-channel-attached" => Some("attachment"),
        _ => None,
    }
}
impl Summary {
    fn event(event: &Event, id: u64) -> Result<Self> {
        let mut out = Self::default();
        if event.kind == "harness.observed" {
            let state = event.fields.get("state").and_then(Value::as_str);
            if let Some(state) = state {
                anyhow::ensure!(
                    STATES[1..].contains(&state),
                    "unsupported legacy harness state {state:?}"
                );
            }
            if state.is_some() {
                out.state = Some(event.clone());
            }
            for &name in OPTIONAL {
                if matches!(state, Some("ended" | "indeterminate"))
                    && matches!(name, "blocked_on" | "ask")
                {
                    out.fields.insert(
                        name.into(),
                        Field {
                            rank: event.rank.clone(),
                            value: None,
                        },
                    );
                } else if let Some(value) = event.fields.get(name) {
                    out.fields.insert(
                        name.into(),
                        Field {
                            rank: event.rank.clone(),
                            value: value.as_str().map(str::to_owned),
                        },
                    );
                }
            }
            if event.nested
                && event
                    .fields
                    .get("provider_auth")
                    .and_then(Value::as_bool)
                    .is_some()
            {
                out.auth = Some(event.clone());
            }
            let state = state.map(str::to_owned);
            out.raw_run = Some(Run {
                first: state.clone(),
                last: state,
                uniform: true,
                since: event.observed(),
            });
        } else if event.kind == "harness.diagnostic"
            && event.nested
            && let Some(category) = event
                .fields
                .get("code")
                .and_then(Value::as_str)
                .and_then(diagnostic)
        {
            out.diagnostics.insert(category.into(), event.clone());
        }
        out.machine = Machine::event(event, id)?;
        Ok(out)
    }
    fn join(&self, right: &Self) -> Self {
        let mut fields = self.fields.clone();
        for (name, value) in &right.fields {
            if fields.get(name).is_none_or(|old| old.rank < value.rank) {
                fields.insert(name.clone(), value.clone());
            }
        }
        let mut diagnostics = self.diagnostics.clone();
        for (category, event) in &right.diagnostics {
            if diagnostics
                .get(category)
                .is_none_or(|old| old.rank < event.rank)
            {
                diagnostics.insert(category.clone(), event.clone());
            }
        }
        let raw_run = match (&self.raw_run, &right.raw_run) {
            (Some(l), Some(r)) => Some(Run {
                first: l.first.clone(),
                last: r.last.clone(),
                uniform: l.uniform && r.uniform && l.last == r.first,
                since: if r.uniform && l.last == r.first {
                    l.since
                } else {
                    r.since
                },
            }),
            (l, None) => l.clone(),
            (None, r) => r.clone(),
        };
        Self {
            state: newer(self.state.as_ref(), right.state.as_ref()).cloned(),
            fields,
            auth: newer(self.auth.as_ref(), right.auth.as_ref()).cloned(),
            diagnostics,
            raw_run,
            machine: self.machine.join(&right.machine),
        }
    }
}

// A finite transducer for exactly seat_status::transitions. Each entry records the
// final reducer state and last transition source for one possible incoming state.
// Composition is associative; heartbeats explicitly excluded by the canonical oracle
// are identities. Source pointers are local row IDs, never canonical winner ranks.
#[derive(Clone)]
struct Machine(Vec<(u16, u64)>);
impl Default for Machine {
    fn default() -> Self {
        Self((0..MACHINE_STATES).map(|s| (s as u16, 0)).collect())
    }
}
#[derive(Clone, Copy)]
struct Status {
    harness: usize,
    prompt: usize,
    auth: bool,
    update: bool,
    human: bool,
    permission: bool,
}
impl Status {
    fn decode(mut index: usize) -> Self {
        index -= 1;
        let permission = index & 1 != 0;
        index /= 2;
        let human = index & 1 != 0;
        index /= 2;
        let update = index & 1 != 0;
        index /= 2;
        let auth = index & 1 != 0;
        index /= 2;
        let prompt = index % 3;
        index /= 3;
        Self {
            harness: index,
            prompt,
            auth,
            update,
            human,
            permission,
        }
    }
    fn encode(self) -> u16 {
        (1 + (((((self.harness * 3 + self.prompt) * 2 + usize::from(self.auth)) * 2
            + usize::from(self.update))
            * 2
            + usize::from(self.human))
            * 2
            + usize::from(self.permission))) as u16
    }
    fn state(self) -> usize {
        if self.update {
            5
        } else if self.auth {
            9
        } else {
            match self.prompt {
                1 => 5,
                2 => 9,
                _ => self.harness,
            }
        }
    }
}
impl Machine {
    fn event(event: &Event, id: u64) -> Result<Self> {
        let observation = event.kind == "harness.observed";
        let fields = &event.fields;
        if observation
            && event.nested
            && fields.get("status_transition").and_then(Value::as_bool) == Some(false)
        {
            return Ok(Self::default());
        }
        if event.kind == "runtime.observed" {
            if fields["status"] != "running" {
                return Ok(Self::default());
            }
            let mut out = Self::default();
            out.0[0] = (1, id);
            return Ok(out);
        }
        let raw = if observation {
            match fields.get("state").and_then(Value::as_str) {
                Some(state) => Some(
                    STATES
                        .iter()
                        .position(|s| *s == state)
                        .context("unsupported legacy harness state")?,
                ),
                None => return Ok(Self::default()),
            }
        } else {
            None
        };
        let code = fields
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !observation
            && !matches!(
                code,
                "provider-auth-expired"
                    | "provider-auth-restored"
                    | "provider-trust-prompt"
                    | "provider-update-prompt"
                    | "provider-update-restored"
            )
        {
            return Ok(Self::default());
        }
        let mut out = Self::default();
        for (incoming, entry) in out.0.iter_mut().enumerate() {
            if incoming == 0 && !observation {
                continue;
            }
            let mut status = Status::decode(incoming.max(1));
            let previous = status.state();
            if let Some(raw) = raw {
                if matches!(raw, 6 | 7) {
                    status.human = false;
                    status.permission = false;
                } else {
                    if let Some(value) = fields.get("blocked_on") {
                        status.human = value.as_str() == Some("human");
                    }
                    if let Some(value) = fields.get("ask") {
                        status.permission = value.as_str() == Some("permission");
                    }
                }
                status.harness = if !matches!(raw, 6 | 7) && status.human && status.permission {
                    5
                } else {
                    raw
                };
                if let Some(auth) = fields.get("provider_auth").and_then(Value::as_bool) {
                    status.auth = !auth;
                }
            } else {
                match code {
                    "provider-auth-expired" => status.prompt = 2,
                    "provider-trust-prompt" => status.prompt = 1,
                    "provider-auth-restored" => status.prompt = 0,
                    "provider-update-prompt" => status.update = true,
                    "provider-update-restored" => status.update = false,
                    _ => {}
                }
            }
            let recorded = observation
                && status.prompt == 0
                && !status.auth
                && !status.update
                && fields.get("status_transition").and_then(Value::as_bool) == Some(true);
            *entry = (
                status.encode(),
                if !(observation
                    && fields.get("status_transition").and_then(Value::as_bool) == Some(false))
                    && (incoming == 0 || previous != status.state() || recorded)
                {
                    id
                } else {
                    0
                },
            );
        }
        Ok(out)
    }
    fn join(&self, right: &Self) -> Self {
        Self(
            self.0
                .iter()
                .map(|(state, transition)| {
                    let (next, new_transition) = right.0[usize::from(*state)];
                    (
                        next,
                        if new_transition != 0 {
                            new_transition
                        } else {
                            *transition
                        },
                    )
                })
                .collect(),
        )
    }
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(MACHINE_STATES * 10);
        for (state, source) in &self.0 {
            bytes.extend(state.to_le_bytes());
            bytes.extend(source.to_le_bytes());
        }
        bytes
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        anyhow::ensure!(
            bytes.len() == MACHINE_STATES * 10,
            "agent ordered machine version mismatch"
        );
        let mut entries = Vec::with_capacity(MACHINE_STATES);
        for entry in bytes.chunks_exact(10) {
            let state = u16::from_le_bytes(entry[..2].try_into()?);
            anyhow::ensure!(
                usize::from(state) < MACHINE_STATES,
                "invalid agent reducer state"
            );
            entries.push((state, u64::from_le_bytes(entry[2..].try_into()?)));
        }
        Ok(Self(entries))
    }
}

impl View for AgentView {
    fn definition(&self) -> Definition {
        Definition {
            name: VIEW,
            fingerprint: "agent-ordered-harness.v1;canonical.v1;sparse-seven;episodes433;retain-repaired;strict-state-reason",
            kinds: KINDS,
            local_kinds: &[],
            max_contributions: 1,
        }
    }
    fn create_schema(&self, connection: &Connection) -> Result<()> {
        connection.execute_batch(ordered::SCHEMA)?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS local_agent_harness_rows(subject TEXT PRIMARY KEY, observed TEXT);")?;
        Ok(())
    }
    fn affected_keys(
        &self,
        _transaction: &Transaction<'_>,
        old: Option<&ClaimRecord>,
        new: Option<&ClaimRecord>,
    ) -> Result<BTreeSet<String>> {
        Ok(old
            .into_iter()
            .chain(new)
            .filter(|c| c.subject.starts_with("agent/"))
            .map(|c| c.subject.clone())
            .collect())
    }
    fn contributions(
        &self,
        claim: &ClaimRecord,
        key: &canonical::ClaimKey,
    ) -> Result<Vec<Contribution>> {
        if !claim.subject.starts_with("agent/") {
            return Ok(Vec::new());
        }
        let rank = canonical::sortable_key(key);
        let event = Event {
            claim: claim.id.clone(),
            subject: claim.subject.clone(),
            kind: claim.kind.clone(),
            rank: rank.clone(),
            accepted: claim.accepted_at_unix_ms,
            fields: claim.body.get("fields").unwrap_or(&claim.body).clone(),
            nested: claim.body.get("fields").is_some(),
        };
        Ok(vec![Contribution {
            key: claim.subject.clone(),
            register: if claim.kind == "runtime.observed" {
                "runtime"
            } else {
                "source"
            }
            .into(),
            value: serde_json::to_value(event)?,
            rank,
        }])
    }
    fn maintain_key(
        &self,
        transaction: &Transaction<'_>,
        key: &str,
        old: Option<&ClaimRecord>,
        new: Option<&ClaimRecord>,
    ) -> Result<Option<bool>> {
        if let Some(old) = old {
            ordered::retract(transaction, &old.id)?;
        }
        if let Some(new) = new {
            let event: Option<String> = transaction
                .query_row(
                    "SELECT value FROM ivm_contributions WHERE view=?1 AND key=?2 AND claim_id=?3",
                    params![VIEW, key, new.id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(event) = event {
                ordered::admit(transaction, &serde_json::from_str(&event)?)?;
            }
        }
        let observed = observed(transaction, key)?;
        let next = serde_json::to_string(&observed)?;
        let old: Option<String> = transaction
            .query_row(
                "SELECT observed FROM local_agent_harness_rows WHERE subject=?1",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        let changed = old.as_deref() != Some(&next);
        if changed {
            transaction.execute("INSERT INTO local_agent_harness_rows VALUES(?1,?2) ON CONFLICT(subject) DO UPDATE SET observed=excluded.observed",params![key,next])?;
        }
        Ok(Some(changed))
    }
}

fn observed(
    connection: &Connection,
    subject: &str,
) -> Result<Option<crate::model::CurrentHarnessView>> {
    let runtime: Option<String> = connection
        .query_row(
            "SELECT value FROM ivm_heads WHERE view=?1 AND key=?2 AND register='runtime'",
            params![VIEW, subject],
            |r| r.get(0),
        )
        .optional()?;
    let Some(runtime) = runtime else {
        return Ok(None);
    };
    let runtime: Event = serde_json::from_str(&runtime)?;
    let Some(incarnation) = runtime.fields.get("incarnation_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    let named = ordered::all(connection, subject, Some(incarnation))?;
    let diag = |category: &str| named.diagnostics.get(category);
    let text = |event: &Event, name: &str| {
        event
            .fields
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let fence = |event: &Event,
                 state: &str,
                 driver: Option<String>,
                 transport: Option<String>,
                 reason: Option<String>,
                 blocked_on: Option<&str>| crate::model::CurrentHarnessView {
        turn_recovery: None,
        state: state.into(),
        driver,
        incarnation_id: incarnation.into(),
        transport,
        reason,
        blocked_on: blocked_on.map(str::to_owned),
        ask: None,
        input_buffer: None,
        exit: None,
        claim: event.claim.clone(),
        since_unix_ms: event.accepted,
        observed_at_unix_ms: event.accepted,
    };
    let mut view = if let Some(event) = diag("admission") {
        Some(fence(
            event,
            "indeterminate",
            None,
            None,
            Some(text(event, "reason").context("non-text harness admission reason")?),
            None,
        ))
    } else if runtime.fields["status"] != "running" {
        None
    } else if let Some(event) =
        diag("update").filter(|e| e.fields["code"] == "provider-update-prompt")
    {
        Some(fence(
            event,
            "blocked",
            text(event, "driver"),
            Some("native".into()),
            text(event, "reason"),
            Some("human"),
        ))
    } else if let Some(event) = named
        .auth
        .as_ref()
        .filter(|e| e.fields["provider_auth"] == false)
    {
        Some(fence(
            event,
            "needs-login",
            text(event, "driver"),
            text(event, "transport"),
            Some("providerAuth".into()),
            Some("human"),
        ))
    } else if let Some(event) =
        diag("prompt").filter(|e| e.fields["code"] != "provider-auth-restored")
    {
        let trust = event.fields["code"] == "provider-trust-prompt";
        let driver = text(event, "driver").unwrap_or_else(|| "claude".into());
        let transport = if driver == "claude" {
            "claude-channel"
        } else {
            "native"
        };
        Some(fence(
            event,
            if trust { "blocked" } else { "needs-login" },
            Some(driver),
            Some(transport.into()),
            Some(
                if trust {
                    "providerTrustPrompt"
                } else {
                    "providerAuth"
                }
                .into(),
            ),
            Some("human"),
        ))
    } else if let Some(event) =
        diag("attachment").filter(|e| e.fields["code"] == "claude-channel-unattached")
    {
        Some(fence(
            event,
            if event.fields["status"] == "starting" {
                "starting"
            } else {
                "blocked"
            },
            Some("claude".into()),
            Some("claude-channel".into()),
            Some("claude-channel-unattached".into()),
            Some("channel"),
        ))
    } else {
        let unnamed = ordered::unnamed_after(connection, subject, &runtime.rank)?;
        let merged = named.join(&unnamed);
        let Some(event) = merged.state.as_ref() else {
            return Ok(None);
        };
        let field = |name: &str| merged.fields.get(name).and_then(|f| f.value.clone());
        let mut state = text(event, "state").context("harness state missing")?;
        let mut reason = field("reason");
        let terminal = matches!(state.as_str(), "ended" | "indeterminate");
        if seat_status::permission_blocked(
            Some(&state),
            field("blocked_on").as_deref(),
            field("ask").as_deref(),
        ) {
            state = "blocked".into();
        }
        if reason.as_deref() == Some("providerAuth") {
            if named
                .auth
                .as_ref()
                .is_some_and(|e| e.fields["provider_auth"] == true)
            {
                reason = None;
            } else {
                state = "needs-login".into();
            }
        }
        Some(crate::model::CurrentHarnessView {
            turn_recovery: None,
            state,
            driver: field("driver"),
            incarnation_id: incarnation.into(),
            transport: field("transport"),
            reason,
            blocked_on: if terminal { None } else { field("blocked_on") },
            ask: if terminal { None } else { field("ask") },
            input_buffer: field("input_buffer"),
            exit: field("exit"),
            claim: event.claim.clone(),
            since_unix_ms: event.accepted,
            observed_at_unix_ms: event.accepted,
        })
    };
    if let Some(view) = view.as_mut() {
        // Since uses the selected claim's stamp first, then the named raw-state run;
        // diagnostics/auth can replace that with the final canonical transition.
        let event =
            claim_by_id_tx(connection, &view.claim)?.context("agent source claim unavailable")?;
        if let Some(since) = event
            .body
            .pointer("/fields/observed_since_ms")
            .and_then(Value::as_u64)
        {
            view.since_unix_ms = u128::from(since).min(event.accepted_at_unix_ms);
        } else if event.kind == "harness.observed"
            && let Some(run) = named
                .raw_run
                .as_ref()
                .filter(|r| r.last.as_deref() == Some(view.state.as_str()))
        {
            view.since_unix_ms = run.since;
        }
        let prompts =
            named.diagnostics.contains_key("prompt") || named.diagnostics.contains_key("update");
        let unstamped_auth = event
            .body
            .pointer("/fields/provider_auth")
            .and_then(Value::as_bool)
            .is_some()
            && event.body.pointer("/fields/observed_since_ms").is_none();
        if prompts || view.state == "needs-login" || unstamped_auth {
            let (state, transition) = named.machine.0[0];
            let state = if state == 0 {
                0
            } else {
                Status::decode(usize::from(state)).state()
            };
            let name = if state == 9 {
                "unauthenticated"
            } else {
                STATES[state]
            };
            let target = if view.state == "needs-login" {
                "unauthenticated"
            } else {
                &view.state
            };
            if transition != 0 && name == target {
                let event = ordered::event(connection, transition)?;
                view.since_unix_ms = if event.nested {
                    event.observed()
                } else {
                    event.accepted
                };
            }
        }
    }
    Ok(view)
}

/// Called only inside the authorized caller's SQLite snapshot. A row has no authority
/// of its own, and an uncertified source never invokes a canonical read fallback.
/// This returns observed harness state only. The work.claimed/work.progress overlay
/// used by current_harness is a separate dependency that must be covered before hookup.
pub fn read_harness(
    connection: &Connection,
    views: &smallclaims::ivm::Views,
    subject: &str,
) -> Result<Option<crate::model::CurrentHarnessView>> {
    let cut = smallclaims::ivm::source_cut(connection)?.context("agent source unavailable")?;
    views.token(connection, VIEW, cut.epoch)?;
    let row: Option<String> = connection
        .query_row(
            "SELECT observed FROM local_agent_harness_rows WHERE subject=?1",
            [subject],
            |r| r.get(0),
        )
        .optional()?;
    row.map(|row| serde_json::from_str(&row))
        .transpose()
        .map(Option::flatten)
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::{exchange_from, exchange_of, receive_and_project};
    use smallclaims::ivm::{Readiness, SourceCut, Views};
    struct Fixture {
        store: Store,
        views: Views,
    }
    impl Fixture {
        fn new() -> Self {
            let store = Store::open_memory("grove").unwrap();
            let views = Views::new(definitions()).unwrap();
            let mut connection = store.connection.write();
            let tx = connection.transaction().unwrap();
            views.create_schema(&tx).unwrap();
            views
                .initialize_empty(
                    &tx,
                    SourceCut {
                        epoch: 1,
                        admitted: 0,
                        projected: 0,
                        local_generation: 0,
                    },
                )
                .unwrap();
            tx.commit().unwrap();
            drop(connection);
            Self { store, views }
        }
        fn input(subject: &str, kind: &str, fields: Value) -> ClaimInput {
            ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: None,
                fields: serde_json::from_value(fields).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            }
        }
        // Admit raw canonical fixtures in a real Store transaction. The public latest-
        // retention path stamps/collapses observations, so it cannot exercise old sparse
        // and mixed histories. Runtime mutation and view maintenance commit together.
        fn append(&self, subject: &str, kind: &str, fields: Value) -> ClaimRecord {
            let input = Self::input(subject, kind, fields);
            validate_claim_input(&input).unwrap();
            let mut connection = self.store.connection.write();
            let tx = connection.transaction().unwrap();
            let claim = append_claim_tx(
                &tx,
                &self.store.origin,
                subject,
                kind,
                None,
                &json!({"fields":input.fields}),
                &[],
                None,
            )
            .unwrap();
            self.capture_tx(&tx, &claim);
            tx.commit().unwrap();
            drop(connection);
            self.check(subject);
            claim
        }
        fn capture(&self, claim: &ClaimRecord) {
            let mut connection = self.store.connection.write();
            let tx = connection.transaction().unwrap();
            self.capture_tx(&tx, claim);
            tx.commit().unwrap();
        }
        fn apply_tx(&self, tx: &Transaction<'_>, claim: &ClaimRecord) {
            let key = canonical::claim_key(tx, &claim.id).unwrap();
            let changes = self.views.change(tx, None, Some((claim, &key)), 1).unwrap();
            assert!(
                changes.deferred.is_empty(),
                "{:?}",
                self.views.availability(tx, VIEW, 1)
            );
        }
        fn publish_tx(&self, tx: &Transaction<'_>) {
            let index = current_index(tx).unwrap();
            self.views
                .publish_cut(
                    tx,
                    SourceCut {
                        epoch: 1,
                        admitted: index,
                        projected: index,
                        local_generation: 0,
                    },
                )
                .unwrap();
        }
        fn capture_tx(&self, tx: &Transaction<'_>, claim: &ClaimRecord) {
            self.apply_tx(tx, claim);
            self.publish_tx(tx);
        }
        fn capture_many(&self, claims: &[ClaimRecord]) {
            if claims.is_empty() {
                return;
            }
            let mut connection = self.store.connection.write();
            let tx = connection.transaction().unwrap();
            for claim in claims {
                self.apply_tx(&tx, claim);
            }
            // Only the complete captured transaction prefix may publish readiness.
            self.publish_tx(&tx);
            tx.commit().unwrap();
        }
        fn check(&self, subject: &str) {
            let connection = self.store.readers.get();
            let index = current_index(&connection).unwrap();
            let mut oracle =
                current_harness_fold_at(&connection, subject, Some(index), false, false).unwrap();
            if let Some(view) = oracle.as_mut() {
                seat_status::enrich_harness(&connection, subject, Some(index), view).unwrap();
            }
            let keyed = read_harness(&connection, &self.views, subject).unwrap();
            assert_eq!(
                serde_json::to_value(&keyed).unwrap(),
                serde_json::to_value(&oracle).unwrap(),
                "oracle mismatch {subject}"
            );
        }
    }
    const SEAT: &str = "agent/grove.cedar";
    #[test]
    fn ordered_agent_sparse_fields_legacy_since_and_incarnation_reset_match_canonical() {
        let f = Fixture::new();
        f.append(
            SEAT,
            "runtime.observed",
            json!({"status":"running","incarnation_id":"one"}),
        );
        f.append(SEAT,"harness.observed",json!({"state":"idle","driver":"codex","incarnation_id":"one","reason":"paused","input_buffer":"draft"}));
        for _ in 0..20 {
            f.append(
                SEAT,
                "harness.observed",
                json!({"state":"idle","incarnation_id":"one"}),
            );
        }
        f.append(SEAT,"harness.observed",json!({"state":"working","incarnation_id":"one","blocked_on":"human","ask":"permission"}));
        f.append(
            SEAT,
            "harness.observed",
            json!({"state":"working","incarnation_id":"one"}),
        );
        f.append(
            SEAT,
            "harness.observed",
            json!({"state":"ended","incarnation_id":"one"}),
        );
        f.append(
            SEAT,
            "harness.observed",
            json!({"state":"working","incarnation_id":"one","ask":null,"reason":null}),
        );
        f.append(
            SEAT,
            "runtime.observed",
            json!({"status":"running","incarnation_id":"two"}),
        );
        f.append(
            SEAT,
            "harness.observed",
            json!({"state":"ready","incarnation_id":"one"}),
        );
        f.append(
            SEAT,
            "harness.observed",
            json!({"state":"working","incarnation_id":"two","driver":"claude"}),
        );
        f.append(
            SEAT,
            "runtime.observed",
            json!({"status":"stopped","incarnation_id":"two"}),
        );
    }
    #[test]
    fn ordered_agent_native_fences_and_mixed_stamps_match_canonical() {
        let f = Fixture::new();
        f.append(
            SEAT,
            "runtime.observed",
            json!({"status":"running","incarnation_id":"one"}),
        );
        f.append(SEAT,"harness.observed",json!({"state":"working","driver":"codex","incarnation_id":"one","status_transition":true,"observed_since_ms":1,"observed_at_ms":2}));
        for auth in [false, true, false, true] {
            f.append(SEAT,"harness.observed",json!({"state":"working","driver":"codex","incarnation_id":"one","provider_auth":auth}));
            f.append(
                SEAT,
                "harness.observed",
                json!({"state":"working","incarnation_id":"one","status_transition":false}),
            );
        }
        for code in [
            "provider-auth-expired",
            "provider-auth-restored",
            "provider-trust-prompt",
            "provider-auth-restored",
            "provider-update-prompt",
            "provider-update-restored",
            "claude-channel-unattached",
            "claude-channel-attached",
        ] {
            f.append(
                SEAT,
                "harness.diagnostic",
                json!({"code":code,"incarnation_id":"one","driver":"claude","reason":"screen"}),
            );
            f.append(
                SEAT,
                "harness.observed",
                json!({"state":"idle","incarnation_id":"one","driver":"claude"}),
            );
            f.append(SEAT,"harness.observed",json!({"state":"idle","incarnation_id":"one","status_transition":false,"observed_since_ms":3}));
        }
        f.append(
            SEAT,
            "harness.diagnostic",
            json!({"code":"harness-admission-failed","incarnation_id":"one","reason":"probe"}),
        );
        f.append(
            SEAT,
            "runtime.observed",
            json!({"status":"stopped","incarnation_id":"one"}),
        );
        f.append(
            SEAT,
            "runtime.observed",
            json!({"status":"running","incarnation_id":"two"}),
        );
    }
    #[test]
    fn ordered_agent_unnamed_fields_merge_in_canonical_epoch() {
        let f = Fixture::new();
        f.append(
            SEAT,
            "harness.observed",
            json!({"state":"idle","reason":"old"}),
        );
        f.append(
            SEAT,
            "runtime.observed",
            json!({"status":"running","incarnation_id":"one"}),
        );
        f.append(
            SEAT,
            "harness.observed",
            json!({"state":"working","driver":"codex","reason":"unnamed"}),
        );
        f.append(
            SEAT,
            "harness.observed",
            json!({"state":"idle","incarnation_id":"one","transport":"native"}),
        );
        f.append(
            SEAT,
            "harness.observed",
            json!({"state":"working","reason":null,"incarnation_id":null}),
        );
        f.append(
            SEAT,
            "harness.observed",
            json!({"state":"ended","incarnation_id":"one"}),
        );
        f.append(
            SEAT,
            "runtime.observed",
            json!({"status":"running","incarnation_id":"two"}),
        );
        f.append(
            SEAT,
            "harness.observed",
            json!({"state":"working","incarnation_id":"one","reason":"old-incarnation"}),
        );
    }
    #[test]
    fn ordered_agent_unrelated_and_duplicate_inputs_change_no_public_row() {
        let f = Fixture::new();
        f.append(
            SEAT,
            "runtime.observed",
            json!({"status":"running","incarnation_id":"one"}),
        );
        let claim = f.append(
            SEAT,
            "harness.observed",
            json!({"state":"idle","incarnation_id":"one"}),
        );
        let connection = f.store.readers.get();
        let generation: u64 = connection
            .query_row(
                "SELECT generation FROM ivm_views WHERE name=?1",
                [VIEW],
                |r| r.get(0),
            )
            .unwrap();
        drop(connection);
        f.capture(&claim);
        f.append(
            "daemon/grove",
            "daemon.diagnostic",
            json!({"code":"test","reason":"unrelated","severity":"warning"}),
        );
        let connection = f.store.readers.get();
        let next: u64 = connection
            .query_row(
                "SELECT generation FROM ivm_views WHERE name=?1",
                [VIEW],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(next, generation);
        assert!(matches!(
            f.views.readiness(&connection, VIEW, 1).unwrap(),
            Readiness::Ready(_)
        ));
        f.check(SEAT);
    }
    #[test]
    fn ordered_agent_ties_permuted_replication_and_duplicate_arrival_match_canonical() {
        let alder = Store::open_memory("alder").unwrap();
        let birch = Store::open_memory("birch").unwrap();
        for source in [&alder, &birch] {
            source.set_write_clock_at(1_800_000_000_000).unwrap();
        }
        alder
            .append_claim(&Fixture::input(
                SEAT,
                "runtime.observed",
                json!({"status":"running","incarnation_id":"one"}),
            ))
            .unwrap();
        alder
            .append_claim(&Fixture::input(
                SEAT,
                "harness.observed",
                json!({"state":"working","incarnation_id":"one","driver":"codex","reason":"first"}),
            ))
            .unwrap();
        birch
            .append_claim(&Fixture::input(
                SEAT,
                "harness.observed",
                json!({"state":"idle","incarnation_id":"one","transport":"native","reason":null}),
            ))
            .unwrap();
        birch
            .append_claim(&Fixture::input(
                SEAT,
                "harness.diagnostic",
                json!({"code":"provider-auth-expired","incarnation_id":"one","driver":"codex"}),
            ))
            .unwrap();
        let mut envelopes = exchange_from(&alder, &ReplicationInventory::default()).envelopes;
        envelopes.extend(exchange_from(&birch, &ReplicationInventory::default()).envelopes);
        let forward = Fixture::new();
        let reverse = Fixture::new();
        for (f, order) in [
            (&forward, envelopes.clone()),
            (&reverse, envelopes.iter().rev().cloned().collect()),
        ] {
            for envelope in order.iter().chain(order.iter()) {
                let before = f.store.index().unwrap();
                receive_and_project(
                    &f.store,
                    "relay",
                    &exchange_of("relay", vec![envelope.clone()]),
                );
                let claims = {
                    let connection = f.store.readers.get();
                    connection.prepare(&format!("SELECT {CLAIM_COLUMNS} FROM claims JOIN batches ON batches.id=claims.batch_id WHERE claims.store_index>?1 ORDER BY claims.store_index")).unwrap()
                        .query_map([before],claim_from_row).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()
                };
                f.capture_many(&claims);
                f.check(SEAT);
            }
        }
        let left = read_harness(&forward.store.readers.get(), &forward.views, SEAT).unwrap();
        let right = read_harness(&reverse.store.readers.get(), &reverse.views, SEAT).unwrap();
        assert_eq!(
            serde_json::to_value(left).unwrap(),
            serde_json::to_value(right).unwrap()
        );
    }
    #[test]
    fn ordered_agent_rollback_preserves_rows_and_frontiers() {
        let f = Fixture::new();
        f.append(
            SEAT,
            "runtime.observed",
            json!({"status":"running","incarnation_id":"one"}),
        );
        f.append(
            SEAT,
            "harness.observed",
            json!({"state":"idle","incarnation_id":"one"}),
        );
        let before = read_harness(&f.store.readers.get(), &f.views, SEAT).unwrap();
        let index = f.store.index().unwrap();
        {
            let mut connection = f.store.connection.write();
            let tx = connection.transaction().unwrap();
            let claim = append_claim_tx(
                &tx,
                "grove",
                SEAT,
                "harness.observed",
                None,
                &json!({"fields":{"state":"working","incarnation_id":"one"}}),
                &[],
                None,
            )
            .unwrap();
            let key = canonical::claim_key(&tx, &claim.id).unwrap();
            f.views.change(&tx, None, Some((&claim, &key)), 1).unwrap();
            let next = current_index(&tx).unwrap();
            f.views
                .publish_cut(
                    &tx,
                    SourceCut {
                        epoch: 1,
                        admitted: next,
                        projected: next,
                        local_generation: 0,
                    },
                )
                .unwrap();
            assert_eq!(
                read_harness(&tx, &f.views, SEAT).unwrap().unwrap().state,
                "working"
            );
            tx.rollback().unwrap();
        }
        assert_eq!(f.store.index().unwrap(), index);
        assert_eq!(
            serde_json::to_value(read_harness(&f.store.readers.get(), &f.views, SEAT).unwrap())
                .unwrap(),
            serde_json::to_value(before).unwrap()
        );
        f.check(SEAT);
    }
    #[test]
    fn ordered_agent_source_pending_and_canonical_mutations_fence_keyed_reads() {
        let f = Fixture::new();
        f.append(
            SEAT,
            "runtime.observed",
            json!({"status":"running","incarnation_id":"one"}),
        );
        let claim = f
            .store
            .append_claim(&Fixture::input(
                SEAT,
                "harness.observed",
                json!({"state":"idle","incarnation_id":"one"}),
            ))
            .unwrap();
        assert!(read_harness(&f.store.readers.get(), &f.views, SEAT).is_err());
        f.capture(&claim);
        f.check(SEAT);
        {
            let connection = f.store.connection.write();
            connection
                .execute(
                    "UPDATE claims SET body=json_set(body,'$.fields.reason','edited') WHERE id=?1",
                    [claim.id],
                )
                .unwrap();
        }
        assert!(matches!(
            f.views.readiness(&f.store.readers.get(), VIEW, 1).unwrap(),
            Readiness::Fenced
        ));
        assert!(read_harness(&f.store.readers.get(), &f.views, SEAT).is_err());
    }
    #[test]
    fn ordered_agent_keyed_read_cost_does_not_grow_with_sparse_history() {
        let f = Fixture::new();
        f.append(
            SEAT,
            "runtime.observed",
            json!({"status":"running","incarnation_id":"one"}),
        );
        let mut costs = Vec::new();
        for size in [32, 256] {
            for _ in costs.len() * 32..size {
                let mut connection = f.store.connection.write();
                let tx = connection.transaction().unwrap();
                let claim = append_claim_tx(
                    &tx,
                    "grove",
                    SEAT,
                    "harness.observed",
                    None,
                    &json!({"fields":{"state":"working","incarnation_id":"one"}}),
                    &[],
                    None,
                )
                .unwrap();
                let before = smallclaims::sqlite::work::total();
                f.capture_tx(&tx, &claim);
                let maintenance = smallclaims::sqlite::work::total() - before;
                if claim.store_index == size as u64 {
                    eprintln!(
                        "ordered-agent maintenance observations={size} statements={} vm_steps={}",
                        maintenance.statements, maintenance.vm_steps
                    );
                }
                tx.commit().unwrap();
            }
            f.check(SEAT);
            let connection = f.store.readers.get();
            // Count this connection's VM instructions. Process-wide work totals include
            // other parallel controls and cannot establish a growth bound.
            let instructions = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let counter = instructions.clone();
            connection.progress_handler(
                1,
                Some(move || {
                    counter.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            );
            let before = STATEMENTS_RUN.with(std::cell::Cell::get);
            let row = read_harness(&connection, &f.views, SEAT);
            let statements = STATEMENTS_RUN.with(std::cell::Cell::get) - before;
            connection.progress_handler(0, None::<fn() -> bool>);
            let row = row.unwrap().unwrap();
            let vm_steps = instructions.load(Ordering::Relaxed);
            assert_eq!(row.state, "working");
            costs.push(vm_steps);
            eprintln!(
                "ordered-agent keyed-read observations={size} statements={statements} vm_steps={vm_steps}"
            );
        }
        assert!(
            costs[1] == costs[0],
            "history-dependent keyed read: {costs:?}"
        );
    }
    #[test]
    fn ordered_agent_retained_original_repair_and_deleted_claim_are_explicit() {
        let f = Fixture::new();
        f.append(
            SEAT,
            "runtime.observed",
            json!({"status":"running","incarnation_id":"one"}),
        );
        let original = f.append(
            SEAT,
            "harness.observed",
            json!({"state":"idle","incarnation_id":"one","driver":"codex","reason":"old"}),
        );
        let replacement = f.append(
            SEAT,
            "harness.observed",
            json!({"state":"working","incarnation_id":"one","reason":null}),
        );
        {
            let mut connection = f.store.connection.write();
            let tx = connection.transaction().unwrap();
            let key = canonical::claim_key(&tx, &replacement.id).unwrap();
            let changes = f
                .views
                .repair(&tx, &original, (&replacement, &key), 1)
                .unwrap();
            assert!(changes.deferred.is_empty());
            assert!(changes.changed.is_empty());
            let retained: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM ivm_contributions WHERE view=?1 AND claim_id=?2)",
                    params![VIEW, original.id],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                retained,
                "harness repair must retain the canonical original"
            );
            tx.commit().unwrap();
        }
        f.check(SEAT);
        f.store
            .connection
            .write()
            .execute("DELETE FROM claims WHERE id=?1", [replacement.id])
            .unwrap();
        assert!(
            read_harness(&f.store.readers.get(), &f.views, SEAT).is_err(),
            "delete requires explicit reinstallation; never serve an old row"
        );
    }
    #[test]
    fn ordered_agent_bare_legacy_fields_and_nontext_incarnation_match_canonical() {
        let f = Fixture::new();
        for (kind, body) in [
            (
                "runtime.observed",
                json!({"status":"running","incarnation_id":"one"}),
            ),
            (
                "harness.observed",
                json!({"state":"idle","incarnation_id":"one","driver":"codex","reason":"legacy"}),
            ),
            (
                "harness.observed",
                json!({"state":"working","incarnation_id":17,"transport":"native","reason":null}),
            ),
            (
                "harness.observed",
                json!({"state":"ended","incarnation_id":"one"}),
            ),
            (
                "harness.observed",
                json!({"incarnation_id":"one","blocked_on":"human","ask":"permission"}),
            ),
            (
                "harness.observed",
                json!({"state":"working","incarnation_id":"one","status_transition":false}),
            ),
            (
                "harness.observed",
                json!({"state":"idle","incarnation_id":"one"}),
            ),
            (
                "harness.observed",
                json!({"state":"blocked","incarnation_id":"one","blocked_on":"human"}),
            ),
        ] {
            let mut connection = f.store.connection.write();
            let tx = connection.transaction().unwrap();
            let claim = smallclaims::store::append_claim_record_tx(
                &tx,
                "grove",
                SEAT,
                kind,
                None,
                &body,
                &[],
                None,
            )
            .unwrap();
            f.capture_tx(&tx, &claim);
            tx.commit().unwrap();
            drop(connection);
            f.check(SEAT);
        }
    }
    #[test]
    fn ordered_agent_unsupported_states_and_null_admission_reason_fence_with_evidence() {
        for (kind, fields, expected) in [
            (
                "harness.observed",
                json!({"state":"","incarnation_id":"one"}),
                "unsupported legacy harness state",
            ),
            (
                "harness.observed",
                json!({"state":"legacy-unknown","incarnation_id":"one"}),
                "unsupported legacy harness state",
            ),
            (
                "harness.observed",
                json!({"state":"","incarnation_id":"one","status_transition":false}),
                "unsupported legacy harness state",
            ),
            (
                "harness.diagnostic",
                json!({"code":"harness-admission-failed","incarnation_id":"one","reason":null}),
                "non-text harness admission reason",
            ),
        ] {
            let f = Fixture::new();
            f.append(
                SEAT,
                "runtime.observed",
                json!({"status":"running","incarnation_id":"one"}),
            );
            f.append(
                SEAT,
                "harness.observed",
                json!({"state":"idle","incarnation_id":"one"}),
            );
            let mut connection = f.store.connection.write();
            let tx = connection.transaction().unwrap();
            let claim = smallclaims::store::append_claim_record_tx(
                &tx,
                "grove",
                SEAT,
                kind,
                None,
                &json!({"fields":fields}),
                &[],
                None,
            )
            .unwrap();
            let key = canonical::claim_key(&tx, &claim.id).unwrap();
            let changes = f.views.change(&tx, None, Some((&claim, &key)), 1).unwrap();
            assert!(changes.deferred.contains(VIEW));
            assert!(changes.changed.is_empty());
            f.publish_tx(&tx);
            tx.commit().unwrap();
            drop(connection);
            let connection = f.store.readers.get();
            let status = f.views.availability(&connection, VIEW, 1).unwrap();
            assert!(matches!(status.readiness, Readiness::Fenced));
            assert!(status.error.unwrap().contains(expected));
            assert!(read_harness(&connection, &f.views, SEAT).is_err());
            if kind == "harness.diagnostic" {
                assert!(current_harness_fold_at(&connection, SEAT, None, false, false).is_err());
            }
        }
    }
}
