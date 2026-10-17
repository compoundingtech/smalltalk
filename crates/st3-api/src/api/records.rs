use super::*;

pub async fn put_document(
    State(state): State<AppState>,
    bound: Option<axum::Extension<BoundAgent>>,
    Json(request): Json<DocumentPutRequest>,
) -> Result<Json<DocumentVersion>, ApiError> {
    if let Some(bound) = bound.as_ref()
        && !crate::handoff::may_write(&bound.0.0, &request.name)
    {
        return Err(ApiError::bad(St3Error::new(
            "document-not-yours",
            format!(
                "`{}` belongs to another seat; a seat writes only doc/agents/… for itself",
                request.name
            ),
        )));
    }
    let response = state
        .store
        .put_document_as(
            &request.name,
            &request.bytes,
            &request.expected_document,
            &request.idempotency_key,
            bound.as_ref().map(|bound| bound.0.0.as_str()),
        )
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(response))
}

pub async fn list_rules(
    State(state): State<AppState>,
) -> Result<Json<Vec<smallclaims::rules::NamedRule>>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || store.current_rules().map(|rules| rules.as_ref().clone()))
        .await
        .map(Json)
}

#[derive(Deserialize)]
pub struct RuleAuditQuery {
    rule: Option<String>,
    limit: Option<usize>,
}

pub async fn list_rule_audits(
    State(state): State<AppState>,
    Query(query): Query<RuleAuditQuery>,
) -> Result<Json<Vec<RuleAudit>>, ApiError> {
    let store = state.store.clone();
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    blocking_store(move || {
        Ok(store
            .rule_audits(query.rule.as_deref(), limit)?
            .into_iter()
            .map(|claim| {
                let field = |name: &str| {
                    claim.body["fields"][name]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned()
                };
                RuleAudit {
                    rule: claim
                        .subject
                        .strip_prefix("rule/")
                        .unwrap_or(&claim.subject)
                        .to_owned(),
                    actor: field("actor"),
                    action: field("action"),
                    target: field("target"),
                    at_unix_ms: claim.accepted_at_unix_ms,
                    claim: claim.id,
                }
            })
            .collect())
    })
    .await
    .map(Json)
}

pub async fn set_rule(
    State(state): State<AppState>,
    Json(request): Json<RuleSetRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    concrete_person(&request.actor)?;
    let claim = state
        .store
        .set_rule(&request.name, &request.rule, &request.actor)
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(claim))
}

#[derive(Deserialize)]
pub struct DocumentQuery {
    name: Option<String>,
    prefix: Option<String>,
    cursor: Option<String>,
    #[serde(default)]
    history: bool,
    limit: Option<usize>,
}

#[derive(Serialize, Deserialize)]
struct DocumentCursor {
    name: String,
    created_index: u64,
    exact_name: Option<String>,
    prefix: Option<String>,
    history: bool,
}

pub async fn list_documents(
    State(state): State<AppState>,
    Query(query): Query<DocumentQuery>,
) -> Result<Json<DocumentListResponse>, ApiError> {
    let limit = query.limit.unwrap_or(100).clamp(1, 200);
    let history = query.history;
    let cursor = query
        .cursor
        .as_deref()
        .map(|encoded| {
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded.strip_prefix("document/").unwrap_or(""))
                .map_err(|_| {
                    ApiError::bad(St3Error::new(
                        "validation-failed",
                        "invalid document cursor",
                    ))
                })?;
            let cursor: DocumentCursor = serde_json::from_slice(&bytes).map_err(|_| {
                ApiError::bad(St3Error::new(
                    "validation-failed",
                    "invalid document cursor",
                ))
            })?;
            if cursor.exact_name != query.name
                || cursor.prefix != query.prefix
                || cursor.history != history
            {
                return Err(ApiError::bad(St3Error::new(
                    "validation-failed",
                    "document cursor filters changed",
                )));
            }
            Ok(cursor)
        })
        .transpose()?;
    let store = state.store.clone();
    let name = query.name.clone();
    let prefix = query.prefix.clone();
    let mut items = blocking_store(move || {
        store.list_documents_page(
            name.as_deref(),
            prefix.as_deref(),
            history,
            cursor
                .as_ref()
                .map(|cursor| (cursor.name.as_str(), cursor.created_index)),
            limit.saturating_add(1),
        )
    })
    .await?;
    let has_more = items.len() > limit;
    items.truncate(limit);
    let next_cursor = if has_more {
        let last = items.last().expect("nonempty page with more items");
        let cursor = DocumentCursor {
            name: last.name.clone(),
            created_index: last.created_index,
            exact_name: query.name,
            prefix: query.prefix,
            history,
        };
        Some(format!(
            "document/{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&cursor).map_err(ApiError::internal)?)
        ))
    } else {
        None
    };
    Ok(Json(DocumentListResponse {
        items,
        has_more,
        limit,
        history,
        next_cursor,
    }))
}

