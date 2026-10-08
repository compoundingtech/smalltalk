//! Authorized native-subject reads. Operational Resources remain a separate contract.
use super::*;
use crate::store::{NativeSourceFence, NativeSourceRecord, native_source_log_order};
use st3_schema::client_projection::{self as disclosure, Audience, SpecialProjector};

const MAX_HEADS: usize = 64;
const SOURCE_PAGE: usize = 200;
const PAYLOAD_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub(super) struct PairingBinding {
    pub claim_id: String,
    pub subject: String,
    pub expires_at_unix_ms: u128,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
enum Provenance {
    Replicated {
        claim_id: String,
        origin: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        actor: Option<String>,
        accepted_at: String,
        store_index: u64,
    },
    Local {
        observation_id: String,
        node: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        actor: Option<String>,
        observed_at: String,
        after_store_index: u64,
        position: u64,
    },
}

#[derive(Clone, Debug, Serialize)]
struct SubjectClaim {
    id: String,
    #[serde(rename = "ref")]
    subject: String,
    kind: String,
    schema_id: String,
    retention: st3_schema::Retention,
    provenance: Provenance,
    payload_availability: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    fields: BTreeMap<String, Value>,
    omitted_fields: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct LocalFence {
    node: String,
    position: u64,
}

#[derive(Clone, Debug, Serialize)]
struct SubjectProjection {
    kind: &'static str,
    id: String,
    #[serde(rename = "ref")]
    subject: String,
    family: String,
    schema_id: String,
    heads: Vec<SubjectClaim>,
    heads_complete: bool,
    local_fence: LocalFence,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct SubjectsQuery {
    family: String,
    ref_prefix: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct SubjectQuery {
    #[serde(rename = "ref")]
    subject: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct HistoryQuery {
    #[serde(rename = "ref")]
    subject: String,
    kind: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Cursor {
    host: String,
    schema_id: String,
    session: String,
    visibility: String,
    collection: String,
    family: Option<String>,
    subject: Option<String>,
    ref_prefix: Option<String>,
    claim_kind: Option<String>,
    limit: usize,
    graph_index: u64,
    local_position: u64,
    after_ref: Option<String>,
    before: Option<(u64, u64)>,
    retention_version: Option<String>,
    expires_at_unix_ms: u128,
}

struct CursorBoundary<'a> {
    collection: &'a str,
    family: &'a str,
    subject: Option<&'a str>,
    prefix: Option<&'a str>,
    kind: Option<&'a str>,
    limit: usize,
}

fn visibility_key(session: &ClientSession) -> Result<String, ApiError> {
    let value = (&session.actor, &session.authority_actor, &session.scopes);
    Ok(hex::encode(Sha256::digest(
        serde_json::to_vec(&value).map_err(ApiError::internal)?,
    )))
}

fn cursor_key() -> Result<&'static ring::hmac::Key, ApiError> {
    static KEY: std::sync::LazyLock<Result<ring::hmac::Key, String>> =
        std::sync::LazyLock::new(|| {
            let mut bytes = [0_u8; 32];
            getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
            Ok(ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &bytes))
        });
    KEY.as_ref()
        .map_err(|error| ApiError::internal(anyhow::anyhow!(error.clone())))
}

fn decode_cursor(value: &str) -> Result<Cursor, ApiError> {
    let malformed =
        || client_page_expired("the native subject cursor is invalid or belongs to another daemon");
    let encoded = value.strip_prefix("subject-page/").ok_or_else(malformed)?;
    let (body, signature) = encoded.split_once('.').ok_or_else(malformed)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body)
        .map_err(|_| malformed())?;
    let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| malformed())?;
    ring::hmac::verify(cursor_key()?, &bytes, &signature).map_err(|_| malformed())?;
    serde_json::from_slice(&bytes).map_err(|_| malformed())
}

fn encode_cursor(cursor: &Cursor) -> Result<String, ApiError> {
    let bytes = serde_json::to_vec(cursor).map_err(ApiError::internal)?;
    let signature = ring::hmac::sign(cursor_key()?, &bytes);
    Ok(format!(
        "subject-page/{}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes),
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.as_ref())
    ))
}

fn family_id(family: &str) -> Result<String, ApiError> {
    disclosure::family_schema_id(family).ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "unsupported-capability",
            "the native subject family is not registered",
        ))
    })
}

fn validate_ref(subject: &str) -> Result<String, ApiError> {
    if subject == "custom/client" || subject.starts_with("custom/client/") {
        return Err(forbidden(
            "the internal client namespace is not a native client read",
        ));
    }
    let family = st3_schema::registry()
        .validate_subject(subject)
        .map_err(|error| validation(error.message))?
        .family
        .clone();
    if !disclosure::family_ref_allowed(&family, subject) {
        return Err(forbidden("the native subject family is not a client read"));
    }
    Ok(family)
}

pub(super) fn expiry_delay(session: &ClientSession) -> Duration {
    let millis = session
        .pairing_binding
        .as_ref()
        .map_or(u128::from(u64::MAX), |binding| {
            binding.expires_at_unix_ms.saturating_sub(client_now_ms())
        });
    Duration::from_millis(millis.min(u128::from(u64::MAX)) as u64)
}

/// A copied scope set is not authority for a long-lived native subscription.
pub(super) fn revalidate_session(
    state: &AppState,
    session: &ClientSession,
) -> Result<(), ApiError> {
    require_scope(session, "read.projections")?;
    let Some(binding) = &session.pairing_binding else {
        return if session.transport == "unix" {
            Ok(())
        } else {
            Err(forbidden(
                "the native subject reader requires a current paired binding",
            ))
        };
    };
    if binding.expires_at_unix_ms <= client_now_ms() {
        return Err(forbidden("the paired native subject reader expired"));
    }
    let store = &state.store;
    let paired = store
        .native_admitted_claim_by_id(&binding.claim_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| forbidden("the paired native subject reader is no longer admitted"))?;
    let fields = paired
        .body
        .get("fields")
        .and_then(Value::as_object)
        .ok_or_else(|| forbidden("the paired native subject binding is invalid"))?;
    let current_scopes = fields
        .get("scopes")
        .and_then(Value::as_array)
        .ok_or_else(|| forbidden("the paired native subject scopes are invalid"))?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if paired.subject != binding.subject
        || paired.kind != "custom.client.pairing-completed"
        || fields.get("session_actor").and_then(Value::as_str) != Some(session.actor.as_str())
        || fields.get("person_id").and_then(Value::as_str) != Some(session.authority_actor.as_str())
        || fields
            .get("expires_at_unix_ms")
            .and_then(Value::as_u64)
            .map(u128::from)
            != Some(binding.expires_at_unix_ms)
        || current_scopes != session.scopes
    {
        return Err(forbidden("the paired native subject authority changed"));
    }
    let fence = store.native_source_fence().map_err(ApiError::internal)?;
    let latest = store
        .native_subject_history(
            &binding.subject,
            Some("custom.client.pairing-completed"),
            None,
            &fence,
            1,
        )
        .map_err(ApiError::internal)?;
    let revoked = store
        .native_subject_history(
            &binding.subject,
            Some("custom.client.pairing-revoked"),
            None,
            &fence,
            1,
        )
        .map_err(ApiError::internal)?;
    if latest
        .first()
        .is_none_or(|record| record.record.id != binding.claim_id)
        || revoked
            .first()
            .is_some_and(|record| record.record.store_index > paired.store_index)
    {
        return Err(forbidden(
            "the paired native subject reader was replaced or revoked",
        ));
    }
    Ok(())
}

struct Reader<'a> {
    state: &'a AppState,
    session: &'a ClientSession,
    fence: NativeSourceFence,
}

