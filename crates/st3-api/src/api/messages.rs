use super::*;

pub async fn message_delivery(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let subject = if subject.starts_with("message/") {
        subject
    } else {
        format!("message/{subject}")
    };
    blocking_api(move || {
        let message = state.store.message(&subject).map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found(format!("message `{subject}` does not exist")))?;
        let claims = state.store.claims_for(&subject, Some("message.sent")).map_err(ApiError::internal)?;
        let sent_at = claims.first().map(|claim| claim.accepted_at_unix_ms).unwrap_or_default();
        Ok(Json(json!({ "id": subject, "from": message.from, "to": message.to,
            "delivery": message_delivery_for(&state.store, &message, sent_at, client_now_ms()).map_err(ApiError::internal)? })))
    }).await
}

#[derive(Deserialize)]
pub struct MessageKeyQuery {
    key: String,
}

/// The message a send's idempotency key landed as, so a client whose send went unanswered can
/// tell whether it was sent before it sends again. Only a message send or reply answers; any
/// other key is `message-not-sent`.
pub async fn message_by_key(
    State(state): State<AppState>,
    Query(query): Query<MessageKeyQuery>,
) -> Result<Json<MessageSendReceipt>, ApiError> {
    blocking_api(move || {
        let not_sent = || ApiError {
            status: StatusCode::NOT_FOUND,
            code: "message-not-sent".into(),
            message: format!("no message was sent with idempotency key `{}`", query.key),
            details: Box::default(),
        };
        let claim = state
            .store
            .operation_claim(&query.key)
            .map_err(ApiError::internal)?
            .filter(|claim| claim.kind == "message.sent")
            .ok_or_else(not_sent)?;
        let message = state
            .store
            .message(&claim.subject)
            .map_err(ApiError::internal)?
            .ok_or_else(not_sent)?;
        Ok(Json(MessageSendReceipt {
            kind: Some(
                if crate::silent::is_held(&message) {
                    "silent"
                } else {
                    "wake"
                }
                .into(),
            ),
            message,
            idempotency_key: query.key.clone(),
            already_sent: true,
            sent_at: Some(client_timestamp(claim.accepted_at_unix_ms)),
        }))
    })
    .await
}

#[derive(Deserialize)]
pub struct SubscriptionRequestQuery {
    subscription: String,
}

pub async fn list_subscription_requests(
    State(state): State<AppState>,
    Query(query): Query<SubscriptionRequestQuery>,
) -> Result<Json<Vec<crate::model::SubscriptionRequestView>>, ApiError> {
    let subscription = if query.subscription.starts_with("subscription/") {
        query.subscription
    } else {
        format!("subscription/{}", query.subscription)
    };
    let store = state.store.clone();
    blocking_store(move || store.subscription_requests(&subscription))
        .await
        .map(Json)
}

/// Release a held subscription request or cancel an open one as a person.
pub async fn decide_subscription_request(
    State(state): State<AppState>,
    AxumPath((decision, request)): AxumPath<(String, String)>,
    Json(input): Json<crate::model::SubscriptionRequestDecision>,
) -> Result<Json<crate::model::SubscriptionRequestView>, ApiError> {
    let store = state.store.clone();
    let response =
        blocking_action(move || store.decide_subscription_request(&request, &decision, &input))
            .await?;
    signal_changed(&state);
    Ok(Json(response))
}

pub async fn send_message(
    State(state): State<AppState>,
    Json(request): Json<MessageSendRequest>,
) -> Result<Json<MessageSendReceipt>, ApiError> {
    blocking_api(move || accept_message_receipt(&state, request, None, None).map(Json)).await
}

#[derive(Deserialize)]
pub struct MessagesPageQuery {
    to: Option<String>,
    from: Option<String>,
    subject: Option<String>,
    subject_prefix: Option<String>,
    #[serde(default)]
    include_closed: bool,
    cursor: Option<String>,
    limit: Option<usize>,
    /// A seat delivery process's report; see [`delivery_presence`].
    delivery: Option<String>,
}