#[derive(Deserialize)]
pub struct DocumentContentQuery {
    reference: String,
}

#[derive(Serialize)]
pub struct DocumentContent {
    reference: String,
    bytes: Vec<u8>,
}

pub async fn get_document(
    State(state): State<AppState>,
    Query(query): Query<DocumentContentQuery>,
) -> Result<Json<DocumentContent>, ApiError> {
    let (name, hash) = query.reference.rsplit_once('@').ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "invalid-document-reference",
            "a document reference needs `@HASH`",
        ))
    })?;
    let store = state.store.clone();
    let name = name.to_owned();
    let hash = hash.to_owned();
    let bytes = blocking_store(move || store.get_document(&name, &hash))
        .await?
        .ok_or_else(|| {
            ApiError::not_found(format!("document `{}` is not stored", query.reference))
        })?;
    Ok(Json(DocumentContent {
        reference: query.reference,
        bytes,
    }))
}

pub async fn post_claim(
    State(state): State<AppState>,
    Json(request): Json<ClaimInput>,
) -> Result<Json<ClaimRecord>, ApiError> {
    // A write can wait for the store's writer. It waits on the blocking pool, so the API's
    // workers keep answering other requests meanwhile.
    let store = state.store.clone();
    let kind = request.kind.clone();
    let (response, appended) =
        blocking_action(move || store.append_client_claim_outcome(&request)).await?;
    finish_claim_publication(&state, &kind, response, appended, None).await
}

pub async fn get_claim(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ClaimRecord>, ApiError> {
    let store = state.store.clone();
    let id_for_read = id.clone();
    blocking_store(move || store.claim_by_id(&id_for_read))
        .await?
        .filter(|claim| !claim.subject.starts_with("glass/"))
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("claim `{id}` does not exist")))
}

#[derive(Deserialize)]
pub struct ClaimsQuery {
    subject: Option<String>,
    owner_run: Option<String>,
    #[serde(default, alias = "after")]
    after_index: u64,
    #[serde(alias = "before")]
    before_index: Option<u64>,
    #[serde(default)]
    order: ClaimsOrder,
    #[serde(default = "default_claim_limit")]
    limit: usize,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ClaimsOrder {
    #[default]
    Asc,
    Desc,
}

pub async fn list_claims(
    State(state): State<AppState>,
    Query(query): Query<ClaimsQuery>,
) -> Result<Json<ClaimsPage>, ApiError> {
    if query.limit == 0 || query.limit > 500 {
        return Err(ApiError::bad(St3Error::new(
            "invalid-page-limit",
            "a claim page limit must be between 1 and 500",
        )));
    }
    let store = state.store.clone();
    blocking_store(move || {
        store.claims_page(
            query.subject.as_deref(),
            query.owner_run.as_deref(),
            query.after_index,
            query.before_index,
            matches!(query.order, ClaimsOrder::Desc),
            query.limit,
        )
    })
    .await
    .map(Json)
}

#[derive(Default, Deserialize)]
pub struct ReviewsQuery {
    reviewer: Option<String>,
}

pub async fn list_reviews(
    State(state): State<AppState>,
    Query(query): Query<ReviewsQuery>,
) -> Result<Json<Vec<HumanReviewView>>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || store.pending_human_reviews(query.reviewer.as_deref()))
        .await
        .map(Json)
}