impl Reader<'_> {
    fn party(&self, audience: &str) -> bool {
        !audience.is_empty() && audience == self.session.authority_actor
    }

    fn fields(&self, record: &ClaimRecord) -> Result<BTreeMap<String, Value>, ApiError> {
        crate::store::normalize_native_claim_fields(&record.kind, &record.body)
            .map_err(ApiError::internal)
    }

    fn preceding_request(
        &self,
        record: &ClaimRecord,
        kind: &str,
        field: &str,
    ) -> Result<Option<ClaimRecord>, ApiError> {
        let fields = self.fields(record)?;
        let explicit_id = if kind == "planning-session.question-requested" {
            None
        } else {
            fields.get(field).and_then(Value::as_str)
        };
        let episode = match kind {
            "work.person-asked" if explicit_id.is_none() => fields
                .get("key")
                .zip(fields.get("attempt"))
                .map(|(key, attempt)| (key, attempt, "key", "attempt")),
            "planning-session.question-requested" => fields
                .get("decision_id")
                .filter(|value| value.as_str().is_some())
                .zip(
                    fields
                        .get("expected_revision")
                        .filter(|value| value.as_u64().is_some()),
                )
                .map(|(decision, revision)| (decision, revision, "decision_id", "revision")),
            _ => None,
        };
        if explicit_id.is_none() && episode.is_none() && kind != "message.sent" {
            return Ok(None);
        }
        // Durable episodes precede answers in replicated canonical order, even
        // when replication admits their requests later on this host. Local-only
        // observations retain the answering host's source-log boundary.
        let local = crate::store::local_observation_position(record);
        let mut before = Some((record.store_index, local.unwrap_or(0)));
        let mut before_id = record.id.clone();
        loop {
            let candidates = if local.is_some() {
                self.state.store.native_subject_history(
                    &record.subject,
                    Some(kind),
                    before,
                    &self.fence,
                    SOURCE_PAGE,
                )
            } else {
                self.state.store.native_preceding_kind_heads(
                    &record.subject,
                    kind,
                    &before_id,
                    &self.fence,
                    SOURCE_PAGE,
                )
            }
            .map_err(ApiError::internal)?;
            if candidates.is_empty() {
                return Ok(None);
            }
            let next = candidates.last().map(native_source_log_order);
            let next_id = candidates.last().map(|source| source.record.id.clone());
            for candidate in candidates {
                if let Some(id) = explicit_id {
                    if candidate.record.id == id {
                        return Ok(Some(candidate.record));
                    }
                } else if let Some((key, revision, key_field, revision_field)) = episode {
                    let request_fields = self.fields(&candidate.record)?;
                    if request_fields.get(key_field) == Some(key)
                        && request_fields.get(revision_field) == Some(revision)
                    {
                        return Ok(Some(candidate.record));
                    }
                } else if kind == "message.sent" {
                    return Ok(Some(candidate.record));
                }
            }
            before = next;
            if let Some(id) = next_id {
                before_id = id;
            }
        }
    }

    fn identity_request(&self, subject: &str, kind: &str) -> Result<Option<ClaimRecord>, ApiError> {
        let current = self
            .state
            .store
            .native_source_fence()
            .map_err(ApiError::internal)?;
        Ok(self
            .state
            .store
            .native_subject_replicated_kind_heads(subject, kind, None, &current, 1)
            .map_err(ApiError::internal)?
            .into_iter()
            .next()
            .map(|source| source.record))
    }

    fn message_audience(&self, record: &ClaimRecord) -> Result<bool, ApiError> {
        let mut fields = self.fields(record)?;
        let mut before = (
            record.store_index,
            crate::store::local_observation_position(record).unwrap_or(0),
        );
        let mut before_id = record.id.clone();
        let local = crate::store::local_observation_position(record).is_some();
        // Match the existing conversation/attachment audience, including bounded
        // ancestry, but never let a repaired parent grant visibility.
        for _ in 0..16 {
            if ["from", "to"].iter().any(|name| {
                fields
                    .get(*name)
                    .and_then(Value::as_str)
                    .is_some_and(|party| self.party(party) || party.starts_with("agent/"))
            }) {
                return Ok(true);
            }
            let Some(parent) = fields.get("in_reply_to").and_then(Value::as_str) else {
                return Ok(false);
            };
            let subject = if parent.starts_with("message/") {
                parent.to_owned()
            } else {
                format!("message/{parent}")
            };
            let parent = if local {
                self.state.store.native_subject_history(
                    &subject,
                    Some("message.sent"),
                    Some(before),
                    &self.fence,
                    1,
                )
            } else {
                self.state.store.native_preceding_kind_heads(
                    &subject,
                    "message.sent",
                    &before_id,
                    &self.fence,
                    1,
                )
            }
            .map_err(ApiError::internal)?
            .into_iter()
            .next();
            let Some(parent) = parent else {
                return Ok(false);
            };
            before = native_source_log_order(&parent);
            before_id = parent.record.id.clone();
            fields = self.fields(&parent.record)?;
        }
        Ok(false)
    }

    fn identity_visible(&self, subject: &str, family: &str) -> Result<bool, ApiError> {
        let policy = disclosure::family_policy(family)
            .ok_or_else(|| validation("the native subject family has no disclosure policy"))?;
        match policy.identity {
            Audience::Denied => Ok(false),
            Audience::Person => Ok(self.party(subject)),
            Audience::Account => {
                Ok(acting_party(self.session) && self.session.allows("read.declarations"))
            }
            Audience::Glass => {
                let person = glass_person(self.session, false)?;
                let owner = st3_schema::glasses::owner(subject)
                    .map_err(|error| validation(error.message))?;
                if owner != person {
                    return Err(forbidden("the glass belongs to another person"));
                }
                let current = self
                    .state
                    .store
                    .native_source_fence()
                    .map_err(ApiError::internal)?;
                Ok(self
                    .state
                    .store
                    .glasses(&person, current.graph_index)
                    .map_err(ApiError::internal)?
                    .iter()
                    .any(|glass| glass["id"] == subject))
            }
            Audience::Message => {
                let Some(request) = self.identity_request(subject, "message.sent")? else {
                    return Ok(false);
                };
                self.message_audience(&request)
            }
            Audience::Attention => {
                let Some(request) = self.identity_request(subject, "attention.requested")? else {
                    return Ok(false);
                };
                Ok(self
                    .fields(&request)?
                    .get("reviewer")
                    .and_then(Value::as_str)
                    .is_some_and(|reviewer| self.party(reviewer)))
            }
            Audience::RecordedActor | Audience::Shared => Ok(true),
            _ => Ok(false),
        }
    }

    fn claim_visible(&self, record: &ClaimRecord) -> Result<bool, ApiError> {
        let Ok(spec) = st3_schema::registry().validate_subject(&record.subject) else {
            return Ok(false);
        };
        if !disclosure::family_ref_allowed(&spec.family, &record.subject)
            || record.kind == "custom.client"
            || record.kind.starts_with("custom.client.")
        {
            return Ok(false);
        }
        let Some(policy) = disclosure::claim_policy(&record.kind) else {
            return Ok(false);
        };
        let fields = self.fields(record)?;
        let field_party = |name: &str| {
            fields
                .get(name)
                .and_then(Value::as_str)
                .is_some_and(|party| self.party(party))
        };
        // Wildcard/shared claim kinds do not erase a private family's audience.
        let audience = if policy.audience == Audience::Shared {
            disclosure::family_policy(&spec.family)
                .map_or(Audience::Denied, |family| family.identity)
        } else {
            policy.audience
        };
        match audience {
            Audience::Denied => Ok(false),
            Audience::Shared => Ok(true),
            Audience::RecordedActor => {
                Ok(record.actor.as_deref() == Some(self.session.authority_actor.as_str()))
            }
            Audience::Person => Ok(self.party(&record.subject)),
            Audience::Account => {
                Ok(acting_party(self.session) && self.session.allows("read.declarations"))
            }
            Audience::Glass => self.identity_visible(&record.subject, "glass"),
            Audience::Message => {
                if record.kind == "message.sent" {
                    return self.message_audience(record);
                }
                let Some(request) = self.preceding_request(record, "message.sent", "")? else {
                    return Ok(false);
                };
                self.message_audience(&request)
            }
            Audience::Attention => {
                if record.kind == "attention.requested" {
                    return Ok(field_party("reviewer"));
                }
                Ok(self
                    .preceding_request(record, "attention.requested", "request")?
                    .map(|request| self.fields(&request))
                    .transpose()?
                    .is_some_and(|fields| {
                        fields
                            .get("reviewer")
                            .and_then(Value::as_str)
                            .is_some_and(|party| self.party(party))
                    }))
            }
            Audience::PersonRequest => Ok(field_party(
                if record.kind == "planning-session.question-requested" {
                    "requester"
                } else {
                    "person"
                },
            )),
            Audience::PersonAnswer => {
                let planning = record.kind == "planning-session.question-answered";
                let kind = if planning {
                    "planning-session.question-requested"
                } else {
                    "work.person-asked"
                };
                let audience = if planning { "requester" } else { "person" };
                Ok(self
                    .preceding_request(record, kind, "episode")?
                    .map(|request| self.fields(&request))
                    .transpose()?
                    .is_some_and(|fields| {
                        fields
                            .get(audience)
                            .and_then(Value::as_str)
                            .is_some_and(|party| self.party(party))
                    }))
            }
            Audience::ReviewerRequest => Ok(field_party("reviewer")),
            Audience::ReviewerResult => {
                if record.kind == "revision-proposal.approved" {
                    return Ok(field_party("reviewer"));
                }
                Ok(self
                    .preceding_request(record, "gate.requested", "request")?
                    .map(|request| self.fields(&request))
                    .transpose()?
                    .is_some_and(|fields| {
                        fields
                            .get("reviewer")
                            .and_then(Value::as_str)
                            .is_some_and(|party| self.party(party))
                    }))
            }
            Audience::OperationalAudience => {
                if record.kind == "operational.recovered" {
                    return Ok(self
                        .preceding_request(record, "operational.failure", "failure")?
                        .map(|failure| self.claim_visible(&failure))
                        .transpose()?
                        .unwrap_or(false));
                }
                Ok(field_party("reviewer"))
            }
        }
    }

    fn project(
        &self,
        source: NativeSourceRecord,
        family: &str,
        history: bool,
    ) -> Result<SubjectClaim, ApiError> {
        let record = source.record;
        let policy = disclosure::claim_policy(&record.kind)
            .ok_or_else(|| validation("the claim has no native disclosure policy"))?;
        let native = self.fields(&record)?;
        let family_policy = disclosure::family_policy(family)
            .ok_or_else(|| validation("the native subject family has no disclosure policy"))?;
        let special = if record.kind == "intent.desired" {
            family_policy.special.iter().copied().find(|projector| {
                matches!(
                    projector,
                    SpecialProjector::AgentDesired | SpecialProjector::AccountDesired
                )
            })
        } else {
            policy.special
        };
        let mut fields = policy
            .fields
            .iter()
            .filter(|name| disclosure::disclosed_field(&record.kind, name).is_some())
            .filter_map(|name| {
                native
                    .get(*name)
                    .map(|value| ((*name).to_owned(), value.clone()))
            })
            .collect::<BTreeMap<_, _>>();
        let mut invalid_payload = false;
        match special {
            Some(SpecialProjector::CustomFields) => {
                fields = native;
                if record.body.get("fields").is_none() {
                    fields.remove("_operation");
                }
            }
            Some(SpecialProjector::GlassBody) => {
                if let Some(body) = native.get("body") {
                    fields.insert(
                        "body".into(),
                        st3_schema::glasses::body_for_read(body)
                            .map_err(|error| validation(error.message))?,
                    );
                }
            }
            Some(SpecialProjector::AgentDesired) if family == "agent" => {
                if (!history || self.session.allows("read.declarations"))
                    && let Some(desired) = native.get("desired")
                {
                    if desired.get("stop").and_then(Value::as_str) == Some(record.subject.as_str())
                    {
                        // A native stop is not a family declaration AST. Retain its
                        // kind/revision/provenance without disclosing a declaration bag.
                    } else if desired.is_null() {
                        fields.insert("desired".into(), Value::Null);
                    } else {
                        let mut desired = canonical_desired(desired, "agent")?;
                        crate::graph::redact_agent_env_values(&mut desired);
                        fields.insert("desired".into(), desired);
                    }
                }
            }
            Some(SpecialProjector::AccountDesired)
                if family == "account"
                    && acting_party(self.session)
                    && self.session.allows("read.declarations") =>
            {
                if let Some(desired) = native.get("desired") {
                    if desired.get("stop").and_then(Value::as_str) == Some(record.subject.as_str())
                    {
                        // Stop declarations do not contain an account declaration.
                    } else if desired.is_null() {
                        fields.insert("desired".into(), Value::Null);
                    } else if let Some(account) = crate::accounts::parse_account(
                        &record.subject,
                        &canonical_desired(desired, "account")?,
                    ) {
                        let node = |name: &str, value: &str| json!({"name":name,"arguments":[value],"properties":{},"children":[]});
                        let mut children = vec![node("provider", &account.provider)];
                        if let Some(owner) = &account.owner {
                            children.push(node("owner", owner));
                        }
                        if let Some(plan) = &account.plan {
                            children.push(node("plan", plan));
                        }
                        for login in &account.logins {
                            children.push(json!({"name":"login","arguments":[login.path],"properties":login.host.as_ref().map_or_else(|| json!({}), |host| json!({"host":host})),"children":[]}));
                        }
                        fields.insert("desired".into(), json!({"name":"account","arguments":[account.name],"properties":{},"children":children}));
                    }
                }
            }
            Some(SpecialProjector::ResourceObservation) => {
                // Legacy state/extra bags never establish disclosure. Only the
                // canonical nested facts intersect the reviewed native resource.
                fields.remove("state");
                if let Some(kind) = native.get("kind").and_then(Value::as_str)
                    && let Some(selection) = disclosure::resource_field_policy(kind)
                    && let Some(spec) = st3_schema::registry().resource(kind)
                    && let Some(facts) = record.body.pointer("/fields/facts")
                {
                    if let Some(facts) = facts.as_object() {
                        let safe = selection
                            .iter()
                            .filter(|name| spec.fields.contains_key(**name))
                            .filter_map(|name| {
                                facts
                                    .get(*name)
                                    .map(|value| ((*name).to_owned(), value.clone()))
                            })
                            .collect::<BTreeMap<_, _>>();
                        if st3_schema::registry()
                            .validate_resource_facts(kind, &safe)
                            .is_err()
                        {
                            invalid_payload = true;
                        } else {
                            fields.insert(
                                "facts".into(),
                                serde_json::to_value(safe).map_err(ApiError::internal)?,
                            );
                        }
                    } else if facts.is_null() {
                        fields.insert("facts".into(), Value::Null);
                    }
                }
            }
            _ => {}
        }
        let spec = st3_schema::registry().claim(&record.kind);
        let omitted_fields = spec
            .into_iter()
            .flat_map(|spec| spec.fields.keys())
            .filter(|name| {
                !policy.fields.contains(&name.as_str())
                    && !matches!(
                        (special, name.as_str()),
                        (Some(SpecialProjector::GlassBody), "body")
                            | (
                                Some(
                                    SpecialProjector::AgentDesired
                                        | SpecialProjector::AccountDesired
                                ),
                                "desired"
                            )
                    )
                    || (record.kind == "resource.observed" && name.as_str() == "state")
                    || (history
                        && !self.session.allows("read.declarations")
                        && special == Some(SpecialProjector::AgentDesired)
                        && name.as_str() == "desired")
            })
            .cloned()
            .collect::<Vec<_>>();
        let retention = spec.map_or(st3_schema::Retention::Durable, |spec| spec.retention);
        let mut availability = if fields.is_empty() && !omitted_fields.is_empty() {
            "withheld"
        } else {
            "available"
        };
        let mut reason = None;
        if invalid_payload {
            fields.clear();
            availability = "unavailable";
            reason = Some("invalid-native-fields");
        } else if !fields.values().all(json_safe) {
            fields.clear();
            availability = "unavailable";
            reason = Some("unsafe-json-number");
        } else if serde_json::to_vec(&fields)
            .map_err(ApiError::internal)?
            .len()
            > PAYLOAD_BYTES
        {
            fields.clear();
            availability = "unavailable";
            reason = Some("payload-size");
        }
        let provenance = source.local_position.map_or_else(
            || Provenance::Replicated {
                claim_id: record.id.clone(),
                origin: record.origin.clone(),
                actor: record.actor.clone(),
                accepted_at: client_timestamp(record.accepted_at_unix_ms),
                store_index: record.store_index,
            },
            |position| Provenance::Local {
                observation_id: record.id.clone(),
                node: self.state.node.clone(),
                actor: record.actor.clone(),
                observed_at: client_timestamp(record.accepted_at_unix_ms),
                after_store_index: record.store_index,
                position,
            },
        );
        let schema_id = disclosure::claim_schema_id(family, &record.kind)
            .ok_or_else(|| validation("the claim has no native projected schema"))?;
        Ok(SubjectClaim {
            id: record.id,
            subject: record.subject,
            kind: record.kind,
            schema_id,
            retention,
            provenance,
            payload_availability: availability,
            reason,
            fields,
            omitted_fields,
        })
    }
}

