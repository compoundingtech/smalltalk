use super::*;

pub(super) const THRESHOLD_MS: u64 = 3_600_000;
pub(super) const CLEANUP_COMMAND: &str = "st conversations cleanup --all --older-than 1h";

fn overdue(
    store: &Store,
    now: u128,
    threshold: u64,
    to: Option<&str>,
    resume_archives: bool,
) -> anyhow::Result<Vec<MessageView>> {
    let mut result = Vec::new();
    // Include retained mail for retired seats: those are precisely the messages a current
    // agents projection would hide. Inspection never changes their lifecycle.
    for message in store.messages(to, false)? {
        if !matches!(message.status.as_str(), "sent" | "staged" | "delivered")
            && !(resume_archives && unfinished_archive(store, &message)?)
        {
            continue;
        }
        let Some(sent) = store.latest_claim(&message.subject, Some("message.sent"))? else {
            continue;
        };
        if now.saturating_sub(sent.accepted_at_unix_ms) > u128::from(threshold) {
            result.push(message);
        }
    }
    Ok(result)
}

fn archive_key(subject: &str, lifecycle: &str) -> String {
    format!("mail-backlog-archive:{subject}:{lifecycle}")
}

fn unfinished_archive(store: &Store, message: &MessageView) -> anyhow::Result<bool> {
    Ok(matches!(message.status.as_str(), "delivered" | "read")
        && store
            .operation_claim(&archive_key(&message.subject, "delivered"))?
            .is_some()
        || message.status == "read"
            && store
                .operation_claim(&archive_key(&message.subject, "read"))?
                .is_some())
}

pub(super) fn report(store: &Store, now: u128) -> anyhow::Result<st3_client::MailBacklog> {
    Ok(st3_client::MailBacklog {
        count: overdue(store, now, THRESHOLD_MS, None, false)?.len() as u64,
        threshold_ms: THRESHOLD_MS,
        cleanup_command: CLEANUP_COMMAND.into(),
    })
}