#[derive(Default, Deserialize)]
pub struct AttentionQuery {
    person: Option<String>,
}

pub async fn list_attention(
    State(state): State<AppState>,
    Query(query): Query<AttentionQuery>,
) -> Result<Json<Vec<AttentionItemView>>, ApiError> {
    let person = query.person.map(|person| {
        if person.contains('/') {
            person
        } else {
            format!("person/{person}")
        }
    });
    if person
        .as_deref()
        .is_some_and(|person| !person.starts_with("person/"))
    {
        return Err(ApiError::bad(St3Error::new(
            "invalid-attention-person",
            "an attention filter must name a person subject",
        )));
    }
    let store = state.store.clone();
    blocking_store(move || store.attention_items(person.as_deref()))
        .await
        .map(Json)
}

pub async fn request_attention(
    State(_state): State<AppState>,
    Json(_post): Json<crate::model::AttentionRequestPost>,
) -> Result<Json<AttentionRequestView>, ApiError> {
    Err(ApiError::bad(St3Error::new(
        "attention-migrated",
        "attention is a derived view; use work ask/done/cancel-ask or act on its source",
    )))
}

pub async fn resolve_attention(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Json(request): Json<AttentionResolveRequest>,
) -> Result<Json<AttentionRequestView>, ApiError> {
    if let Some(proof) = request.delegation {
        let subject = if subject.starts_with("attention/") {
            subject
        } else {
            format!("attention/{subject}")
        };
        let actor = request.actor.clone();
        let summary = request.reason.clone().unwrap_or_else(|| "Read".into());
        if !matches!(request.outcome.as_str(), "resolved" | "dismissed") {
            return Err(ApiError::bad(St3Error::new(
                "invalid-attention-outcome",
                "an attention outcome must be resolved or dismissed",
            )));
        }
        // Resolve the exact original update, including a completed one on an identical retry.
        // Attention snapshots contain only open cards; they cannot identify closed episodes.
        let update = state
            .store
            .delegated_update_request(&proof)
            .map_err(ApiError::bad)?;
        if client_attention_id(&update.subject, &proof.person, &proof.episode)
            .map_err(ApiError::internal)?
            != subject
        {
            return Err(ApiError::bad(St3Error::new(
                "delegation-refused",
                "the attention card does not match the update episode",
            )));
        }
        let card = json!({
            "episode": proof.episode, "person_id": proof.person,
            "title": update.body["fields"]["title"], "detail": update.body["fields"]["reason"]
        });
        let target = update.subject;
        let response = state
            .store
            .finish_person_step(
                &PersonStepResponse {
                    subject: target.clone(),
                    actor: request.actor,
                    summary: summary.clone(),
                    evidence: vec![],
                    episode: Some(proof.episode.clone()),
                    delegation: Some(proof),
                    answer: Some(crate::person_request::AnswerInput {
                        id: Some("read".into()),
                        text: None,
                    }),
                    idempotency_key: request.idempotency_key,
                },
                false,
            )
            .map_err(ApiError::bad)?;
        signal_changed(&state);
        // Preserve the legacy response shape for callers of this compatibility endpoint.
        return Ok(Json(AttentionRequestView {
            subject,
            request: card["episode"].as_str().unwrap_or_default().into(),
            reviewer: card["person_id"].as_str().unwrap_or_default().into(),
            title: card["title"].as_str().unwrap_or_default().into(),
            reason: card["detail"].as_str().unwrap_or_default().into(),
            severity: "warning".into(),
            targets: vec![target],
            actor,
            requested_at_unix_ms: response.created_at_unix_ms,
            status: "resolved".into(),
            resolved_at_unix_ms: Some(response.updated_at_unix_ms),
            outcome: Some(request.outcome),
            resolution_reason: Some(summary),
            until: None,
            step: None,
            step_attempt: None,
            closed_by: None,
        }));
    }
    Err(ApiError::bad(St3Error::new(
        "attention-migrated",
        "attention is a derived view; use work ask/done/cancel-ask or act on its source",
    )))
}