/// Rebuild only canonical AST keys. Arbitrary attached objects are not AST
/// values, and env entries must be leaves before the existing redactor runs.
fn canonical_desired(value: &Value, root: &str) -> Result<Value, ApiError> {
    fn node(value: &Value) -> Result<Value, ApiError> {
        let invalid = || validation("the native desired AST is not canonical");
        let object = value.as_object().ok_or_else(invalid)?;
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        // Native KDL serialization omits empty collections; present collections must
        // still have their canonical types before normalization and redaction.
        let empty_properties = serde_json::Map::new();
        let arguments = match object.get("arguments") {
            None => &[][..],
            Some(value) => value.as_array().ok_or_else(invalid)?.as_slice(),
        };
        let properties = match object.get("properties") {
            None => &empty_properties,
            Some(value) => value.as_object().ok_or_else(invalid)?,
        };
        let children = match object.get("children") {
            None => &[][..],
            Some(value) => value.as_array().ok_or_else(invalid)?.as_slice(),
        };
        let scalar = |value: &Value| !value.is_array() && !value.is_object();
        if !arguments.iter().all(scalar) || !properties.values().all(scalar) {
            return Err(invalid());
        }
        let children = children.iter().map(node).collect::<Result<Vec<_>, _>>()?;
        if name == "env"
            && (!arguments.is_empty()
                || !properties.is_empty()
                || children.iter().any(|child| {
                    child["arguments"]
                        .as_array()
                        .is_none_or(|args| args.len() != 1 || !args[0].is_string())
                        || child["properties"]
                            .as_object()
                            .is_none_or(|props| !props.is_empty())
                        || child["children"]
                            .as_array()
                            .is_none_or(|children| !children.is_empty())
                }))
        {
            return Err(invalid());
        }
        Ok(
            json!({"name": name, "arguments": arguments, "properties": properties, "children": children}),
        )
    }
    if value.get("name").and_then(Value::as_str) != Some(root) {
        return Err(validation("the native desired AST has the wrong family"));
    }
    node(value)
}

