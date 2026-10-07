//! Lifetimes of open PEEK streams, independent of single-use acquisition capabilities.
use super::*;
use rusqlite::{Connection, OptionalExtension as _, params};
use std::sync::{LazyLock, Mutex};
use tokio::time::Instant;

pub(crate) const IDLE: Duration = Duration::from_secs(60);
pub(crate) const ABSOLUTE: Duration = Duration::from_secs(300);
pub(crate) const WATCH: Duration = Duration::from_secs(5);
pub(crate) const ORIGIN_HEADER: &str = "x-st3-raw-origin";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub lease_id: String,
    pub gateway: String,
    pub gateway_epoch: String,
    pub actor: String,
    pub person: String,
    pub authorization_epoch: String,
    pub grant_subject: Option<String>,
    pub grant_digest: Option<String>,
    pub gateway_member_key: Option<String>,
    pub owner_member_key: Option<String>,
    pub terminal: String,
    pub owner: String,
    pub incarnation: String,
    pub mode: String,
    pub absolute_deadline_unix_ms: u128,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum Control {
    SelectedUse {
        sequence: u64,
    },
    AuthorityProof {
        lease_id: String,
        watcher_epoch: String,
        sequence: u64,
    },
}

struct Clock {
    idle: Instant,
    absolute: Instant,
    proof: Instant,
    activity_sequence: u64,
    proof_sequence: u64,
}

impl Clock {
    fn expired_code(&self, now: Instant) -> Option<&'static str> {
        if now >= self.absolute {
            Some("lease-absolute-expired")
        } else if now >= self.idle {
            Some("lease-idle-expired")
        } else if now >= self.proof {
            Some("lease-watch-lost")
        } else {
            None
        }
    }
}

pub(crate) struct Lease {
    pub binding: Binding,
    owner_side: bool,
    snapshot: String,
    acquisition_epoch: String,
    membership: MembershipAuthority,
    store: std::sync::Weak<Store>,
    clock: Mutex<Clock>,
    cancelled: watch::Sender<Option<&'static str>>,
    observer: Mutex<Option<smallclaims::sqlite::CommitObserver>>,
}

fn epoch() -> &'static str {
    static EPOCH: LazyLock<String> = LazyLock::new(|| uuid::Uuid::now_v7().to_string());
    &EPOCH
}