pub async fn withdraw_attention(
    State(_state): State<AppState>,
    AxumPath(_subject): AxumPath<String>,
    Json(_request): Json<AttentionWithdrawRequest>,
) -> Result<Json<AttentionRequestView>, ApiError> {
    Err(ApiError::bad(St3Error::new(
        "attention-migrated",
        "attention is a derived view; use work ask/done/cancel-ask or act on its source",
    )))
}

/// The owner a review decision answers. A step, mission or loop run, or a resource, answers for
/// itself. An `attention/...` card or a `gate-operation/...` request names its gate's owner, and
/// a bare `GENERATION/PATH` a step run. Anything else is refused as naming no review, rather
/// than read as a step run that has none.
fn review_owner(state: &AppState, target: &str) -> Result<String, ApiError> {
    if ["resource/", "step-run/", "mission-run/", "loop-run/"]
        .iter()
        .any(|prefix| target.starts_with(prefix))
    {
        return Ok(target.to_owned());
    }
    let unknown = |why: String| ApiError::bad(St3Error::new("review-target-unknown", why));
    if target.starts_with("attention/") {
        if let Some(card) = client_attention_resources(&state.store, None, false)
            .map_err(ApiError::internal)?
            .into_iter()
            .find(|card| card["id"] == target)
        {
            return match (card["attention_kind"].as_str(), card["source_id"].as_str()) {
                (Some("human-gate"), Some(owner)) => Ok(owner.to_owned()),
                (kind, _) => Err(unknown(format!(
                    "`{target}` is a {} card, not a review; answer it where it asks",
                    kind.unwrap_or("different")
                ))),
            };
        }
        // A gate's card closes once it is answered or asked again. Say which, for its owner.
        for (request, owner, reviewer) in state
            .store
            .human_gate_requests()
            .map_err(ApiError::internal)?
        {
            if client_attention_id(&owner, &reviewer, &request).map_err(ApiError::internal)?
                != target
            {
                continue;
            }
            let current = state
                .store
                .pending_human_reviews(Some(&reviewer))
                .map_err(ApiError::internal)?
                .into_iter()
                .find(|review| review.owner == owner);
            let why = match current {
                Some(review) => format!(
                    "st asked `{owner}` again as `{}`; review that card",
                    client_attention_id(&owner, &review.reviewer, &review.request)
                        .map_err(ApiError::internal)?
                ),
                None => format!(
                    "`{owner}` has no pending human review: {}",
                    state
                        .store
                        .human_review_refusal(&owner)
                        .map_err(ApiError::internal)?
                ),
            };
            return Err(ApiError::bad(St3Error::new(
                "review-not-requested",
                format!("`{target}` is not open: {why}"),
            )));
        }
        return Err(unknown(format!("`{target}` names no review card")));
    }
    if target.starts_with("gate-operation/") {
        return state
            .store
            .claims_for(target, Some("gate.requested"))
            .map_err(ApiError::internal)?
            .last()
            .and_then(|request| request.body.pointer("/fields/owner"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| unknown(format!("`{target}` names no gate request")));
    }
    let step = format!("step-run/{target}");
    if target.contains('/')
        && state
            .store
            .step_run(&step)
            .map_err(ApiError::internal)?
            .is_some()
    {
        return Ok(step);
    }
    Err(unknown(format!(
        "`{target}` names no step run, mission run, loop or attention card; \
         `st alerts ls --as PERSON` lists the reviews waiting"
    )))
}

pub async fn post_review(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Json(request): Json<ReviewRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    if !matches!(
        request.decision.as_str(),
        "approved" | "rejected" | "changes-requested"
    ) {
        return Err(ApiError::bad(St3Error::new(
            "invalid-review-decision",
            "a review decision must be approved, rejected, or changes-requested",
        )));
    }
    if matches!(request.decision.as_str(), "rejected" | "changes-requested")
        && request
            .reason
            .as_deref()
            .is_none_or(|reason| reason.trim().is_empty())
    {
        return Err(ApiError::bad(St3Error::new(
            "missing-review-reason",
            "a rejection or request for changes needs a reason",
        )));
    }
    let subject = review_owner(&state, &subject)?;
    let actor = request.actor.map(|actor| {
        if actor.contains('/') {
            actor
        } else {
            format!("person/{actor}")
        }
    });
    let review_request = if subject.starts_with("step-run/")
        || subject.starts_with("mission-run/")
        || subject.starts_with("loop-run/")
    {
        let pending = state
            .store
            .pending_human_reviews(None)
            .map_err(ApiError::internal)?
            .into_iter()
            .find(|review| review.owner == subject);
        let Some(pending) = pending else {
            let why = state
                .store
                .human_review_refusal(&subject)
                .map_err(ApiError::internal)?;
            return Err(ApiError::bad(St3Error::new(
                "review-not-requested",
                format!("`{subject}` has no pending human review: {why}"),
            )));
        };
        let review_request = state
            .store
            .claim_by_id(&pending.request)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::internal("the pending review request does not exist"))?;
        let reviewer = review_request
            .body
            .pointer("/fields/reviewer")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::internal("a human review request has no reviewer"))?;
        if actor.as_deref() != Some(reviewer)
            && !request.delegation.as_ref().is_some_and(|proof| {
                proof.person == reviewer
                    && actor
                        .as_deref()
                        .is_some_and(|actor| actor.starts_with("agent/"))
            })
        {
            return Err(ApiError::bad(St3Error::new(
                "wrong-reviewer",
                format!("the pending review requires `{reviewer}`"),
            )));
        }
        Some(review_request)
    } else {
        None
    };
    let mode = review_request
        .as_ref()
        .and_then(|claim| claim.body.pointer("/fields/mode"))
        .and_then(Value::as_str)
        .unwrap_or("approve");
    if mode == "feedback" && !subject.starts_with("step-run/") {
        return Err(ApiError::bad(St3Error::new(
            "feedback-gate-needs-step",
            "a feedback review must belong to a step",
        )));
    }
    if !match mode {
        "feedback" => matches!(request.decision.as_str(), "approved" | "changes-requested"),
        _ => matches!(request.decision.as_str(), "approved" | "rejected"),
    } {
        return Err(ApiError::bad(St3Error::new(
            "review-decision-not-offered",
            format!(
                "`{}` is not offered by this {mode} review",
                request.decision
            ),
        )));
    }
    let verdict = match request.decision.as_str() {
        "approved" => "pass",
        "changes-requested" => "feedback",
        _ => "fail",
    };
    let mut fields = BTreeMap::from([
        ("verdict".into(), Value::String(verdict.into())),
        ("decision".into(), Value::String(request.decision.clone())),
        (
            "reason".into(),
            request.reason.map(Value::String).unwrap_or(Value::Null),
        ),
    ]);
    if let Some(review_request) = &review_request {
        fields.insert("request".into(), Value::String(review_request.id.clone()));
    }
    let mut evidence: Vec<String> = review_request
        .iter()
        .map(|request| request.id.clone())
        .collect();
    if let Some(proof) = &request.delegation {
        crate::store::delegation::add_fields(&mut fields, proof);
        state.store.delegation_evidence(&mut evidence, proof).map_err(ApiError::bad)?;
    }
    let response = state
        .store
        .append_claim(&ClaimInput {
            subject: review_request
                .as_ref()
                .map(|request| request.subject.clone())
                .unwrap_or_else(|| subject.clone()),
            kind: "gate.result".into(),
            actor,
            fields,
            evidence,
            expected_subject: request.expected_subject,
            idempotency_key: None,
        })
        .map_err(ApiError::bad)?;
    if request.decision == "rejected" {
        if let Some(owner) = review_request
            .as_ref()
            .and_then(|claim| claim.body.pointer("/fields/owner"))
            .and_then(Value::as_str)
        {
            if owner.starts_with("step-run/") {
                let step = state.store.step_run(owner).map_err(ApiError::internal)?;
                let claimant = if let Some(step) = step {
                    if step.claimant.is_some() || step.carried_claimant.is_some() {
                        step.claimant.or(step.carried_claimant)
                    } else {
                        state
                            .store
                            .claims_for(owner, Some("work.claimed"))
                            .map_err(ApiError::internal)?
                            .into_iter()
                            .rev()
                            .find(|claim| {
                                claim
                                    .body
                                    .pointer("/fields/attempt")
                                    .and_then(Value::as_u64)
                                    == Some(u64::from(step.attempt))
                            })
                            .and_then(|claim| {
                                claim
                                    .body
                                    .pointer("/fields/claimant")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned)
                            })
                    }
                } else {
                    None
                };
                if let Some(claimant) = claimant {
                    let reason = response
                        .body
                        .pointer("/fields/reason")
                        .and_then(Value::as_str)
                        .unwrap_or("No reason provided.");
                    send_planning_message(
                        &state,
                        &format!("review-rejected:{}:{claimant}", response.id),
                        response.actor.as_deref().unwrap_or("daemon/runtime"),
                        &claimant,
                        &format!("Your work on `{owner}` was rejected. Reason: {reason}"),
                        "Human review rejected",
                    )?;
                }
            }
        }
    }
    signal_changed(&state);
    Ok(Json(response))
}