pub(super) async fn get(
    State(state): State<AppState>,
    Extension(session): Extension<client_v0::ClientSession>,
) -> Result<Json<st3_client::MailBacklog>, ApiError> {
    client_v0::require_scope(&session, "read.projections")?;
    blocking_store(move || report(&state.store, client_now_ms()))
        .await
        .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CleanupRequest {
    pub older_than_ms: u64,
    pub to: Option<String>,
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub dry_run: bool,
}

pub(super) async fn cleanup(
    State(state): State<AppState>,
    Json(request): Json<CleanupRequest>,
) -> Result<Json<Value>, ApiError> {
    if request.older_than_ms == 0 || request.all == request.to.is_some() {
        return Err(ApiError::bad(St3Error::new(
            "invalid-mail-cleanup",
            "choose a mailbox or all mailboxes and a positive age threshold",
        )));
    }
    let dry_run = request.dry_run;
    let store = state.store.clone();
    let result = blocking_api(move || {
        let to = request.to.as_deref().map(normalize_message_party);
        let messages = overdue(
            &store,
            client_now_ms(),
            request.older_than_ms,
            to.as_deref(),
            true,
        )
        .map_err(ApiError::internal)?;
        let mut archived = Vec::new();
        for message in messages {
            if !request.dry_run {
                let mut head = store
                    .latest_claim(&message.subject, None)
                    .map_err(ApiError::internal)?;
                // Fence against delivery racing the selection. Explicit archival follows the
                // same recipient lifecycle as `conversations archive`, with keys identifying
                // manual backlog archival and evidence linking each preceding claim.
                let mut current = store
                    .message(&message.subject)
                    .map_err(ApiError::internal)?
                    .unwrap();
                if !matches!(current.status.as_str(), "sent" | "staged" | "delivered")
                    && !unfinished_archive(&store, &current).map_err(ApiError::internal)?
                {
                    continue;
                }
                for lifecycle in ["delivered", "read", "closed"] {
                    let needed = match lifecycle {
                        "delivered" => matches!(current.status.as_str(), "sent" | "staged"),
                        "read" => current.status == "delivered",
                        "closed" => current.status == "read",
                        _ => unreachable!(),
                    };
                    if !needed {
                        continue;
                    }
                    let record = store
                        .append_claim(&ClaimInput {
                            subject: message.subject.clone(),
                            kind: format!("message.{lifecycle}"),
                            actor: Some(message.to.clone()),
                            fields: BTreeMap::from([("status".into(), json!(lifecycle))]),
                            evidence: head
                                .as_ref()
                                .map(|claim| vec![claim.id.clone()])
                                .unwrap_or_default(),
                            expected_subject: Some(head.as_ref().map(|claim| claim.id.clone())),
                            idempotency_key: Some(archive_key(&message.subject, lifecycle)),
                        })
                        .map_err(ApiError::bad)?;
                    head = Some(record);
                    current.status = lifecycle.into();
                }
            }
            archived.push(message.subject);
        }
        Ok(json!({ "count": archived.len(), "messages": archived, "dry_run": request.dry_run }))
    })
    .await?;
    if !dry_run {
        signal_message_changed(&state, "message.closed", false);
    }
    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn backlog_counts_unread_retired_mail_and_cleanup_keeps_fresh_and_read_mail() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let now = client_now_ms();
        for (id, recipient, phase, time) in [
            ("old-sent", "agent/example/retired", "sent", now - 7_200_000),
            (
                "old-staged",
                "agent/example/other",
                "staged",
                now - 7_200_000,
            ),
            (
                "accepted",
                "agent/example/retired",
                "delivered",
                now - 7_200_000,
            ),
            ("read", "agent/example/retired", "read", now - 7_200_000),
            (
                "interrupted",
                "agent/example/retired",
                "delivered",
                now - 7_200_000,
            ),
            (
                "interrupted-read",
                "agent/example/retired",
                "read",
                now - 7_200_000,
            ),
            ("closed", "agent/example/retired", "closed", now - 7_200_000),
            ("fresh", "agent/example/retired", "sent", now),
            ("fresh-delivered", "agent/example/retired", "delivered", now),
        ] {
            state.store.set_write_clock_at(time).unwrap();
            let subject = format!("message/{id}");
            state
                .store
                .append_claim(&ClaimInput {
                    subject: subject.clone(),
                    kind: "message.sent".into(),
                    actor: Some("person/operator".into()),
                    fields: BTreeMap::from([
                        ("from".into(), json!("person/operator")),
                        ("to".into(), json!(recipient)),
                        ("status".into(), json!("sent")),
                        ("content".into(), json!("retained mail")),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: Some(id.into()),
                })
                .unwrap();
            let phases: &[&str] = match phase {
                "staged" => &["staged"],
                "delivered" => &["delivered"],
                "read" => &["delivered", "read"],
                "closed" => &["delivered", "read", "closed"],
                _ => &[],
            };
            for phase in phases {
                state
                    .store
                    .append_claim(&ClaimInput {
                        subject: subject.clone(),
                        kind: format!("message.{phase}"),
                        actor: Some(recipient.into()),
                        fields: BTreeMap::from([("status".into(), json!(phase))]),
                        evidence: vec![],
                        expected_subject: None,
                        idempotency_key: (id == "interrupted"
                            || id == "interrupted-read" && phase == &"read")
                            .then(|| archive_key(&subject, phase)),
                    })
                    .unwrap();
            }
        }
        state.store.set_write_clock_at(now).unwrap();
        assert_eq!(report(&state.store, now).unwrap().count, 4);
        let Json(doctor) = doctor_report(&state).unwrap();
        let warning = doctor
            .checks
            .iter()
            .find(|check| check.name == "mail-backlog")
            .unwrap();
        assert_eq!(warning.status, "warn");
        assert!(warning.message.starts_with("4 unread"));
        assert!(warning.message.contains(CLEANUP_COMMAND));
        let request = |dry_run, to: Option<String>| CleanupRequest {
            older_than_ms: THRESHOLD_MS,
            all: to.is_none(),
            to,
            dry_run,
        };
        let Json(preview) = cleanup(State(state.clone()), Json(request(true, None)))
            .await
            .unwrap();
        assert_eq!(preview["count"], 5);
        assert_eq!(report(&state.store, now).unwrap().count, 4);
        let Json(selected) = cleanup(
            State(state.clone()),
            Json(request(false, Some("agent/example/retired".into()))),
        )
        .await
        .unwrap();
        assert_eq!(selected["count"], 4);
        let Json(all) = cleanup(State(state.clone()), Json(request(false, None)))
            .await
            .unwrap();
        assert_eq!(all["count"], 1);
        let Json(repeat) = cleanup(State(state.clone()), Json(request(false, None)))
            .await
            .unwrap();
        assert_eq!(repeat["count"], 0);
        assert_eq!(report(&state.store, now).unwrap().count, 0);
        let Json(doctor) = doctor_report(&state).unwrap();
        assert_eq!(
            doctor
                .checks
                .iter()
                .find(|check| check.name == "mail-backlog")
                .unwrap()
                .status,
            "pass"
        );
        for (id, expected) in [
            ("old-sent", "closed"),
            ("old-staged", "closed"),
            ("fresh", "sent"),
            ("accepted", "closed"),
            ("fresh-delivered", "delivered"),
            ("closed", "closed"),
            ("interrupted-read", "closed"),
            ("read", "read"),
            ("interrupted", "closed"),
        ] {
            assert_eq!(
                state
                    .store
                    .message(&format!("message/{id}"))
                    .unwrap()
                    .unwrap()
                    .status,
                expected
            );
        }
        for id in [
            "old-sent",
            "old-staged",
            "accepted",
            "interrupted",
            "interrupted-read",
        ] {
            let claims = state
                .store
                .claims_for(&format!("message/{id}"), Some("message.closed"))
                .unwrap();
            assert_eq!(claims.len(), 1);
            assert_eq!(
                claims[0].actor.as_deref(),
                Some(if id == "old-staged" {
                    "agent/example/other"
                } else {
                    "agent/example/retired"
                })
            );
        }
        assert_eq!(
            state
                .store
                .claims_for("message/interrupted", Some("message.delivered"))
                .unwrap()
                .len(),
            1
        );
    }
}