fn json_safe(value: &Value) -> bool {
    match value {
        Value::Number(number) => {
            number
                .as_i64()
                .is_none_or(|value| value.unsigned_abs() <= 9_007_199_254_740_991)
                && number
                    .as_u64()
                    .is_none_or(|value| value <= 9_007_199_254_740_991)
                && number.as_f64().is_some_and(|value| {
                    value.is_finite()
                        && (value.fract() != 0.0 || value.abs() <= 9_007_199_254_740_991.0)
                })
        }
        Value::Array(values) => values.iter().all(json_safe),
        Value::Object(values) => values.values().all(json_safe),
        _ => true,
    }
}

type NativeResponse = (Extension<ClientSnapshot>, Json<Value>);

async fn read_native<T: Send + 'static>(
    state: AppState,
    session: ClientSession,
    read_permit: Option<tokio::sync::OwnedSemaphorePermit>,
    operation: impl FnOnce(&Reader<'_>) -> Result<T, ApiError> + Send + 'static,
) -> Result<(ClientSnapshot, T), ApiError> {
    blocking_store(move || {
        // A canceled awaiter must not release a slot while its SQLite worker still runs.
        let _read_permit = read_permit;
        state
            .store
            .read_snapshot(|index| {
                Ok((|| {
                    revalidate_session(&state, &session)?;
                    let fence = state
                        .store
                        .native_source_fence()
                        .map_err(ApiError::internal)?;
                    let reader = Reader {
                        state: &state,
                        session: &session,
                        fence,
                    };
                    Ok((client_snapshot_at(&state, index), operation(&reader)?))
                })())
            })
    })
    .await?
}

