//! Server-established assistant provenance, checked in the mutation's writer transaction.
use super::*;

#[derive(Clone, Debug)]
pub struct AssistantWriteAuthorization {
    pub grant_subject: String,
    pub actor: String,
    pub owner: String,
    pub policy_revision: u64,
    pub scope: String,
    pub recipient: Option<String>,
    pub approval_subject: Option<String>,
}

/// The connection must be the writer transaction that commits the protected mutation.
pub fn authorize(connection: &Connection, origin: &str, authorization: &AssistantWriteAuthorization) -> Result<(), St3Error> {
    authorized_grant(connection, origin, authorization).map(|_| ())
}

pub fn authorize_grant(connection: &Connection, origin: &str, authorization: &AssistantWriteAuthorization) -> Result<String, St3Error> {
    let granted = authorized_grant(connection, origin, authorization)?;
    granted.body["fields"]["grant_id"].as_str().filter(|id| id.starts_with("assistant-grant/") && id.len() > "assistant-grant/".len())
        .map(str::to_owned).ok_or_else(|| St3Error::new("forbidden", "assistant grant identity is absent"))
}

fn authorized_grant(connection: &Connection, origin: &str, authorization: &AssistantWriteAuthorization) -> Result<ClaimRecord, St3Error> {
    let denied = || St3Error::new("forbidden", "assistant write authorization changed or expired before commit");
    let claim = |kind: &str| -> Result<Option<ClaimRecord>, St3Error> {
        connection.query_row(&latest_claim_of_kind_query(),
            params![authorization.grant_subject,kind],claim_from_row).optional().map_err(internal)
    };
    let granted = claim("custom.client.assistant.v1.granted")?.ok_or_else(denied)?;
    let policy_claim = claim("custom.client.assistant.v1.policy")?;
    let policy = policy_claim.as_ref().unwrap_or(&granted);
    if granted.origin != origin || policy.origin != origin || claim("custom.client.assistant.v1.revoked")?.is_some() {
        return Err(denied());
    }
    let fields = &policy.body["fields"];
    if fields["principal"] != "assistant/chatgpt" || fields["session_actor"] != authorization.actor
        || fields["issuer"] != authorization.owner || fields["disclosure_consent"] != true
        || fields["policy_revision"].as_u64() != Some(authorization.policy_revision)
        || fields["expires_at_unix_ms"].as_u64().map(u128::from).is_none_or(|expiry| expiry <= now_ms())
        || !fields["scopes"].as_array().is_some_and(|scopes| scopes.iter().any(|scope| scope.as_str() == Some(&authorization.scope)))
    { return Err(denied()); }
    if let Some(recipient) = &authorization.recipient
        && !fields["recipient_allowlist"].as_array().is_some_and(|recipients| recipients.iter().any(|value| value.as_str() == Some(recipient)))
    {
        return Err(denied());
    }
    if let Some(subject) = &authorization.approval_subject {
        let approved: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM claims WHERE subject=?1 AND kind='work.person-done' AND actor=?2 AND origin=?3 AND json_extract(body,'$.fields.answer.id')='approve' AND json_extract(body,'$.fields.acted_for') IS NULL)",
            params![subject,authorization.owner,origin],|row| row.get(0)).map_err(internal)?;
        if !approved { return Err(denied()); }
    }
    Ok(granted)
}

/// Local generic writers cannot assert assistant identity or server-owned provenance.
pub fn refuse_unverified(kind: &str, actor: Option<&str>, from: Option<&str>, has_provenance: bool) -> Result<(), St3Error> {
    if kind == "message.sent"
        && (actor.is_some_and(|actor| actor.starts_with("assistant/"))
            || from.is_some_and(|from| from.starts_with("assistant/"))
            || has_provenance)
    {
        return Err(St3Error::new("assistant-send-refused", "assistant messages require the authorized native assistant path"));
    }
    Ok(())
}

/// Replication already authenticates the originating writer. Do not reauthorize an old
/// committed message against a receiver's missing, expired or revoked local grant.
pub fn validate_replicated(kind: &str, actor: Option<&str>, fields: &Value) -> Result<(), St3Error> {
    if kind != "message.sent" { return Ok(()); }
    if ["sender_class", "assistant_grant_id", "assistant_owner", "assistant_session_actor"].iter().all(|key| fields.get(*key).is_none()) {
        return refuse_unverified(kind, actor, fields["from"].as_str(), false);
    }
    if fields["sender_class"] != "external-assistant"
        || !fields["assistant_grant_id"].as_str().is_some_and(|id| id.starts_with("assistant-grant/") && id.len() > "assistant-grant/".len())
        || actor != Some("assistant/chatgpt") || fields["from"] != "assistant/chatgpt"
        || !fields["assistant_owner"].as_str().is_some_and(|owner| owner.starts_with("person/") && owner.matches('/').count() == 1)
        || !fields["assistant_session_actor"].as_str().is_some_and(|session| {
            session.strip_prefix("assistant/chatgpt/session/") == fields["assistant_grant_id"].as_str()
        })
    {
        return Err(St3Error::new("assistant-send-refused", "replicated assistant provenance is inconsistent"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replicated_assistant_sender_keeps_owner_and_session_as_provenance_only() {
        let fields = json!({
            "from":"assistant/chatgpt", "sender_class":"external-assistant",
            "assistant_grant_id":"assistant-grant/one", "assistant_owner":"person/ada",
            "assistant_session_actor":"assistant/chatgpt/session/assistant-grant/one",
        });
        validate_replicated("message.sent", Some("assistant/chatgpt"), &fields).unwrap();
        for (actor, patch) in [
            ("person/ada", json!({"from":"person/ada"})),
            ("assistant/chatgpt", json!({"assistant_owner":null})),
            ("assistant/chatgpt", json!({"assistant_session_actor":"assistant/chatgpt/session/assistant-grant/other"})),
        ] {
            let mut forged = fields.clone();
            forged.as_object_mut().unwrap().extend(patch.as_object().unwrap().clone());
            assert!(validate_replicated("message.sent", Some(actor), &forged).is_err());
        }
    }
}