#[derive(Debug, Default, Deserialize)]
pub struct LaneListQuery {
    #[serde(default)]
    all: bool,
    #[serde(default)]
    run: Option<String>,
}

/// This node's key for sekrets attestations, for `st sekrets enable` to register.
pub async fn sekrets_node(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let (node, key) =
        crate::sekrets_daemon::node_key(&state.store, &state.node).map_err(ApiError::bad)?;
    Ok(Json(json!({
        "node": node,
        "key": key,
        "person": crate::sekrets_daemon::PERSON.get(),
    })))
}

#[derive(Deserialize)]
pub struct SekretsAttestRequest {
    nonce: String,
}

/// Sign which seat the calling process is, for the sekrets gateway. Only over the local socket,
/// where the kernel names the caller.
pub async fn sekrets_attest(
    State(state): State<AppState>,
    peer: Option<axum::Extension<LocalPeer>>,
    Json(request): Json<SekretsAttestRequest>,
) -> Result<Json<crate::sekrets::protocol::Attestation>, ApiError> {
    let Some(axum::Extension(peer)) = peer else {
        return Err(ApiError::bad(St3Error::new(
            "sekrets-attestation-refused",
            "attestations are only given over the local socket",
        )));
    };
    blocking_external(move || {
        crate::sekrets_daemon::attest(
            &state.store,
            &state.node,
            &state.pty_root,
            peer.pid,
            peer.ancestor.as_deref(),
            &request.nonce,
        )
    })
    .await?
    .map(Json)
    .map_err(ApiError::bad)
}