impl Reader<'_> {
    fn selected_head(
        &self,
        subject: &str,
        kind: &str,
        local: bool,
    ) -> Result<Option<NativeSourceRecord>, ApiError> {
        let mut before = None;
        loop {
            let records = if local {
                self.state.store.native_subject_local_kind_heads(
                    subject,
                    kind,
                    before.as_deref(),
                    &self.fence,
                    SOURCE_PAGE,
                )
            } else {
                self.state.store.native_subject_replicated_kind_heads(
                    subject,
                    kind,
                    before.as_deref(),
                    &self.fence,
                    SOURCE_PAGE,
                )
            }
            .map_err(ApiError::internal)?;
            let Some(last) = records.last() else {
                return Ok(None);
            };
            let next = last.record.id.clone();
            for source in records {
                if self.claim_visible(&source.record)? {
                    return Ok(Some(source));
                }
            }
            before = Some(next);
        }
    }

    fn subject(&self, subject: &str, family: &str) -> Result<Option<SubjectProjection>, ApiError> {
        if !self.identity_visible(subject, family)? {
            return Ok(None);
        }
        let descriptor = disclosure::family_descriptor(family)
            .ok_or_else(|| validation("the native subject family has no descriptor"))?;
        let mut kinds = descriptor["claims"]
            .as_object()
            .into_iter()
            .flat_map(|claims| claims.keys())
            .filter(|kind| kind.as_str() != "custom.*")
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut complete = true;
        if family == "custom" {
            let mut before = None;
            loop {
                let records = self
                    .state
                    .store
                    .native_subject_history(subject, None, before, &self.fence, SOURCE_PAGE)
                    .map_err(ApiError::internal)?;
                let Some(last) = records.last() else {
                    break;
                };
                let next = native_source_log_order(last);
                for source in records {
                    if self.claim_visible(&source.record)? {
                        kinds.insert(source.record.kind);
                    }
                    if kinds.len() > MAX_HEADS {
                        complete = false;
                        break;
                    }
                }
                if !complete {
                    break;
                }
                before = Some(next);
            }
        }
        let mut heads = Vec::new();
        let mut bytes = 0;
        for kind in kinds {
            for local in [false, true] {
                let Some(source) = self.selected_head(subject, &kind, local)? else {
                    continue;
                };
                let head = self.project(source, family, false)?;
                let size = serde_json::to_vec(&head).map_err(ApiError::internal)?.len();
                if heads.len() == MAX_HEADS || bytes + size > 128 * 1024 {
                    complete = false;
                    continue;
                }
                bytes += size;
                heads.push(head);
            }
        }
        if heads.is_empty() {
            return Ok(None);
        }
        Ok(Some(SubjectProjection {
            kind: "subject",
            id: subject.to_owned(),
            subject: subject.to_owned(),
            family: family.to_owned(),
            schema_id: family_id(family)?,
            heads,
            heads_complete: complete,
            local_fence: LocalFence {
                node: self.state.node.clone(),
                position: self.fence.local_position,
            },
        }))
    }

    fn retention_version(
        &self,
        family: &str,
        subject: Option<&str>,
        prefix: Option<&str>,
        kind: Option<&str>,
        fence: &NativeSourceFence,
    ) -> Result<String, ApiError> {
        match subject {
            Some(subject) => self
                .state
                .store
                .native_subject_retention_version(subject, kind, fence),
            None => self.state.store.native_family_retention_version(
                family,
                prefix,
                fence,
                (family == "custom").then_some(self.session.authority_actor.as_str()),
            ),
        }
        .map_err(ApiError::internal)
    }

    fn cursor(
        &self,
        encoded: Option<&str>,
        boundary: CursorBoundary<'_>,
    ) -> Result<Cursor, ApiError> {
        let CursorBoundary {
            collection,
            family,
            subject,
            prefix,
            kind,
            limit,
        } = boundary;
        let visibility = visibility_key(self.session)?;
        if let Some(encoded) = encoded {
            let cursor = decode_cursor(encoded)?;
            if cursor.host != self.state.node
                || cursor.schema_id != family_id(family)?
                || cursor.session != self.session.actor
                || cursor.visibility != visibility
                || cursor.collection != collection
                || cursor.family.as_deref() != Some(family)
                || cursor.subject.as_deref() != subject
                || cursor.ref_prefix.as_deref() != prefix
                || cursor.claim_kind.as_deref() != kind
                || cursor.limit != limit
                || cursor.graph_index > self.fence.graph_index
                || cursor.local_position > self.fence.local_position
                || client_now_ms() >= cursor.expires_at_unix_ms
                || cursor.expires_at_unix_ms > client_now_ms().saturating_add(CLIENT_PAGE_TTL_MS)
            {
                return Err(client_page_expired(
                    "the native subject cursor expired or its boundary changed",
                ));
            }
            let fence = NativeSourceFence {
                graph_index: cursor.graph_index,
                local_position: cursor.local_position,
            };
            let current = self.retention_version(family, subject, prefix, kind, &fence)?;
            if cursor.retention_version.as_deref() != Some(current.as_str()) {
                return Err(client_page_expired(
                    "the retained native sources changed; restart pagination",
                ));
            }
            if let (Some(subject), Some(position)) = (subject, cursor.before)
                && !self
                    .state
                    .store
                    .native_source_record_exists(subject, position)
                    .map_err(ApiError::internal)?
            {
                return Err(client_page_expired(
                    "the retained native history cursor is no longer available",
                ));
            }
            return Ok(cursor);
        }
        Ok(Cursor {
            host: self.state.node.clone(),
            schema_id: family_id(family)?,
            session: self.session.actor.clone(),
            visibility,
            collection: collection.into(),
            family: Some(family.into()),
            subject: subject.map(str::to_owned),
            ref_prefix: prefix.map(str::to_owned),
            claim_kind: kind.map(str::to_owned),
            limit,
            graph_index: self.fence.graph_index,
            local_position: self.fence.local_position,
            after_ref: None,
            before: None,
            retention_version: Some(self.retention_version(
                family,
                subject,
                prefix,
                kind,
                &self.fence,
            )?),
            expires_at_unix_ms: client_now_ms().saturating_add(CLIENT_PAGE_TTL_MS),
        })
    }

    fn page(
        &self,
        cursor: Cursor,
        filters: BTreeMap<String, String>,
        items: Vec<Value>,
        more: bool,
    ) -> Result<Value, ApiError> {
        let mut page = json!({
            "kind":"page", "collection":cursor.collection, "filters":filters, "items":items,
            "page": {
                "limit":cursor.limit, "has_more":more,
                "next_cursor":if more { Some(encode_cursor(&cursor)?) } else { None },
                "cursor_expires_at":if more { Some(client_timestamp(cursor.expires_at_unix_ms)) } else { None },
            },
            "local_fence":{"node":self.state.node,"position":cursor.local_position},
        });
        if let Some(sync) = client_sync_notice(self.state) {
            page["sync"] = serde_json::to_value(sync).map_err(ApiError::internal)?;
        }
        if cursor.subject.is_some() {
            let retention = self.state.store.native_retention_coverage();
            page["coverage"] = json!({
                "source":"answering-host-retained", "replicated_through":cursor.graph_index,
                "local_through":cursor.local_position, "earlier_history":"not-guaranteed",
                "local_retention":retention["local_retention"],
                "replicated_retention":retention["replicated_retention"],
                "projection_limits": {
                    "max_heads":MAX_HEADS, "max_claim_payload_bytes":PAYLOAD_BYTES,
                    "max_response_bytes":CLIENT_MAX_RESPONSE_BYTES,
                },
            });
        }
        Ok(page)
    }

    fn subjects_page(&self, query: &SubjectsQuery) -> Result<Value, ApiError> {
        family_id(&query.family)?;
        let glass_owner = (query.family == "glass")
            .then(|| glass_person(self.session, false))
            .transpose()?;
        if query.ref_prefix.as_ref().is_some_and(|prefix| {
            !prefix.starts_with(&format!("{}/", query.family)) || prefix.len() > 512
        }) {
            return Err(validation(
                "ref_prefix must remain inside the selected native family",
            ));
        }
        let limit = query
            .limit
            .unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS)
            .clamp(1, CLIENT_MAX_PAGE_ITEMS);
        let mut cursor = self.cursor(
            query.cursor.as_deref(),
            CursorBoundary {
                collection: "subjects",
                family: &query.family,
                subject: None,
                prefix: query.ref_prefix.as_deref(),
                kind: None,
                limit,
            },
        )?;
        let reader = Reader {
            state: self.state,
            session: self.session,
            fence: NativeSourceFence {
                graph_index: cursor.graph_index,
                local_position: cursor.local_position,
            },
        };
        let mut items = Vec::new();
        let mut used = 0;
        let mut more = false;
        let mut after = cursor.after_ref.clone();
        'pages: loop {
            let references = self
                .state
                .store
                .native_subject_refs(
                    &query.family,
                    query.ref_prefix.as_deref(),
                    after.as_deref(),
                    &reader.fence,
                    (query.family == "custom").then_some(self.session.authority_actor.as_str()),
                    SOURCE_PAGE,
                )
                .map_err(ApiError::internal)?;
            if references.is_empty() {
                break;
            }
            let full = references.len() == SOURCE_PAGE;
            for reference in references {
                if let Some(person) = glass_owner.as_deref()
                    && st3_schema::glasses::owner(&reference)
                        .map_err(|error| validation(error.message))?
                        != person
                {
                    after = Some(reference);
                    continue;
                }
                if !disclosure::family_ref_allowed(&query.family, &reference) {
                    after = Some(reference);
                    continue;
                }
                if let Some(subject) = reader.subject(&reference, &query.family)? {
                    let value = serde_json::to_value(subject).map_err(ApiError::internal)?;
                    let size = serde_json::to_vec(&value)
                        .map_err(ApiError::internal)?
                        .len();
                    if items.len() == limit || used + size > CLIENT_MAX_RESPONSE_BYTES - 128_000 {
                        more = true;
                        break 'pages;
                    }
                    used += size;
                    items.push(value);
                    // Public continuation boundaries may name only a returned,
                    // authorized identity, never an internal hidden scan candidate.
                    cursor.after_ref = Some(reference.clone());
                }
                after = Some(reference);
            }
            if !full {
                break;
            }
        }
        let mut filters = BTreeMap::from([("family".into(), query.family.clone())]);
        if let Some(prefix) = &query.ref_prefix {
            filters.insert("ref_prefix".into(), prefix.clone());
        }
        self.page(cursor, filters, items, more)
    }

    fn history_page(&self, query: &HistoryQuery, collection: &str) -> Result<Value, ApiError> {
        let family = validate_ref(&query.subject)?;
        if !self.identity_visible(&query.subject, &family)? {
            return Err(ApiError::not_found("the native subject is not available"));
        }
        if let Some(kind) = &query.kind
            && disclosure::claim_schema_id(&family, kind).is_none()
        {
            return Err(validation(
                "the selected claim kind is not registered for this family",
            ));
        }
        let limit = query
            .limit
            .unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS)
            .clamp(1, CLIENT_MAX_PAGE_ITEMS);
        let mut cursor = self.cursor(
            query.cursor.as_deref(),
            CursorBoundary {
                collection,
                family: &family,
                subject: Some(&query.subject),
                prefix: None,
                kind: query.kind.as_deref(),
                limit,
            },
        )?;
        let reader = Reader {
            state: self.state,
            session: self.session,
            fence: NativeSourceFence {
                graph_index: cursor.graph_index,
                local_position: cursor.local_position,
            },
        };
        // Hidden or missing refs have exactly the same public result.
        if reader.subject(&query.subject, &family)?.is_none() {
            return Err(ApiError::not_found("the native subject is not available"));
        }
        let mut before = cursor.before;
        let mut items = Vec::new();
        let mut used = 0;
        let mut more = false;
        'pages: loop {
            let records = self
                .state
                .store
                .native_subject_history(
                    &query.subject,
                    query.kind.as_deref(),
                    before,
                    &reader.fence,
                    SOURCE_PAGE,
                )
                .map_err(ApiError::internal)?;
            if records.is_empty() {
                break;
            }
            let full = records.len() == SOURCE_PAGE;
            for source in records {
                let position = native_source_log_order(&source);
                if reader.claim_visible(&source.record)? {
                    let value = serde_json::to_value(reader.project(source, &family, true)?)
                        .map_err(ApiError::internal)?;
                    let size = serde_json::to_vec(&value)
                        .map_err(ApiError::internal)?
                        .len();
                    if items.len() == limit || used + size > CLIENT_MAX_RESPONSE_BYTES - 128_000 {
                        more = true;
                        break 'pages;
                    }
                    used += size;
                    items.push(value);
                    cursor.before = Some(position);
                }
                before = Some(position);
            }
            if !full {
                break;
            }
        }
        let mut filters = BTreeMap::from([("ref".into(), query.subject.clone())]);
        if let Some(kind) = &query.kind {
            filters.insert("kind".into(), kind.clone());
        }
        self.page(cursor, filters, items, more)
    }
}

pub(in crate::api) async fn get(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<SubjectQuery>,
) -> Result<NativeResponse, ApiError> {
    let family = validate_ref(&query.subject)?;
    let (snapshot, subject) = read_native(state, session, None, move |reader| {
        reader
            .subject(&query.subject, &family)?
            .ok_or_else(|| ApiError::not_found("the native subject is not available"))
    })
    .await?;
    Ok((
        Extension(snapshot),
        Json(serde_json::to_value(subject).map_err(ApiError::internal)?),
    ))
}

pub(in crate::api) async fn list(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<SubjectsQuery>,
) -> Result<NativeResponse, ApiError> {
    let (_, (snapshot, page)) = read_native(state, session, None, move |reader| {
        let page = reader.subjects_page(&query)?;
        let index = query
            .cursor
            .as_deref()
            .map(decode_cursor)
            .transpose()?
            .map_or(reader.fence.graph_index, |cursor| cursor.graph_index);
        Ok((client_snapshot_at(reader.state, index), page))
    })
    .await?;
    Ok((Extension(snapshot), Json(page)))
}

pub(in crate::api) async fn claims(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<HistoryQuery>,
) -> Result<NativeResponse, ApiError> {
    let (_, (snapshot, page)) = read_native(state, session, None, move |reader| {
        let page = reader.history_page(&query, "subject-claims")?;
        let index = query
            .cursor
            .as_deref()
            .map(decode_cursor)
            .transpose()?
            .map_or(reader.fence.graph_index, |cursor| cursor.graph_index);
        Ok((client_snapshot_at(reader.state, index), page))
    })
    .await?;
    Ok((Extension(snapshot), Json(page)))
}

pub(in crate::api) async fn history(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<HistoryQuery>,
) -> Result<NativeResponse, ApiError> {
    let (_, (snapshot, page)) = read_native(state, session, None, move |reader| {
        let page = reader.history_page(&query, "subject-history")?;
        let index = query
            .cursor
            .as_deref()
            .map(decode_cursor)
            .transpose()?
            .map_or(reader.fence.graph_index, |cursor| cursor.graph_index);
        Ok((client_snapshot_at(reader.state, index), page))
    })
    .await?;
    Ok((Extension(snapshot), Json(page)))
}