impl MessagesPageQuery {
    fn validate_filters(&self) -> Result<(), ApiError> {
        if (self.subject.is_some() || self.subject_prefix.is_some()) && self.from.is_none() {
            return Err(ApiError::bad(St3Error::new(
                "validation-failed",
                "subject filters need a sender filter",
            )));
        }
        if self.subject.is_some() && self.subject_prefix.is_some() {
            return Err(ApiError::bad(St3Error::new(
                "validation-failed",
                "subject and subject_prefix are mutually exclusive",
            )));
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct MessagesPageCursor {
    after: u64,
    through: u64,
    to: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    subject_prefix: Option<String>,
    include_closed: bool,
    limit: usize,
}

impl MessagesPageCursor {
    fn validate(
        &self,
        query: &MessagesPageQuery,
        to: Option<&str>,
        from: Option<&str>,
        limit: usize,
    ) -> Result<(), ApiError> {
        let matches = self.to.as_deref() == to
            && self.from.as_deref() == from
            && self.subject == query.subject
            && self.subject_prefix == query.subject_prefix
            && self.include_closed == query.include_closed
            && self.limit == limit;
        if !matches {
            return Err(ApiError::bad(St3Error::new(
                "validation-failed",
                "message cursor filters changed",
            )));
        }
        Ok(())
    }
}

pub async fn list_messages_page(
    State(state): State<AppState>,
    Query(query): Query<MessagesPageQuery>,
    peer: Option<Extension<NativeDeliveryPeer>>,
) -> Result<Json<MessagePage>, ApiError> {
    let limit = query.limit.unwrap_or(100).clamp(1, 200);
    let to = query.to.as_deref().map(normalize_message_party);
    let from = query.from.as_deref().map(normalize_message_party);
    query.validate_filters()?;
    if let (Some(to), Some(report)) = (to.as_deref(), query.delivery.as_deref()) {
        delivery_presence::record(to, report);
    } else {
        record_legacy_poll(
            peer.as_ref().map(|Extension(peer)| peer),
            to.as_deref(),
            query.include_closed,
        );
    }
    let cursor = query
        .cursor
        .as_deref()
        .map(|encoded| {
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded.strip_prefix("messages/").unwrap_or(""))
                .map_err(|_| {
                    ApiError::bad(St3Error::new("validation-failed", "invalid message cursor"))
                })?;
            let cursor: MessagesPageCursor = serde_json::from_slice(&bytes).map_err(|_| {
                ApiError::bad(St3Error::new("validation-failed", "invalid message cursor"))
            })?;
            cursor.validate(&query, to.as_deref(), from.as_deref(), limit)?;
            Ok(cursor)
        })
        .transpose()?;
    let through = match &cursor {
        Some(cursor) => cursor.through,
        None => state.store.index().map_err(ApiError::internal)?,
    };
    let after = cursor.as_ref().map(|cursor| cursor.after);
    let subject = query.subject.clone();
    let subject_prefix = query.subject_prefix.clone();
    let store = state.store.clone();
    let (items, next_after) = blocking_store(move || {
        let native = !query.include_closed
            && peer
                .as_ref()
                .is_some_and(|peer| to.as_deref() == Some(peer.0.agent.as_str()));
        let filtered = from.is_some() || subject.is_some() || subject_prefix.is_some();
        let (mut items, next_after) = if filtered {
            store.messages_page_filtered(
                to.as_deref(),
                from.as_deref(),
                subject.as_deref(),
                subject_prefix.as_deref(),
                query.include_closed,
                after,
                through,
                limit,
            )?
        } else if native {
            store.messages_page_for_delivery(to.as_deref().unwrap(), after, through, limit)?
        } else {
            store.messages_page(to.as_deref(), query.include_closed, after, through, limit)?
        };
        mailbox::hold_pre_boot_mail(
            &store, peer.as_ref().map(|peer| &peer.0), to.as_deref(), &mut items,
        )?;
        Ok((items, next_after))
    })
    .await?;
    let next_cursor = next_after
        .map(|after| {
            let cursor = MessagesPageCursor {
                after,
                through,
                to: query.to.as_deref().map(normalize_message_party),
                from: query.from.as_deref().map(normalize_message_party),
                subject: query.subject.clone(),
                subject_prefix: query.subject_prefix.clone(),
                include_closed: query.include_closed,
                limit,
            };
            serde_json::to_vec(&cursor).map(|bytes| {
                format!(
                    "messages/{}",
                    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
                )
            })
        })
        .transpose()
        .map_err(ApiError::internal)?;
    Ok(Json(MessagePage {
        items,
        has_more: next_cursor.is_some(),
        next_cursor,
        limit,
    }))
}

pub async fn list_messages(
    State(state): State<AppState>,
    Query(query): Query<MessagesQuery>,
    peer: Option<Extension<NativeDeliveryPeer>>,
) -> Result<Json<Vec<MessageView>>, ApiError> {
    let recipient = query.to.as_deref().map(normalize_message_party);
    record_legacy_poll(
        peer.as_ref().map(|Extension(peer)| peer),
        recipient.as_deref(),
        query.include_closed,
    );
    let store = state.store.clone();
    blocking_store(move || {
        let native = !query.include_closed
            && peer
                .as_ref()
                .is_some_and(|peer| recipient.as_deref() == Some(peer.0.agent.as_str()));
        let mut messages = if native {
            store.messages_for_delivery_through(recipient.as_deref().unwrap(), store.index()?)?
        } else {
            store.messages(recipient.as_deref(), query.include_closed)?
        };
        mailbox::hold_pre_boot_mail(
            &store, peer.as_ref().map(|peer| &peer.0), recipient.as_deref(), &mut messages,
        )?;
        Ok(messages)
    })
    .await
    .map(Json)
}

pub async fn read_message(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
) -> Result<Json<MessageRead>, ApiError> {
    let subject = if subject.starts_with("message/") {
        subject
    } else {
        format!("message/{subject}")
    };
    let store = state.store.clone();
    let lookup = subject.clone();
    let message = blocking_store(move || store.message(&lookup))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("message `{subject}` does not exist")))?;
    let provenance = {
        let state = state.clone();
        let lookup = message.subject.clone();
        blocking_store(move || {
            Ok(state
                .store
                .latest_claim(&lookup, Some("message.sent"))?
                .and_then(|claim| client_v0::message_provenance(&state, &claim)))
        })
        .await?
    };
    Ok(Json(MessageRead {
        message,
        provenance,
    }))
}