pub async fn list_lanes(
    State(state): State<AppState>,
    Query(query): Query<LaneListQuery>,
) -> Result<Json<Vec<crate::model::LaneView>>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || match query.run.as_deref() {
        Some(run) => store.lanes_for_run(run),
        None => store.lanes(query.all),
    })
    .await
    .map(Json)
}

/// One lane by subject, `RUN/NAME`, run, mission, or unique name.
pub async fn get_lane(
    State(state): State<AppState>,
    AxumPath(lane): AxumPath<String>,
) -> Result<Json<crate::model::LaneView>, ApiError> {
    let store = state.store.clone();
    blocking_action(move || {
        let subject = store.resolve_lane(&lane)?;
        store
            .lane(&subject)
            .map_err(|error| St3Error::new("internal", error.to_string()))?
            .ok_or_else(|| St3Error::new("lane-not-found", format!("no lane `{subject}`")))
    })
    .await
    .map(Json)
}

/// Join, leave, move, mark, or approve one lane entry as a person or an agent. A harness can
/// only act as its own seat; `guard_bound_request` checks that before this runs.
pub async fn change_lane(
    State(state): State<AppState>,
    Json(mut request): Json<crate::model::LaneChangeRequest>,
) -> Result<Json<crate::model::LaneChangeResponse>, ApiError> {
    request.actor = match request.actor.trim() {
        actor if actor.starts_with("person/") => actor.to_owned(),
        actor => normalized_agent_actor(actor).ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "invalid-lane-actor",
                "a lane change needs a person or agent actor",
            ))
        })?,
    };
    let store = state.store.clone();
    let response = blocking_action(move || store.change_lane(&request)).await?;
    if response.claim.is_some() {
        signal_changed(&state);
    }
    Ok(Json(response))
}