pub(in crate::api) async fn schemas(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
) -> Result<NativeResponse, ApiError> {
    let (snapshot, payload) = read_native(state, session, None, |_reader| {
        static DISCOVERY: std::sync::LazyLock<Result<Value, String>> =
            std::sync::LazyLock::new(|| {
                let base = serde_json::from_str(include_str!(
                    "../../../../../docs/st3/client-v0/schemas/client-v0.schema.json"
                ))
                .map_err(|error| error.to_string())?;
                disclosure::Contract::derive(&base).map(|contract| contract.discovery)
            });
        DISCOVERY
            .as_ref()
            .cloned()
            .map_err(|error| ApiError::internal(anyhow::anyhow!("{error}")))
    })
    .await?;
    Ok((Extension(snapshot), Json(payload)))
}

pub(super) async fn collection_window(
    state: AppState,
    session: ClientSession,
    family: Option<String>,
    reference: Option<String>,
    prefix: Option<String>,
    limit: usize,
    read_permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<(ClientSnapshot, Vec<Value>, bool), ApiError> {
    if family.is_some() == reference.is_some() || (reference.is_some() && prefix.is_some()) {
        return Err(validation(
            "subjects requires exactly one family or ref; ref_prefix requires family",
        ));
    }
    let (snapshot, (items, more)) = read_native(state, session, Some(read_permit), move |reader| {
        if let Some(reference) = reference {
            let family = validate_ref(&reference)?;
            let item = reader
                .subject(&reference, &family)?
                .map(serde_json::to_value)
                .transpose()
                .map_err(ApiError::internal)?;
            return Ok((item.into_iter().collect(), false));
        }
        let mut page = reader.subjects_page(&SubjectsQuery {
            family: family.expect("validated selector"),
            ref_prefix: prefix,
            limit: Some(limit),
            cursor: None,
        })?;
        let more = page["page"]["has_more"]
            .as_bool()
            .expect("native page boundary");
        let Value::Array(items) = page["items"].take() else {
            return Err(ApiError::internal(anyhow::anyhow!(
                "native page items were not an array"
            )));
        };
        Ok((items, more))
    })
    .await?;
    Ok((snapshot, items, more))
}

pub(super) async fn validate_live_session(
    state: AppState,
    session: ClientSession,
) -> Result<(), ApiError> {
    read_native(state, session, None, |_| Ok(()))
        .await
        .map(|_| ())
}

#[cfg(test)]
mod disclosure_tests {
    use super::*;

    fn state(root: &Path) -> AppState {
        AppState {
            store: Arc::new(Store::open(&root.join("graph.db"), "subject-test").unwrap()),
            notify: Arc::new(Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: "subject-test".into(),
            state_dir: root.to_path_buf(),
            pty_root: root.join("pty"),
            pty_binary: root.join("unused-pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: crate::model::PlannerSpec::default(),
        }
    }

    fn append(state: &AppState, subject: &str, kind: &str, fields: Value) -> ClaimRecord {
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: Some("person/ada".into()),
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
            .unwrap()
    }

    fn source(record: ClaimRecord) -> NativeSourceRecord {
        NativeSourceRecord {
            record,
            local_position: None,
        }
    }

    #[tokio::test]
    async fn canceled_native_readers_keep_slots_until_their_sqlite_workers_finish() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        append(
            &state,
            "custom/team/held",
            "custom.team.record",
            json!({"value":"held"}),
        );
        let slots = Arc::new(tokio::sync::Semaphore::new(COLLECTION_MAX_SUBSCRIPTIONS));
        let mut releases = Vec::new();
        for _ in 0..COLLECTION_MAX_SUBSCRIPTIONS {
            let permit = slots.clone().acquire_owned().await.unwrap();
            let (started, ready) = tokio::sync::oneshot::channel();
            let (release, held) = std::sync::mpsc::channel();
            let worker = tokio::spawn(read_native(
                state.clone(),
                ClientSession::local(Some("person/ada")).unwrap(),
                Some(permit),
                move |reader| {
                    let subject = reader.subject("custom/team/held", "custom")?;
                    assert_eq!(subject.unwrap().subject, "custom/team/held");
                    started.send(()).unwrap();
                    held.recv().unwrap();
                    Ok(())
                },
            ));
            tokio::time::timeout(Duration::from_secs(5), ready)
                .await
                .unwrap()
                .unwrap();
            worker.abort();
            assert!(worker.await.unwrap_err().is_cancelled());
            releases.push(release);
        }
        let available = slots.available_permits();
        for release in releases {
            release.send(()).unwrap();
        }
        assert_eq!(
            available, 0,
            "canceling awaiters released live native SQLite read slots"
        );
        let restored = tokio::time::timeout(
            Duration::from_secs(5),
            slots.acquire_many_owned(u32::try_from(COLLECTION_MAX_SUBSCRIPTIONS).unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
        drop(restored);
    }

    #[test]
    fn planning_answers_resolve_exact_preceding_decision_revision() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let requested = |decision: &str, revision: u64, requester: &str| {
            append(
                &state,
                "planning-session/example",
                "planning-session.question-requested",
                json!({"decision_id":decision,"revision":revision,"requester":requester,
                "decision_type":"boolean","question":"Proceed?"}),
            )
        };
        let first = requested("decision", 1, "person/ada");
        requested("decision", 2, "person/alex");
        let answer = append(
            &state,
            "planning-session/example",
            "planning-session.question-answered",
            json!({"decision_id":"decision","expected_revision":1,"requester":"person/alex"}),
        );
        let future_answer = append(
            &state,
            "planning-session/example",
            "planning-session.question-answered",
            json!({"decision_id":"future","expected_revision":1,"requester":"person/ada"}),
        );
        requested("future", 1, "person/ada");
        let session = ClientSession::for_tests("person/ada/session/test", "person/ada", "unix");
        state
            .store
            .read_snapshot(|_| {
                let reader = Reader {
                    state: &state,
                    session: &session,
                    fence: state.store.native_source_fence()?,
                };
                assert!(reader.claim_visible(&first).unwrap());
                assert!(reader.claim_visible(&answer).unwrap());
                assert!(!reader.claim_visible(&future_answer).unwrap());
                let alex =
                    ClientSession::for_tests("person/alex/session/test", "person/alex", "unix");
                let alex_reader = Reader {
                    session: &alex,
                    ..reader
                };
                assert!(!alex_reader.claim_visible(&answer).unwrap());
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn approvals_and_recoveries_keep_their_actual_private_audience() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let approved = append(
            &state,
            "revision-proposal/example",
            "revision-proposal.approved",
            json!({"reviewer":"person/ada","all_approved":true}),
        );
        let failure = append(
            &state,
            "agent/example",
            "operational.failure",
            json!({"episode":"one","reviewer":"person/alex","reason":"private failure"}),
        );
        let recovered = append(
            &state,
            "agent/example",
            "operational.recovered",
            json!({"episode":"one","failure":failure.id,"reason":"private recovery"}),
        );
        let missing = append(
            &state,
            "agent/example",
            "operational.recovered",
            json!({"episode":"missing","failure":"unknown"}),
        );
        let unaddressed = append(
            &state,
            "agent/example",
            "operational.failure",
            json!({"episode":"unaddressed","reason":"not an audience grant"}),
        );
        let session = ClientSession::for_tests("person/ada/session/test", "person/ada", "unix");
        state
            .store
            .read_snapshot(|_| {
                let reader = Reader {
                    state: &state,
                    session: &session,
                    fence: state.store.native_source_fence()?,
                };
                assert!(reader.claim_visible(&approved).unwrap());
                assert!(!reader.claim_visible(&failure).unwrap());
                assert!(!reader.claim_visible(&recovered).unwrap());
                assert!(!reader.claim_visible(&missing).unwrap());
                assert!(!reader.claim_visible(&unaddressed).unwrap());
                let alex =
                    ClientSession::for_tests("person/alex/session/test", "person/alex", "unix");
                let alex_reader = Reader {
                    session: &alex,
                    ..reader
                };
                assert!(alex_reader.claim_visible(&recovered).unwrap());
                assert!(!alex_reader.claim_visible(&approved).unwrap());
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn excluded_records_cannot_establish_message_or_attention_identity() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let message = append(
            &state,
            "message/excluded",
            "message.sent",
            json!({"from":"person/ada","to":"agent/shared","status":"sent","content":"private"}),
        );
        let reply = append(
            &state,
            "message/reply",
            "message.sent",
            json!({"from":"person/alex","to":"person/reviewer","status":"sent",
                "content":"private","in_reply_to":message.subject}),
        );
        let attention = append(
            &state,
            "attention/excluded",
            "attention.requested",
            json!({"reviewer":"person/ada","title":"private","reason":"private","severity":"warning"}),
        );
        let session = ClientSession::for_tests("person/ada/session/test", "person/ada", "unix");
        state
            .store
            .read_snapshot(|_| {
                let reader = Reader {
                    state: &state,
                    session: &session,
                    fence: state.store.native_source_fence()?,
                };
                assert!(
                    reader
                        .identity_visible(&message.subject, "message")
                        .unwrap()
                );
                assert!(
                    reader
                        .identity_visible(&attention.subject, "attention")
                        .unwrap()
                );
                assert!(reader.identity_visible(&reply.subject, "message").unwrap());
                Ok(())
            })
            .unwrap();
        let connection = rusqlite::Connection::open(root.path().join("graph.db")).unwrap();
        crate::store::projection_digest::register(&connection).unwrap();
        for id in [&message.id, &attention.id] {
            connection
                .execute(
                    "INSERT INTO projection_digest_repaired_claims(id) VALUES(?1)",
                    [id],
                )
                .unwrap();
        }
        state
            .store
            .read_snapshot(|_| {
                let reader = Reader {
                    state: &state,
                    session: &session,
                    fence: state.store.native_source_fence()?,
                };
                assert!(
                    !reader
                        .identity_visible(&message.subject, "message")
                        .unwrap()
                );
                assert!(
                    !reader
                        .identity_visible(&attention.subject, "attention")
                        .unwrap()
                );
                assert!(!reader.identity_visible(&reply.subject, "message").unwrap());
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn resource_facts_never_disclose_legacy_or_unregistered_bags() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let mut record = append(
            &state,
            "resource/example",
            "resource.observed",
            json!({"kind":"filesystem.file","observed_at":1}),
        );
        record.body = json!({"fields":{"kind":"filesystem.file","observed_at":1,
            "state":{"secret":"hidden"},"facts":{"status":null,"path":"/safe",
                "private-id":{"secret":"hidden"}},"other-secret":"hidden"}});
        let session = ClientSession::for_tests("person/ada/session/test", "person/ada", "unix");
        state
            .store
            .read_snapshot(|_| {
                let reader = Reader {
                    state: &state,
                    session: &session,
                    fence: state.store.native_source_fence()?,
                };
                let projected = reader
                    .project(source(record.clone()), "resource", true)
                    .unwrap();
                assert_eq!(
                    projected.fields.get("facts"),
                    Some(&json!({"status":null,"path":"/safe"}))
                );
                assert!(!projected.fields.contains_key("state"));
                assert!(
                    !projected
                        .omitted_fields
                        .iter()
                        .any(|name| name == "private-id" || name == "other-secret")
                );
                assert!(
                    !projected
                        .omitted_fields
                        .iter()
                        .any(|name| name == "observed_at")
                );
                record.body["fields"]["kind"] = json!("unreviewed.kind");
                let unknown = reader
                    .project(source(record.clone()), "resource", true)
                    .unwrap();
                assert!(!unknown.fields.contains_key("facts"));
                assert!(!unknown.fields.contains_key("state"));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn desired_projectors_follow_family_policy_and_sensitive_history_scope() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let agent = append(
            &state,
            "agent/example",
            "intent.desired",
            json!({"desired":{
            "name":"agent","arguments":["example"],"properties":{},"children":[{
                "name":"env","arguments":[],"properties":{},"children":[{
                    "name":"TOKEN","arguments":["private"],"properties":{},"children":[]}]}]}}),
        );
        let account = append(
            &state,
            "account/ada/provider",
            "intent.desired",
            json!({"desired":{
            "name":"account","arguments":["ada/provider"],"properties":{"token":"private"},
            "children":[{"name":"provider","arguments":["claude"],"properties":{},"children":[]},
                {"name":"credential","arguments":["private"],"properties":{},"children":[]}]}}),
        );
        let mut session = ClientSession::for_tests("person/ada/session/test", "person/ada", "unix");
        session.scopes.remove("read.declarations");
        state
            .store
            .read_snapshot(|_| {
                let reader = Reader {
                    state: &state,
                    session: &session,
                    fence: state.store.native_source_fence()?,
                };
                let current = reader
                    .project(source(agent.clone()), "agent", false)
                    .unwrap();
                assert_eq!(
                    current.fields["desired"]["children"][0]["children"][0]["arguments"],
                    json!(["<redacted>"])
                );
                let history = reader
                    .project(source(agent.clone()), "agent", true)
                    .unwrap();
                assert!(!history.fields.contains_key("desired"));
                assert!(history.omitted_fields.iter().any(|name| name == "desired"));
                let mut privileged =
                    ClientSession::for_tests("person/ada/session/test", "person/ada", "unix");
                privileged.scopes.insert("read.declarations".into());
                let privileged_reader = Reader {
                    session: &privileged,
                    ..reader
                };
                let projected = privileged_reader
                    .project(source(account.clone()), "account", true)
                    .unwrap();
                assert_eq!(
                    projected.fields["desired"],
                    json!({"name":"account",
                "arguments":["ada/provider"],"properties":{},"children":[{
                    "name":"provider","arguments":["claude"],"properties":{},"children":[]}]})
                );
                let mut null = agent.clone();
                null.body = json!({"fields":{"desired":null}});
                assert_eq!(
                    privileged_reader
                        .project(source(null), "agent", true)
                        .unwrap()
                        .fields
                        .get("desired"),
                    Some(&Value::Null)
                );
                let mut absent = agent.clone();
                absent.body = json!({"fields":{}});
                let absent = privileged_reader
                    .project(source(absent), "agent", true)
                    .unwrap();
                assert!(!absent.fields.contains_key("desired"));
                assert!(!absent.omitted_fields.iter().any(|name| name == "desired"));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn desired_ast_redaction_drops_extra_keys_and_rejects_nested_env_values() {
        let mut desired = json!({"name":"agent","arguments":["example"],"properties":{},
            "hidden":{"token":"private"},"children":[{"name":"env","arguments":[],
                "properties":{},"children":[{"name":"TOKEN","arguments":["private"],
                    "properties":{},"children":[],"hidden":"private"}]}]});
        let mut safe = canonical_desired(&desired, "agent").unwrap();
        crate::graph::redact_agent_env_values(&mut safe);
        assert_eq!(
            safe["children"][0]["children"][0]["arguments"],
            json!(["<redacted>"])
        );
        assert!(safe.get("hidden").is_none());
        assert!(safe["children"][0]["children"][0].get("hidden").is_none());
        desired["children"][0]["children"][0]["children"] =
            json!([{"name":"nested","arguments":["private"],"properties":{},"children":[]}]);
        assert!(canonical_desired(&desired, "agent").is_err());
    }

    #[test]
    fn sparse_native_desired_ast_preserves_redaction_and_type_boundaries() {
        let desired = json!({"name":"agent","arguments":["example"],"children":[
            {"name":"harness","arguments":["claude"]},
            {"name":"env","children":[{"name":"TOKEN","arguments":["private"]}]}]});
        let mut safe = canonical_desired(&desired, "agent").unwrap();
        crate::graph::redact_agent_env_values(&mut safe);
        assert_eq!(safe["children"][0]["arguments"], json!(["claude"]));
        assert_eq!(
            safe["children"][1]["children"][0]["arguments"],
            json!(["<redacted>"])
        );
        for (field, invalid) in [
            ("arguments", json!({})),
            ("properties", json!([])),
            ("children", Value::Null),
        ] {
            let mut malformed = desired.clone();
            malformed["children"][0][field] = invalid;
            assert!(canonical_desired(&malformed, "agent").is_err());
        }
    }

    #[test]
    fn unsafe_float_integers_are_withheld_at_any_json_depth() {
        assert!(!json_safe(&json!({"nested":[9_007_199_254_740_992.0]})));
        assert!(!json_safe(&json!(-9_007_199_254_740_992.0)));
        assert!(!json_safe(&json!(u64::MAX)));
        assert!(json_safe(
            &json!({"nested":[9_007_199_254_740_991_u64, 0.5, null]})
        ));
    }

    #[test]
    fn durable_answer_audience_survives_reverse_request_arrival() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let answer = append(
            &state,
            "planning-session/reordered",
            "planning-session.question-answered",
            json!({"decision_id":"decision","expected_revision":1,"requester":"person/alex"}),
        );
        let request = append(
            &state,
            "planning-session/reordered",
            "planning-session.question-requested",
            json!({"decision_id":"decision","revision":1,"requester":"person/ada",
                "decision_type":"boolean","question":"Proceed?"}),
        );
        // Model the immutable earlier canonical timestamp of a request received
        // after its answer; arrival indexes intentionally remain reversed.
        let connection = rusqlite::Connection::open(root.path().join("graph.db")).unwrap();
        crate::store::projection_digest::register(&connection).unwrap();
        connection
            .execute(
                "UPDATE claims SET accepted_at_unix_ms=?1 WHERE id=?2",
                rusqlite::params![
                    answer.accepted_at_unix_ms.saturating_sub(1).to_string(),
                    request.id
                ],
            )
            .unwrap();
        let ada = ClientSession::for_tests("person/ada/session/test", "person/ada", "unix");
        let alex = ClientSession::for_tests("person/alex/session/test", "person/alex", "unix");
        state
            .store
            .read_snapshot(|_| {
                let reader = Reader {
                    state: &state,
                    session: &ada,
                    fence: state.store.native_source_fence()?,
                };
                assert!(reader.claim_visible(&answer).unwrap());
                assert!(
                    !Reader {
                        session: &alex,
                        ..reader
                    }
                    .claim_visible(&answer)
                    .unwrap()
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn fenced_glass_continuations_recheck_current_deletion() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let first = "glass/person/ada/019a0000-0000-7000-8000-000000000001";
        let second = "glass/person/ada/019a0000-0000-7000-8000-000000000002";
        for reference in [first, second] {
            append(
                &state,
                reference,
                "glass.upserted",
                json!({"body":{"name":"Private","layout":{"tabs":[]}},"base_revision":null}),
            );
        }
        let mut session = ClientSession::for_tests("person/ada/session/test", "person/ada", "unix");
        session.scopes.insert("read.glasses".into());
        let mut query = SubjectsQuery {
            family: "glass".into(),
            limit: Some(1),
            ..Default::default()
        };
        let page = state
            .store
            .read_snapshot(|_| {
                let reader = Reader {
                    state: &state,
                    session: &session,
                    fence: state.store.native_source_fence()?,
                };
                Ok(reader.subjects_page(&query).unwrap())
            })
            .unwrap();
        assert_eq!(page["items"][0]["ref"], first);
        query.cursor = Some(page["page"]["next_cursor"].as_str().unwrap().to_owned());
        append(&state, second, "glass.deleted", json!({}));
        state
            .store
            .read_snapshot(|_| {
                let reader = Reader {
                    state: &state,
                    session: &session,
                    fence: state.store.native_source_fence()?,
                };
                let continuation = reader.subjects_page(&query).unwrap();
                assert_eq!(continuation["items"], json!([]));
                assert_eq!(continuation["page"]["has_more"], false);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn native_cursor_rejects_body_tampering_and_another_session() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let session = ClientSession::for_tests("person/ada/session/test", "person/ada", "unix");
        state
            .store
            .read_snapshot(|_| {
                let reader = Reader {
                    state: &state,
                    session: &session,
                    fence: state.store.native_source_fence()?,
                };
                let cursor = reader
                    .cursor(
                        None,
                        CursorBoundary {
                            collection: "subjects",
                            family: "agent",
                            subject: None,
                            prefix: None,
                            kind: None,
                            limit: 1,
                        },
                    )
                    .unwrap();
                let encoded = encode_cursor(&cursor).unwrap();
                let (_, signed) = encoded.split_once('/').unwrap();
                let (body, signature) = signed.split_once('.').unwrap();
                let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(body)
                    .unwrap();
                let mut altered: Value = serde_json::from_slice(&bytes).unwrap();
                altered["limit"] = json!(200);
                let altered = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(serde_json::to_vec(&altered).unwrap());
                assert!(decode_cursor(&format!("subject-page/{altered}.{signature}")).is_err());
                let other =
                    ClientSession::for_tests("person/ada/session/other", "person/ada", "unix");
                assert!(
                    Reader {
                        session: &other,
                        ..reader
                    }
                    .cursor(
                        Some(&encoded),
                        CursorBoundary {
                            collection: "subjects",
                            family: "agent",
                            subject: None,
                            prefix: None,
                            kind: None,
                            limit: 1,
                        }
                    )
                    .is_err()
                );
                Ok(())
            })
            .unwrap();
    }
    #[test]
    fn private_family_scan_keeps_hidden_refs_out_of_cursors() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        for (reference, reviewer) in [
            ("attention/A", "person/ada"),
            ("attention/B", "person/alex"),
            ("attention/C", "person/ada"),
        ] {
            append(
                &state,
                reference,
                "attention.requested",
                json!({"reviewer":reviewer,"title":"Review","reason":"Review","severity":"warning"}),
            );
        }
        let session = ClientSession::for_tests("person/ada/session/test", "person/ada", "unix");
        let mut query = SubjectsQuery {
            family: "attention".into(),
            limit: Some(1),
            ..Default::default()
        };
        let first = state
            .store
            .read_snapshot(|_| {
                let reader = Reader {
                    state: &state,
                    session: &session,
                    fence: state.store.native_source_fence()?,
                };
                Ok(reader.subjects_page(&query).unwrap())
            })
            .unwrap();
        assert_eq!(first["items"][0]["ref"], "attention/A");
        query.cursor = Some(first["page"]["next_cursor"].as_str().unwrap().to_owned());
        assert_eq!(
            decode_cursor(query.cursor.as_deref().unwrap())
                .unwrap()
                .after_ref
                .as_deref(),
            Some("attention/A")
        );
        state
            .store
            .read_snapshot(|_| {
                let reader = Reader {
                    state: &state,
                    session: &session,
                    fence: state.store.native_source_fence()?,
                };
                let second = reader.subjects_page(&query).unwrap();
                assert_eq!(second["items"][0]["ref"], "attention/C");
                assert_eq!(second["page"]["has_more"], false);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn family_cursor_expires_when_an_unreturned_source_is_repaired() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let mut records = Vec::new();
        for reference in ["attention/A", "attention/B", "attention/C"] {
            records.push(append(&state, reference, "attention.requested",
                json!({"reviewer":"person/ada","title":"Review","reason":"Review","severity":"warning"})));
        }
        let session = ClientSession::for_tests("person/ada/session/test", "person/ada", "unix");
        let mut query = SubjectsQuery {
            family: "attention".into(),
            limit: Some(1),
            ..Default::default()
        };
        let first = state
            .store
            .read_snapshot(|_| {
                let reader = Reader {
                    state: &state,
                    session: &session,
                    fence: state.store.native_source_fence()?,
                };
                Ok(reader.subjects_page(&query).unwrap())
            })
            .unwrap();
        query.cursor = Some(first["page"]["next_cursor"].as_str().unwrap().to_owned());
        let connection = rusqlite::Connection::open(root.path().join("graph.db")).unwrap();
        crate::store::projection_digest::register(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO projection_digest_repaired_claims(id) VALUES(?1)",
                [&records[2].id],
            )
            .unwrap();
        state
            .store
            .read_snapshot(|_| {
                let reader = Reader {
                    state: &state,
                    session: &session,
                    fence: state.store.native_source_fence()?,
                };
                assert!(reader.subjects_page(&query).is_err());
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn glass_scope_refusal_does_not_depend_on_private_identity_existence() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let session = ClientSession::for_tests("person/ada/session/test", "person/ada", "unix");
        let query = SubjectsQuery {
            family: "glass".into(),
            ..Default::default()
        };
        let refused = || {
            state
                .store
                .read_snapshot(|_| {
                    let reader = Reader {
                        state: &state,
                        session: &session,
                        fence: state.store.native_source_fence()?,
                    };
                    Ok(reader.subjects_page(&query).is_err())
                })
                .unwrap()
        };
        assert!(refused());
        state
            .store
            .append_claim(&ClaimInput {
                subject: "glass/person/alex/019a0000-0000-7000-8000-000000000001".into(),
                kind: "glass.upserted".into(),
                actor: Some("person/alex".into()),
                fields: serde_json::from_value(
                    json!({"body":{"name":"Private","layout":{"tabs":[]}},"base_revision":null}),
                )
                .unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        assert!(refused());
    }

    #[test]
    fn native_stop_retains_claim_metadata_without_a_declaration_bag() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        append(
            &state,
            "agent/stopped",
            "intent.desired",
            json!({"kind":"stop","revision":"stop-revision","desired":{"stop":"agent/stopped"}}),
        );
        let session = ClientSession::for_tests("person/ada/session/test", "person/ada", "unix");
        state
            .store
            .read_snapshot(|_| {
                let reader = Reader {
                    state: &state,
                    session: &session,
                    fence: state.store.native_source_fence()?,
                };
                let projected = reader.subject("agent/stopped", "agent").unwrap().unwrap();
                assert_eq!(projected.heads[0].fields["kind"], "stop");
                assert_eq!(projected.heads[0].fields["revision"], "stop-revision");
                assert!(!projected.heads[0].fields.contains_key("desired"));
                let page = reader
                    .history_page(
                        &HistoryQuery {
                            subject: "agent/stopped".into(),
                            kind: None,
                            limit: None,
                            cursor: None,
                        },
                        "subject-history",
                    )
                    .unwrap();
                assert_eq!(
                    page["coverage"]["local_retention"]["window_ms"],
                    7 * 86_400_000_u64
                );
                assert!(page.get("sync").is_none());
                Ok(())
            })
            .unwrap();
    }
}