fn latest(
    connection: &Connection,
    subject: &str,
    kind: &str,
) -> anyhow::Result<Option<(String, Value)>> {
    let query = smallclaims::store::canonical_sql(
        "SELECT origin,body FROM claims WHERE subject=?1 AND kind=?2 ORDER BY CANONICAL_DESC(claims) LIMIT 1",
    );
    let row: Option<(String, String)> = connection
        .query_row(&query, params![subject, kind], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?;
    row.map(|(origin, body)| Ok((origin, serde_json::from_str(&body)?)))
        .transpose()
}

/// A local free-person session has an explicit daemon/principal epoch, not a fabricated grant.
/// Revoking a principal key or changing its declaration changes this epoch synchronously.
fn principal_epoch(connection: &Connection, person: &str) -> anyhow::Result<Value> {
    let mut statement = connection.prepare(
        "SELECT id FROM claims WHERE subject=?1 AND kind IN ('principal.key-granted','principal.key-revoked') ORDER BY id",
    )?;
    let keys = statement
        .query_map([person], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let revision: Option<String> = connection
        .query_row(
            "SELECT revision FROM desired WHERE subject=?1",
            [person],
            |row| row.get(0),
        )
        .optional()?;
    Ok(json!({"daemon_epoch":epoch(), "person":person, "revision":revision, "keys":keys}))
}

fn authority(
    connection: &Connection,
    actor: &str,
    person: &str,
    pairing_grant: Option<&str>,
) -> anyhow::Result<Value> {
    let principal = principal_epoch(connection, person)?;
    if actor == person {
        return Ok(json!({"local_principal_epoch":principal}));
    }
    // Two pairings of one person and device key derive the same session actor, so the
    // authenticated session carries the exact grant subject instead of searching by actor.
    let subject = pairing_grant
        .ok_or_else(|| anyhow::anyhow!("the paired client session has no exact grant"))?;
    let Some((issuer, paired)) = latest(connection, subject, "custom.client.pairing-completed")?
    else {
        anyhow::bail!("the paired session grant is absent");
    };
    anyhow::ensure!(
        paired
            .pointer("/fields/session_actor")
            .and_then(Value::as_str)
            == Some(actor),
        "session actor changed"
    );
    anyhow::ensure!(
        paired.pointer("/fields/person_id").and_then(Value::as_str) == Some(person),
        "person changed"
    );
    anyhow::ensure!(
        paired
            .pointer("/fields/expires_at_unix_ms")
            .and_then(Value::as_u64)
            .map(u128::from)
            .is_some_and(|expiry| expiry > client_now_ms()),
        "grant expired"
    );
    anyhow::ensure!(
        paired
            .pointer("/fields/scopes")
            .and_then(Value::as_array)
            .is_some_and(|scopes| scopes
                .iter()
                .any(|scope| scope.as_str() == Some("terminal.read"))),
        "terminal.read revoked"
    );
    // A revoked exact grant is never reused, whatever other pairings share its actor.
    anyhow::ensure!(
        latest(connection, subject, "custom.client.pairing-revoked")?.is_none(),
        "pairing revoked"
    );
    Ok(
        json!({"principal":principal,"pairing":subject,"grant":paired,"issuer":client_host_id(&issuer)}),
    )
}

pub(super) fn authorization_epoch(
    state: &AppState,
    session: &ClientSession,
) -> Result<String, ApiError> {
    let connection = state.store.readers.get();
    let authority = authority(
        &connection,
        &session.actor,
        &session.authority_actor,
        session.pairing_grant.as_deref(),
    )
    .map_err(|error| forbidden(error.to_string()))?;
    if authority
        .get("issuer")
        .and_then(Value::as_str)
        .is_some_and(|issuer| issuer != client_host_id(&state.node))
    {
        return Err(forbidden(
            "raw PEEK requires the pairing issuer gateway's authoritative watch",
        ));
    }
    serde_json::to_string(&authority)
        .map(|authority| credential_digest(&authority))
        .map_err(ApiError::internal)
}
struct MembershipAuthority {
    node: String,
    local_key: Option<String>,
    peers: Vec<String>,
    legacy: bool,
}

impl MembershipAuthority {
    fn validate(&self, connection: &Connection, binding: &Binding) -> anyhow::Result<()> {
        let membership = smallclaims::store::fleet_membership_tx_with_local_signer(
            connection,
            self.local_key
                .as_deref()
                .map(|key| (self.node.as_str(), key)),
        )?;
        let view = smallclaims::fleet::FleetView::from_membership(&membership);
        for (host, key) in [
            (&binding.gateway, &binding.gateway_member_key),
            (&binding.owner, &binding.owner_member_key),
        ] {
            let name = host.trim_start_matches("host/");
            if name == self.node && key.is_none() && view.anchor.is_none() {
                continue;
            }
            let sender = crate::fleet::Sender {
                name: name.into(),
                member_key: key.clone(),
                member_signature_valid: key.is_some(),
            };
            crate::fleet::accept(
                &view,
                &sender,
                name == self.node || self.peers.iter().any(|peer| peer == name),
                self.legacy,
            )
            .map_err(|error| anyhow::anyhow!(error.message))?;
        }
        Ok(())
    }
}

fn snapshot(
    connection: &Connection,
    binding: &Binding,
    owner_side: bool,
    acquisition_epoch: &str,
    membership: &MembershipAuthority,
) -> anyhow::Result<String> {
    membership.validate(connection, binding)?;
    let subject = terminal_subject(&binding.terminal);
    let runtime = Store::runtime_authority_on(connection, &subject)?
        .ok_or_else(|| anyhow::anyhow!("runtime missing"))?;
    anyhow::ensure!(
        matches!(runtime.reachability.as_str(), "reachable" | "local"),
        "runtime authority indeterminate"
    );
    let origin = runtime
        .actual_origin
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("runtime owner missing"))?;
    let actual = runtime
        .actual
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("runtime missing"))?;
    let fields = actual.get("fields").unwrap_or(actual);
    anyhow::ensure!(client_host_id(origin) == binding.owner, "owner changed");
    anyhow::ensure!(
        fields.get("incarnation_id").and_then(Value::as_str) == Some(binding.incarnation.as_str()),
        "incarnation changed"
    );
    anyhow::ensure!(
        fields.get("status").and_then(Value::as_str) == Some("running"),
        "runtime exited"
    );
    let authority = authority(
        connection,
        if owner_side {
            &binding.person
        } else {
            &binding.actor
        },
        &binding.person,
        binding.grant_subject.as_deref(),
    )?;
    let current_epoch = credential_digest(&serde_json::to_string(&authority)?);
    anyhow::ensure!(
        current_epoch == acquisition_epoch,
        "acquisition authorization epoch changed"
    );
    anyhow::ensure!(
        owner_side || current_epoch == binding.authorization_epoch,
        "original authorization epoch changed"
    );
    if !owner_side {
        anyhow::ensure!(
            authority
                .get("issuer")
                .and_then(Value::as_str)
                .is_none_or(|issuer| issuer == binding.gateway),
            "pairing issuer watch unavailable"
        );
    }
    if owner_side && binding.actor != binding.person {
        let grant_subject = binding
            .grant_subject
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("original grant identity missing"))?;
        let grant_digest = binding
            .grant_digest
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("original grant digest missing"))?;
        anyhow::ensure!(
            latest(connection, grant_subject, "custom.client.pairing-revoked")?.is_none(),
            "original pairing revoked on owner"
        );
        if let Some((_, grant)) =
            latest(connection, grant_subject, "custom.client.pairing-completed")?
        {
            anyhow::ensure!(
                credential_digest(&serde_json::to_string(&grant)?) == grant_digest,
                "original pairing changed on owner"
            );
        }
    }
    let revision: Option<String> = connection
        .query_row(
            "SELECT revision FROM desired WHERE subject=?1",
            [&subject],
            |row| row.get(0),
        )
        .optional()?;
    Ok(credential_digest(&serde_json::to_string(
        &json!({"authority":authority,"runtime":fields.get("runtime_id"),"owner":origin,"revision":revision}),
    )?))
}