pub async fn post_message_claim(
    State(state): State<AppState>,
    AxumPath(message_id): AxumPath<String>,
    Json(request): Json<MessageLifecycleRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    let subject = message_subject(&message_id);
    let kind = match request.lifecycle.as_str() {
        "staged" => "message.staged",
        "delivered" => "message.delivered",
        "read" => "message.read",
        "closed" => "message.closed",
        other => {
            return Err(ApiError::bad(St3Error::new(
                "invalid-message-lifecycle",
                format!("message lifecycle `{other}` is not registered"),
            )));
        }
    };
    let store = state.store.clone();
    let record = blocking_api(move || {
        // One message, not every message the store has ever held: a lifecycle post read and folded
        // the whole mailbox, a quarter second on a busy host's store, while holding up the next write.
        let message = store
            .message(&subject)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found(format!("message `{subject}` does not exist")))?;
        let actor = request.actor.ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "missing-message-actor",
                "message lifecycle transitions require the recipient actor",
            ))
        })?;
        let actor = normalize_message_party(&actor);
        if actor != message.to
            && !request
                .delegation
                .as_ref()
                .is_some_and(|proof| proof.person == message.to && actor.starts_with("agent/"))
        {
            return Err(ApiError::bad(St3Error::new(
                "wrong-message-recipient",
                format!(
                    "message `{subject}` belongs to `{}`, not `{actor}`",
                    message.to
                ),
            )));
        }
        let mut fields =
            BTreeMap::from([("status".into(), Value::String(request.lifecycle.clone()))]);
        let mut evidence = request.evidence;
        if let Some(proof) = &request.delegation {
            crate::store::delegation::add_fields(&mut fields, proof);
            store.delegation_evidence(&mut evidence, proof).map_err(ApiError::bad)?;
        }
        if kind == "message.staged" {
            fields.insert("recipient".into(), Value::String(actor.clone()));
            if let Some(transport) = request.transport {
                fields.insert("transport".into(), Value::String(transport));
            }
            if let Some(runtime_id) = request.runtime_id {
                fields.insert("runtime_id".into(), Value::String(runtime_id));
            }
        }
        let (record, appended) = store
            .append_claim_outcome(&ClaimInput {
                subject,
                kind: kind.into(),
                actor: Some(actor),
                fields,
                evidence,
                expected_subject: request.expected_subject,
                idempotency_key: Some(request.idempotency_key),
            })
            .map_err(ApiError::bad)?;
        Ok((record, appended, is_work_wake(&message.tags)))
    })
    .await?;
    let (record, appended, work_wake) = record;
    // An idempotent repeat or an already-settled transition changes nothing a reader can see.
    // Signalling it anyway re-reads every subscribed seat's mailbox (#1085).
    if appended {
        signal_message_changed(&state, kind, work_wake);
    }
    Ok(Json(record))
}

