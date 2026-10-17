//! Long message bodies in the API: the whole text of a message whose claim holds a preview.
//! See [`crate::message_body`] for where the text lives and why.

use super::client_blobs::{blob_error, person_reads_message, read_error};
use super::client_v0::{ClientSession, require_scope};
use super::*;
use crate::peer::{ClientReadOperation, ClientReadRequest};
use crate::message_body::BodyAttachment as _;

/// The whole text of a message: its own content when it has no separate body, else the body
/// read from its owner. This member's own file is read directly; another member's is asked for
/// over the peer route, chunk by chunk, and kept nowhere.
async fn whole_text(
    state: &AppState,
    session: &ClientSession,
    message: &MessageView,
) -> Result<String, ApiError> {
    let Some(body) = message.body_attachment() else {
        return Ok(message.content.clone());
    };
    let gone = |text: String| blob_error(St3Error::new("blob-not-found", text));
    let bytes = if body.origin == client_host_id(&state.node) {
        let bodies = message_body::bodies(&state.state_dir);
        match bodies
            .read(&message.from, &message.subject)
            .map_err(ApiError::internal)?
        {
            Some(bytes) => bytes,
            None if bodies.is_expired(&message.from, &message.subject) => {
                return Err(expired_error(&body.origin));
            }
            None => {
                return Err(gone(format!(
                    "the full text of this message is no longer on {}",
                    body.origin
                )));
            }
        }
    } else {
        let relay = state
            .client_relay
            .as_ref()
            .ok_or_else(|| remote_unavailable_for_owner(state, &body.origin))?;
        // The most a body is read in is a few chunks, and the owner has half a minute to give
        // them, so a reader never waits on an owner that streams without end.
        let fetched = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let mut bytes = Vec::new();
            for _ in 0..=(message_body::MAX_BYTES / crate::blobs::CHUNK_BYTES + 1) {
                let value = relay
                    .read(
                        &body.origin,
                        &ClientReadRequest {
                            authority_actor: session.authority_actor.clone(),
                            request: ClientReadOperation::Blob {
                                sha256: body.sha256.clone(),
                                message: message.subject.clone(),
                                offset: bytes.len() as u64,
                            },
                            relay: None,
                        },
                    )
                    .await
                    .map_err(|error| read_error(state, &body.origin, error))?;
                let size = value["size"].as_u64().unwrap_or(u64::MAX);
                let chunk = value["data"]
                    .as_str()
                    .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
                    .ok_or_else(|| remote_unavailable_for_owner(state, &body.origin))?;
                if size > message_body::MAX_BYTES as u64
                    || chunk.is_empty() && (bytes.len() as u64) < size
                    || bytes.len() + chunk.len() > message_body::MAX_BYTES
                {
                    return Err(remote_unavailable_for_owner(state, &body.origin));
                }
                bytes.extend_from_slice(&chunk);
                if bytes.len() as u64 >= size {
                    return Ok(bytes);
                }
            }
            Err(remote_unavailable_for_owner(state, &body.origin))
        })
        .await;
        match fetched {
            Ok(result) => result?,
            Err(_) => return Err(remote_unavailable_for_owner(state, &body.origin)),
        }
    };
    if hex::encode(Sha256::digest(&bytes)) != body.sha256 {
        return Err(gone(format!(
            "the text {} sent does not match this message's hash",
            body.origin
        )));
    }
    String::from_utf8(bytes).map_err(|_| gone("the full text of this message is not UTF-8".into()))
}

/// The typed refusal for a body that aged out: `blob-expired`, which clients already read as
/// "removed after its retention window; the message text remains".
pub fn expired_error(origin: &str) -> ApiError {
    blob_error(St3Error::new(
        "blob-expired",
        format!(
            "the full text of this message expired and was removed from {origin} after its retention; only the preview remains"
        ),
    ))
}

fn body_value(message: &MessageView, text: String, complete: bool) -> Value {
    json!({ "message": message.subject, "bytes": text.len(), "text": text, "complete": complete })
}

/// `GET /v1/client/message-bodies/{id}`: the whole text of a message a session may read, asked
/// of its owner. An owner that cannot be reached answers `remote-unavailable`.
pub async fn client_body(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    let subject = message_subject(&id);
    let lookup = state.clone();
    let message = blocking_store(move || lookup.store.message(&subject))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("message `{id}` does not exist")))?;
    let actor = session.authority_actor.as_str();
    if actor.starts_with("person/")
        && actor.matches('/').count() == 1
        && !person_reads_message(&state.store, actor, &message)
    {
        return Err(ApiError {
            status: StatusCode::FORBIDDEN,
            code: "forbidden".into(),
            message: "this message is not in a conversation the session may read".into(),
            details: Box::default(),
        });
    }
    let text = whole_text(&state, &session, &message).await?;
    Ok(Json(body_value(&message, text, true)))
}

/// `GET /v1/messages/body/{subject}`: the same text for the daemon's own readers, which act for
/// the message's recipient. When the owner cannot give the text it answers the preview and says
/// where the rest is, so a seat is still handed the message.
pub async fn body(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let subject = message_subject(&subject);
    let lookup = state.clone();
    let message = blocking_store(move || lookup.store.message(&subject))
        .await?
        .ok_or_else(|| ApiError::not_found("this message does not exist"))?;
    let actor = [&message.to, &message.from]
        .into_iter()
        .find(|party| party.starts_with("agent/") || party.starts_with("person/"))
        .cloned()
        .ok_or_else(|| ApiError::not_found("this message has no reader to act for"))?;
    let session = ClientSession::local(Some(&actor))?;
    match whole_text(&state, &session, &message).await {
        Ok(text) => Ok(Json(body_value(&message, text, true))),
        Err(error) if matches!(error.code.as_str(), "remote-unavailable" | "blob-not-found" | "blob-expired") => {
            let origin = message.body_origin.clone().unwrap_or_default();
            let kib = message.body_bytes.unwrap_or_default().div_ceil(1024);
            let expired = error.code == "blob-expired";
            let note = if expired {
                format!(
                    "\n\n[The full text of this message ({kib} KiB) expired and was removed from {origin} after its retention; only this preview remains.]"
                )
            } else {
                format!(
                    "\n\n[The full text of this message ({kib} KiB) is on {origin}, which did not give it now. Read it there or try again.]"
                )
            };
            let mut value = body_value(&message, format!("{}{note}", message.content), false);
            if expired {
                value["expired"] = json!(true);
            }
            Ok(Json(value))
        }
        Err(error) => Err(error),
    }
}