impl Lease {
    pub(super) fn register(
        state: &AppState,
        session: &ClientSession,
        terminal: &str,
        owner: &str,
        incarnation: &str,
        origin: Option<Binding>,
        acquisition_epoch: &str,
    ) -> Result<Arc<Self>, ApiError> {
        let membership = MembershipAuthority {
            node: state.node.clone(),
            local_key: state.store.member_public_key(),
            peers: state.configured_peers.clone(),
            legacy: state
                .client_relay
                .as_ref()
                .is_none_or(|relay| relay.legacy_authority()),
        };
        let owner_side = origin.is_some();
        let binding = if let Some(binding) = origin {
            if binding.mode != "peek"
                || binding.person != session.authority_actor
                || binding.terminal != terminal
                || binding.owner != owner
                || binding.incarnation != incarnation
                || binding.gateway_epoch.is_empty()
                || binding.authorization_epoch.is_empty()
            {
                return Err(forbidden("raw lease origin binding differs"));
            }
            binding
        } else {
            let connection = state.store.readers.get();
            let authority = authority(
                &connection,
                &session.actor,
                &session.authority_actor,
                session.pairing_grant.as_deref(),
            )
            .map_err(|error| forbidden(error.to_string()))?;
            let authorization_epoch = acquisition_epoch.to_owned();
            let absolute_deadline_unix_ms = authority
                .pointer("/grant/fields/expires_at_unix_ms")
                .and_then(Value::as_u64)
                .map(u128::from)
                .unwrap_or(u128::MAX)
                .min(client_now_ms() + ABSOLUTE.as_millis());
            let view = state
                .store
                .fleet_view_for_client()
                .map_err(ApiError::internal)?;
            let owner_member_key = view
                .current(owner.trim_start_matches("host/"))
                .first()
                .map(|member| member.member_key.clone());
            let grant_subject = authority
                .get("pairing")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let grant_digest = authority
                .get("grant")
                .map(|grant| serde_json::to_string(grant).map(|body| credential_digest(&body)))
                .transpose()
                .map_err(ApiError::internal)?;
            Binding {
                lease_id: new_request_id(),
                gateway: client_host_id(&state.node),
                gateway_epoch: epoch().into(),
                actor: session.actor.clone(),
                person: session.authority_actor.clone(),
                authorization_epoch,
                grant_subject,
                grant_digest,
                gateway_member_key: membership.local_key.clone(),
                owner_member_key,
                terminal: terminal.into(),
                owner: owner.into(),
                incarnation: incarnation.into(),
                mode: "peek".into(),
                absolute_deadline_unix_ms,
            }
        };
        let initial = snapshot(
            &state.store.readers.get(),
            &binding,
            owner_side,
            acquisition_epoch,
            &membership,
        )
        .map_err(|error| stale(error.to_string()))?;
        let now = Instant::now();
        // Convert the signed acquisition-time wall fence once; all ongoing deadlines are monotonic.
        let remaining = Duration::from_millis(
            u64::try_from(
                binding
                    .absolute_deadline_unix_ms
                    .saturating_sub(client_now_ms())
                    .min(ABSOLUTE.as_millis()),
            )
            .map_err(ApiError::internal)?,
        );
        let lease = Arc::new(Self {
            binding,
            owner_side,
            snapshot: initial,
            acquisition_epoch: acquisition_epoch.into(),
            membership,
            store: Arc::downgrade(&state.store),
            clock: Mutex::new(Clock {
                idle: now + IDLE,
                absolute: now + remaining,
                proof: now + WATCH,
                activity_sequence: 0,
                proof_sequence: 0,
            }),
            cancelled: watch::channel(None).0,
            observer: Mutex::new(None),
        });
        let weak = Arc::downgrade(&lease);
        let observer = state.store.observe_commits(move |connection| {
            if let Some(lease) = weak.upgrade()
                && !snapshot(
                    connection,
                    &lease.binding,
                    lease.owner_side,
                    &lease.acquisition_epoch,
                    &lease.membership,
                )
                .is_ok_and(|current| current == lease.snapshot)
            {
                lease.cancel("lease-revoked");
            }
        });
        *lease
            .observer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(observer);
        // Subscribe/register first, then recheck. A concurrent mutation either invalidates the
        // registered lease or is visible in this recheck; neither path exposes bytes.
        lease
            .revalidate()
            .map_err(|error| stale(error.to_string()))?;
        Ok(lease)
    }