#[cfg(test)]
mod page_cursor_tests {
    use super::*;

    #[test]
    fn message_cursor_rejects_changed_filters() {
        let cursor: MessagesPageCursor = serde_json::from_value(json!({
            "after": 10, "through": 100, "to": "agent/reviewer",
            "from": "agent/ada", "subject": "review", "subject_prefix": null,
            "include_closed": true, "limit": 2
        }))
        .unwrap();
        let binding = json!({
            "to": "agent/reviewer", "from": "agent/ada", "subject": "review",
            "include_closed": true, "limit": 2
        });
        let validate = |value: Value| {
            let query: MessagesPageQuery = serde_json::from_value(value).unwrap();
            let to = query.to.as_deref().map(normalize_message_party);
            let from = query.from.as_deref().map(normalize_message_party);
            cursor.validate(&query, to.as_deref(), from.as_deref(), query.limit.unwrap())
        };
        assert!(validate(binding.clone()).is_ok());
        for (field, value) in [
            ("to", json!("agent/operator")),
            ("from", json!("agent/alex")),
            ("from", Value::Null),
            ("subject", json!("other")),
            ("subject_prefix", json!("rev")),
            ("include_closed", json!(false)),
            ("limit", json!(3)),
        ] {
            let mut changed = binding.clone();
            changed[field] = value;
            let error = validate(changed).expect_err("accepted changed cursor filters");
            assert_eq!(error.code, "validation-failed");
            assert_eq!(error.message, "message cursor filters changed");
        }
    }

    #[test]
    fn message_cursor_prefix_binding_and_legacy_defaults() {
        let mut cursor: MessagesPageCursor = serde_json::from_value(json!({
            "after": 10, "through": 100, "to": null,
            "include_closed": true, "limit": 2
        }))
        .unwrap();
        let mut query: MessagesPageQuery = serde_json::from_value(json!({
            "include_closed": true, "limit": 2
        }))
        .unwrap();
        assert!(cursor.validate(&query, None, None, 2).is_ok());
        query.subject_prefix = Some("literal_%".into());
        assert!(cursor.validate(&query, None, None, 2).is_err());
        cursor.subject_prefix = query.subject_prefix.clone();
        assert!(cursor.validate(&query, None, None, 2).is_ok());
        query.subject_prefix = Some("literal".into());
        assert!(cursor.validate(&query, None, None, 2).is_err());
    }
}

#[cfg(test)]
mod subject_filter_tests {
    use super::*;

    #[test]
    fn subject_filters_require_sender_before_store_access() {
        for field in ["subject", "subject_prefix"] {
            let query: MessagesPageQuery =
                serde_json::from_value(json!({(field): "review", "to": "agent/reviewer"}))
                    .unwrap();
            let error = query.validate_filters().expect_err("accepted subject-only query");
            assert_eq!(error.code, "validation-failed");
            assert_eq!(error.message, "subject filters need a sender filter");
        }
    }
}
