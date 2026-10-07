//! Namespaced agent harness rows maintained by canonical, affected-key edits.
//!
//! The ordered operator handles sparse legacy fields and status episodes without replaying
//! a seat's history. This module does not certify the surrounding declaration, work queue,
//! placement or person joins: the production registry must cover those dependencies before
//! using this source as a complete public card.
use super::*;
use serde::Deserialize;
use smallclaims::ivm::install::Namespace;

#[path = "agent_card_harness/ordered.rs"]
mod ordered;

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

/// This is an internal dependency, not an independently Ready public view.
pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(ordered::SCHEMA)?;
    connection.execute_batch("CREATE TABLE IF NOT EXISTS local_agent_card_harness_rows(namespace TEXT NOT NULL,subject TEXT NOT NULL,observed TEXT NOT NULL,PRIMARY KEY(namespace,subject));")?;
    Ok(())
}

pub(super) fn apply_claim(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    old: Option<&ClaimRecord>,
    new: Option<(&ClaimRecord, &canonical::ClaimKey)>,
) -> Result<BTreeSet<String>> {
    apply(tx, namespace.as_str(), old, new)
}

fn apply(
    tx: &Transaction<'_>,
    namespace: &str,
    old: Option<&ClaimRecord>,
    new: Option<(&ClaimRecord, &canonical::ClaimKey)>,
) -> Result<BTreeSet<String>> {
    let mut agents = BTreeSet::new();
    // The current staged value can differ from the mutation's old value when
    // extraction raced ahead of journal catch-up. Its old subject is a dependency.
    for claim in old.into_iter().chain(new.map(|(claim, _)| claim)) {
        if let Some(agent) = tx.query_row("SELECT subject FROM local_agent_card_harness_nodes WHERE namespace=?1 AND claim=?2",params![namespace,claim.id],|r|r.get::<_,String>(0)).optional()? {
            agents.insert(agent);
        }
    }
    if let Some(old) =
        old.filter(|c| c.subject.starts_with("agent/") && KINDS.contains(&c.kind.as_str()))
    {
        ordered::retract(tx, namespace, &old.id)?;
        agents.insert(old.subject.clone());
    }
    if let Some((claim, key)) =
        new.filter(|(c, _)| c.subject.starts_with("agent/") && KINDS.contains(&c.kind.as_str()))
    {
        let event = Event {
            claim: claim.id.clone(),
            subject: claim.subject.clone(),
            kind: claim.kind.clone(),
            rank: canonical::sortable_key(key),
            accepted: claim.accepted_at_unix_ms,
            fields: claim.body.get("fields").unwrap_or(&claim.body).clone(),
            nested: claim.body.get("fields").is_some(),
        };
        ordered::admit(tx, namespace, &event)?;
        agents.insert(claim.subject.clone());
    }
    for agent in &agents {
        let next = serde_json::to_string(&observed(tx, namespace, agent)?)?;
        tx.execute("INSERT INTO local_agent_card_harness_rows VALUES(?1,?2,?3) ON CONFLICT(namespace,subject) DO UPDATE SET observed=excluded.observed",params![namespace,agent,next])?;
    }
    Ok(agents)
}

pub(super) fn read_harness(
    connection: &Connection,
    namespace: &Namespace,
    agent: &str,
) -> Result<Option<crate::model::CurrentHarnessView>> {
    let value: Option<String> = connection
        .query_row(
            "SELECT observed FROM local_agent_card_harness_rows WHERE namespace=?1 AND subject=?2",
            params![namespace.as_str(), agent],
            |r| r.get(0),
        )
        .optional()?;
    value
        .map(|value| Ok(serde_json::from_str(&value)?))
        .transpose()
        .map(Option::flatten)
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

fn observed(
    connection: &Connection,
    namespace: &str,
    subject: &str,
) -> Result<Option<crate::model::CurrentHarnessView>> {
    let runtime: Option<String> = connection
        .query_row(
            "SELECT event FROM local_agent_card_harness_nodes WHERE namespace=?1 AND subject=?2 AND kind='runtime.observed' ORDER BY rank DESC LIMIT 1",
            params![namespace, subject],
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
    let named = ordered::all(connection, namespace, subject, Some(incarnation))?;
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
        let unnamed = ordered::unnamed_after(connection, namespace, subject, &runtime.rank)?;
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
                let event = ordered::event(connection, namespace, transition)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    const SEAT: &str = "agent/grove.cedar";
    struct Fixture {
        store: Store,
    }
    impl Fixture {
        fn new() -> Self {
            let store = Store::open_memory("grove").unwrap();
            create_schema(&store.connection.write()).unwrap();
            Self { store }
        }
        fn append(&self, subject: &str, kind: &str, fields: Value) -> ClaimRecord {
            let mut connection = self.store.connection.write();
            let tx = connection.transaction().unwrap();
            let claim = append_claim_tx(
                &tx,
                &self.store.origin,
                subject,
                kind,
                None,
                &json!({"fields":fields}),
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
        fn capture_tx(&self, tx: &Transaction<'_>, claim: &ClaimRecord) {
            let key = canonical::claim_key(tx, &claim.id).unwrap();
            apply(tx, "live", None, Some((claim, &key))).unwrap();
        }
        fn check(&self, subject: &str) {
            let connection = self.store.readers.get();
            let index = current_index(&connection).unwrap();
            let mut oracle =
                current_harness_fold_at(&connection, subject, Some(index), false, false).unwrap();
            if let Some(view) = oracle.as_mut() {
                seat_status::enrich_harness(&connection, subject, Some(index), view).unwrap();
            }
            assert_eq!(
                serde_json::to_value(observed(&connection, "live", subject).unwrap()).unwrap(),
                serde_json::to_value(oracle).unwrap()
            );
        }
    }

    #[test]
    fn populated_reverse_capture_and_namespace_replacements_are_isolated() {
        let f = Fixture::new();
        let mut claims = vec![f.append(
            SEAT,
            "runtime.observed",
            json!({"status":"running","incarnation_id":"one"}),
        )];
        let mut seed = 0x713b_u64;
        for _ in 0..64 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let state = ["idle", "working", "blocked", "ready"][(seed >> 32) as usize % 4];
            claims.push(f.append(
                SEAT,
                "harness.observed",
                json!({"state":state,"incarnation_id":"one","driver":"codex"}),
            ));
        }
        let mut connection = f.store.connection.write();
        let tx = connection.transaction().unwrap();
        for claim in claims.iter().rev() {
            let key = canonical::claim_key(&tx, &claim.id).unwrap();
            apply(&tx, "staging", None, Some((claim, &key))).unwrap();
            apply(&tx, "staging", None, Some((claim, &key))).unwrap();
        }
        let live = observed(&tx, "live", SEAT).unwrap();
        assert_eq!(
            serde_json::to_value(observed(&tx, "staging", SEAT).unwrap()).unwrap(),
            serde_json::to_value(&live).unwrap()
        );
        assert!(observed(&tx, "missing", SEAT).unwrap().is_none());
        // A staged replacement cannot mutate the published namespace, including
        // per-namespace transition node IDs and the latest runtime selection.
        apply(&tx, "staging", Some(&claims[0]), None).unwrap();
        assert!(observed(&tx, "staging", SEAT).unwrap().is_none());
        assert_eq!(
            serde_json::to_value(observed(&tx, "live", SEAT).unwrap()).unwrap(),
            serde_json::to_value(live).unwrap()
        );
        tx.rollback().unwrap();
        assert!(observed(&connection, "staging", SEAT).unwrap().is_none());
        drop(connection);
        f.check(SEAT);
    }
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
}