    pub(crate) fn owner_side(&self) -> bool {
        self.owner_side
    }
    pub(crate) fn has_proof(&self) -> bool {
        self.clock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .proof_sequence
            > 0
    }
    pub(crate) fn cancel(&self, code: &'static str) {
        self.cancelled.send_if_modified(|state| {
            if state.is_none() {
                *state = Some(code);
                true
            } else {
                false
            }
        });
    }

    pub(crate) fn revalidate(&self) -> anyhow::Result<()> {
        self.check()?;
        let store = self
            .store
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("authority stopped"))?;
        let current = snapshot(
            &store.readers.get(),
            &self.binding,
            self.owner_side,
            &self.acquisition_epoch,
            &self.membership,
        );
        if !current.is_ok_and(|current| current == self.snapshot) {
            self.cancel("lease-revoked");
            anyhow::bail!("lease revoked");
        }
        self.check()
    }

    pub(crate) fn check(&self) -> anyhow::Result<()> {
        if let Some(code) = *self.cancelled.borrow() {
            anyhow::bail!(code);
        }
        let clock = self
            .clock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        let code = clock.expired_code(now);
        drop(clock);
        if let Some(code) = code {
            self.cancel(code);
            anyhow::bail!(code);
        }
        Ok(())
    }

    pub(crate) fn selected_use(&self, sequence: u64) -> anyhow::Result<()> {
        self.revalidate()?;
        let mut clock = self
            .clock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        if let Some(code) = clock.expired_code(now) {
            drop(clock);
            self.cancel(code);
            anyhow::bail!(code);
        }
        if let Some(code) = *self.cancelled.borrow() {
            anyhow::bail!(code);
        }
        anyhow::ensure!(
            sequence > clock.activity_sequence,
            "selected-use sequence replay"
        );
        clock.activity_sequence = sequence;
        clock.idle = now + IDLE;
        Ok(())
    }

    pub(crate) fn proof(&self, epoch: &str, sequence: u64) -> anyhow::Result<()> {
        self.revalidate()?;
        anyhow::ensure!(epoch == self.binding.gateway_epoch, "watch epoch changed");
        let mut clock = self
            .clock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        if let Some(code) = clock.expired_code(now) {
            drop(clock);
            self.cancel(code);
            anyhow::bail!(code);
        }
        if let Some(code) = *self.cancelled.borrow() {
            anyhow::bail!(code);
        }
        anyhow::ensure!(sequence > clock.proof_sequence, "watch proof replay");
        clock.proof_sequence = sequence;
        clock.proof = now + WATCH;
        Ok(())
    }

    /// This task is selected alongside both byte pumps, never behind a blocked byte write.
    pub(crate) async fn expired(&self) {
        let mut cancelled = self.cancelled.subscribe();
        loop {
            if self.check().is_err() {
                return;
            }
            let deadline = {
                let clock = self
                    .clock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                clock.idle.min(clock.absolute).min(clock.proof)
            };
            tokio::select! { biased; result = cancelled.changed() => { if result.is_err() || cancelled.borrow().is_some() { return; } }, () = tokio::time::sleep_until(deadline) => {} }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, AppState, ClientSession) {
        let root = tempfile::tempdir().unwrap();
        let state = AppState {
            store: Arc::new(Store::open_memory("lease-owner").unwrap()),
            notify: Arc::new(Notify::new()),
            event_notify: watch::channel(0).0,
            node: "lease-owner".into(),
            state_dir: root.path().into(),
            pty_root: root.path().join("pty"),
            pty_binary: root.path().join("unused-pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: crate::model::PlannerSpec::default(),
        };
        claim(
            &state,
            "agent/shell",
            "runtime.observed",
            json!({"status":"running","runtime_id":"runtime-shell","incarnation_id":"incarnation-one","terminal":true}),
        );
        (
            root,
            state,
            ClientSession::local(Some("person/alex")).unwrap(),
        )
    }

    fn claim(state: &AppState, subject: &str, kind: &str, fields: Value) {
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: None,
                fields: fields
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }

    fn pairing(state: &AppState, subject: &str, credential: &str, scopes: Value) {
        claim(
            state,
            subject,
            "custom.client.pairing-completed",
            json!({
                "person_id":"person/alex",
                "session_actor":"person/alex/session/same-key",
                "credential_hash":credential_digest(credential),
                "scopes":scopes,
                "expires_at_unix_ms":client_now_ms()+600_000
            }),
        );
    }

    fn authenticated(state: &AppState, credential: &str) -> ClientSession {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/client/terminals/agent/shell/attachments")
            .header(AUTHORIZATION, format!("Bearer {credential}"))
            .body(Body::empty())
            .unwrap();
        authenticate(state, &request, "fabric-loopback").unwrap()
    }

    #[tokio::test]
    async fn same_key_pairings_keep_the_authenticated_grant() {
        let (_root, state, _) = fixture();
        pairing(
            &state,
            "custom/client/first",
            "first",
            json!(["terminal.read"]),
        );
        let first = authenticated(&state, "first");
        let epoch = authorization_epoch(&state, &first).unwrap();
        pairing(
            &state,
            "custom/client/second",
            "second",
            json!(["read.projections"]),
        );
        let first = authenticated(&state, "first");
        assert_eq!(authorization_epoch(&state, &first).unwrap(), epoch);
        assert!(
            revalidate_session(&state, &first)
                .unwrap()
                .allows("terminal.read")
        );
        let response = super::super::attachment(
            State(state.clone()),
            Extension(first.clone()),
            AxumPath("agent/shell".into()),
            Json(super::super::AttachmentRequest {
                runtime_incarnation: "incarnation-one".into(),
                mode: st3_client::RawTerminalMode::Peek,
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.0["mode"], "peek");
        claim(
            &state,
            "custom/client/second",
            "custom.client.pairing-revoked",
            json!({}),
        );
        assert_eq!(authorization_epoch(&state, &first).unwrap(), epoch);
        claim(
            &state,
            "custom/client/first",
            "custom.client.pairing-revoked",
            json!({}),
        );
        assert_eq!(
            authorization_epoch(&state, &first).unwrap_err().code,
            "forbidden"
        );
        assert!(revalidate_session(&state, &first).is_err());
    }

    #[test]
    fn same_key_pairings_do_not_borrow_another_issuer() {
        let (_root, issuer, _) = fixture();
        pairing(
            &issuer,
            "custom/client/issuer-first",
            "first",
            json!(["terminal.read"]),
        );
        let (_other_root, mut gateway, _) = fixture();
        gateway.store = Arc::new(Store::open_memory("other-member").unwrap());
        gateway.node = "other-member".into();
        gateway
            .store
            .import_replication("lease-owner", &issuer.store.export_replication(0).unwrap())
            .unwrap();
        pairing(
            &gateway,
            "custom/client/gateway-second",
            "second",
            json!(["terminal.read"]),
        );
        let first = authenticated(&gateway, "first");
        assert_eq!(
            authorization_epoch(&gateway, &first).unwrap_err().code,
            "forbidden"
        );
        let second = authenticated(&gateway, "second");
        authorization_epoch(&gateway, &second).unwrap();
    }

    #[tokio::test]
    async fn non_issuer_pairing_revoke_fails_without_committing() {
        let (_root, mut state, session) = fixture();
        let subject = "custom/client/issuer-only";
        claim(
            &state,
            subject,
            "custom.client.pairing-completed",
            json!({
                "device_id":"device/issuer-only",
                "person_id":"person/alex",
                "session_actor":"client/issuer-only",
                "scopes":["terminal.read"],
                "expires_at_unix_ms":client_now_ms()+600_000
            }),
        );
        // The claim keeps its issuer origin while the API runs on another member.
        state.node = "other-member".into();
        let snapshot = new_client_snapshot(&state);
        let request = ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: "action/issuer-only".into(),
            action_type: "pairing.revoke".into(),
            idempotency_key: "issuer-only".into(),
            fence: Fence {
                snapshot_id: snapshot.id.clone(),
                ..Default::default()
            },
            parameters: json!({"target_id":"device/issuer-only"}),
        };
        let error = dispatch_action(&state, &snapshot, &session, &request)
            .await
            .unwrap_err();
        assert_eq!(error.code, "issuer-required");
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(
            error.details.get("issuer_host_id"),
            Some(&json!("host/lease-owner"))
        );
        assert!(
            state
                .store
                .claims_for_subject_kind_at(subject, "custom.client.pairing-revoked", None, true, 1)
                .unwrap()
                .claims
                .is_empty()
        );
        state.node = "lease-owner".into();
        dispatch_action(&state, &new_client_snapshot(&state), &session, &request)
            .await
            .unwrap();
        assert_eq!(
            state
                .store
                .claims_for_subject_kind_at(subject, "custom.client.pairing-revoked", None, true, 1)
                .unwrap()
                .claims
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn principal_key_changes_move_the_authorization_epoch() {
        let (_root, state, session) = fixture();
        let acquired = authorization_epoch(&state, &session).unwrap();
        claim(
            &state,
            "person/alex",
            "principal.key-revoked",
            json!({"key":"acquired-key","reason":"race"}),
        );
        assert_ne!(authorization_epoch(&state, &session).unwrap(), acquired);
    }
}
