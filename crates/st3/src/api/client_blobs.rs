//! Attachment files in the client API: upload, read, and the one-hop reads that carry them
//! between members. See [`crate::blobs`] for where the bytes live and why they never replicate.

use super::client_v0::{ClientSession, require_scope};
use super::*;
use crate::blobs::{self, BlobDir, MAX_BLOB_BYTES, UPLOAD_QUOTA_BYTES};
use crate::model::{AttachmentInput, MessageAttachment};
use std::path::PathBuf;
use crate::peer::{ClientReadOperation, ClientReadRejected, ClientReadRequest};
use axum::body::Bytes;
use axum::http::HeaderMap;

/// An upload is checked against its own limit so a too-large one answers in the API's own error
/// vocabulary; the route's body limit is just above it.
pub(super) const UPLOAD_BODY_LIMIT: usize = MAX_BLOB_BYTES + 4096;

fn retention() -> Duration {
    std::env::var("ST3_BLOB_RETENTION_HOURS")
        .ok()
        .and_then(|hours| hours.parse::<u64>().ok())
        .filter(|hours| *hours > 0)
        .map_or(blobs::DEFAULT_RETENTION, |hours| {
            Duration::from_secs(hours.saturating_mul(3600))
        })
}

/// Delete files past the retention window, at most every ten minutes for one state directory.
fn sweep_occasionally(state: &AppState) {
    static LAST: OnceLock<Mutex<BTreeMap<PathBuf, Instant>>> = OnceLock::new();
    let mut last = LAST
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if last
        .get(&state.state_dir)
        .is_some_and(|at| at.elapsed() < Duration::from_secs(600))
    {
        return;
    }
    last.insert(state.state_dir.clone(), Instant::now());
    drop(last);
    let window = retention();
    BlobDir::under(&state.state_dir).sweep(window);
    let _ = state
        .store
        .expire_blob_uploads(u64::try_from(window.as_millis()).unwrap_or(u64::MAX));
}

pub(super) fn blob_error(error: St3Error) -> ApiError {
    let status = match error.code {
        "blob-too-large" => StatusCode::PAYLOAD_TOO_LARGE,
        "unsupported-media-type" => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "blob-quota-exceeded" => StatusCode::TOO_MANY_REQUESTS,
        "blob-not-found" => StatusCode::NOT_FOUND,
        "blob-expired" => StatusCode::GONE,
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    };
    ApiError {
        status,
        code: error.code.into(),
        message: error.message,
        details: Box::default(),
    }
}

fn blob_reply(value: Value) -> Json<Value> {
    Json(value)
}

/// `POST /v1/client/blobs`: keep the bytes of one image on this member and answer their reference.
pub(super) async fn upload(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "control.messages")?;
    let actor = normalize_message_party(&session.authority_actor);
    let media_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let media_type = blobs::validate_upload(&media_type, &body).map_err(blob_error)?;
    let hash = hex::encode(Sha256::digest(&body));
    state
        .store
        .record_blob_upload(
            &actor,
            &hash,
            media_type,
            body.len() as u64,
            UPLOAD_QUOTA_BYTES,
            u64::try_from(retention().as_millis()).unwrap_or(u64::MAX),
        )
        .map_err(blob_error)?;
    BlobDir::under(&state.state_dir)
        .put(&body)
        .map_err(ApiError::internal)?;
    sweep_occasionally(&state);
    Ok(blob_reply(json!({
        "blob": format!("blob/{hash}"),
        "sha256": hash,
        "size": body.len(),
        "media_type": media_type,
    })))
}

/// Whether a person may read a message's attachments: the people in the conversation, and
/// anyone, under free mode, where an agent is a party.
fn person_reads_message(store: &Store, person: &str, message: &MessageView) -> bool {
    let mut current = Some(message.clone());
    for _ in 0..16 {
        let Some(view) = current else {
            return false;
        };
        if view.from == person
            || view.to == person
            || view.from.starts_with("agent/")
            || view.to.starts_with("agent/")
        {
            return true;
        }
        current = view
            .in_reply_to
            .as_deref()
            .and_then(|parent| store.message(&message_subject(parent)).ok().flatten());
    }
    false
}

/// What the request may read, and the attachment record its message gives for it.
fn authorize(
    state: &AppState,
    session: &ClientSession,
    hash: &str,
    message: Option<&str>,
) -> Result<Option<MessageAttachment>, ApiError> {
    let carrier = message
        .map(|subject| state.store.message(&message_subject(subject)))
        .transpose()
        .map_err(ApiError::internal)?
        .flatten()
        .and_then(|view| {
            view.attachments
                .iter()
                .find(|attachment| attachment.sha256 == hash)
                .cloned()
                .map(|attachment| (view, attachment))
        });
    let actor = session.authority_actor.as_str();
    if actor.starts_with("person/") && actor.matches('/').count() == 1 {
        let uploaded = state
            .store
            .blob_upload_media_type(hash, actor)
            .map_err(ApiError::internal)?
            .is_some();
        let reads = carrier
            .as_ref()
            .is_some_and(|(view, _)| person_reads_message(&state.store, actor, view));
        if !uploaded && !reads {
            return Err(ApiError {
                status: StatusCode::FORBIDDEN,
                code: "forbidden".into(),
                message: "this attachment is not in a conversation the session may read".into(),
                details: Box::default(),
            });
        }
    }
    Ok(carrier.map(|(_, attachment)| attachment))
}

fn blob_parts(id: &str) -> Result<String, ApiError> {
    blobs::parse_reference(id).map_err(|error| ApiError {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        code: "validation-failed".into(),
        message: error.message,
        details: Box::default(),
    })
}

fn read_error(host: &str, error: anyhow::Error) -> ApiError {
    match error.downcast_ref::<ClientReadRejected>() {
        Some(rejected) if rejected.code != "remote-unavailable" => ApiError {
            status: StatusCode::from_u16(rejected.status).unwrap_or(StatusCode::BAD_GATEWAY),
            code: rejected.code.clone(),
            message: rejected.message.clone(),
            details: Box::default(),
        },
        _ => remote_unavailable(host),
    }
}

/// Make the bytes local: fetch them from the member that took the upload, over the same peer
/// route a relayed client read takes, and keep a copy. The hash is checked before keeping.
async fn ensure_local(
    state: &AppState,
    session: &ClientSession,
    hash: &str,
    message: Option<&str>,
    attachment: Option<&MessageAttachment>,
    local_only: bool,
) -> Result<(), ApiError> {
    let blobs = BlobDir::under(&state.state_dir);
    if blobs.size(hash).is_some() {
        return Ok(());
    }
    let missing = |code: &'static str, text: &str| blob_error(St3Error::new(code, text));
    let Some(attachment) = attachment else {
        return Err(missing("blob-not-found", "this member holds no such attachment"));
    };
    let here = client_host_id(&state.node);
    if local_only || attachment.origin == here {
        return Err(missing(
            "blob-expired",
            "the attachment was removed after its retention window; the message text remains",
        ));
    }
    let actor = session.authority_actor.as_str();
    if !(actor.starts_with("person/") || actor.starts_with("agent/")) {
        return Err(ApiError {
            status: StatusCode::FORBIDDEN,
            code: "forbidden".into(),
            message: "name the person or agent reading this attachment".into(),
            details: Box::default(),
        });
    }
    let relay = state
        .client_relay
        .as_ref()
        .ok_or_else(|| remote_unavailable(&attachment.origin))?;
    let message = message.map(message_subject).unwrap_or_default();
    let mut bytes = Vec::new();
    loop {
        let value = relay
            .read(
                &attachment.origin,
                &ClientReadRequest {
                    authority_actor: actor.to_owned(),
                    request: ClientReadOperation::Blob {
                        sha256: hash.to_owned(),
                        message: message.clone(),
                        offset: bytes.len() as u64,
                    },
                    relay: None,
                },
            )
            .await
            .map_err(|error| read_error(&attachment.origin, error))?;
        let size = value["size"].as_u64().unwrap_or(u64::MAX);
        let chunk = value["data"]
            .as_str()
            .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
            .ok_or_else(|| remote_unavailable(&attachment.origin))?;
        if size > MAX_BLOB_BYTES as u64
            || chunk.is_empty() && (bytes.len() as u64) < size
            || bytes.len() + chunk.len() > MAX_BLOB_BYTES
        {
            return Err(remote_unavailable(&attachment.origin));
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 >= size {
            break;
        }
    }
    if hex::encode(Sha256::digest(&bytes)) != hash {
        return Err(missing(
            "blob-content-mismatch",
            "the bytes the owner sent do not match the attachment's hash",
        ));
    }
    blobs.put(&bytes).map_err(ApiError::internal)?;
    sweep_occasionally(state);
    Ok(())
}

#[derive(Deserialize)]
pub(super) struct BlobQuery {
    message: Option<String>,
    offset: Option<u64>,
    /// Answer from this member's files only. A member asked on behalf of another sets it, so a
    /// read for a file this member does not hold ends here instead of travelling on.
    local: Option<bool>,
}

/// `GET /v1/client/blobs/{sha256}?message=message/ID`: the bytes themselves.
pub(super) async fn get(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<BlobQuery>,
) -> Result<Response, ApiError> {
    require_scope(&session, "read.projections")?;
    let hash = blob_parts(&id)?;
    let attachment = authorize(&state, &session, &hash, query.message.as_deref())?;
    ensure_local(
        &state,
        &session,
        &hash,
        query.message.as_deref(),
        attachment.as_ref(),
        query.local.unwrap_or(false),
    )
    .await?;
    let bytes = BlobDir::under(&state.state_dir)
        .read(&hash)
        .map_err(ApiError::internal)?
        .ok_or_else(|| {
            blob_error(St3Error::new(
                "blob-expired",
                "the attachment was removed after its retention window",
            ))
        })?;
    let media_type = blobs::sniff_media_type(&bytes).unwrap_or("application/octet-stream");
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, media_type),
            (
                axum::http::header::CACHE_CONTROL,
                "private, max-age=31536000, immutable",
            ),
            (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response())
}

/// `GET /v1/client/blobs/{sha256}/chunk?message=message/ID&offset=N`: up to 512 KiB of the
/// bytes from `offset`, base64 in JSON, with the total size. Clients bound by the negotiated
/// response size, and members carrying a file between them, read this way.
pub(super) async fn chunk(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<BlobQuery>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    let hash = blob_parts(&id)?;
    let attachment = authorize(&state, &session, &hash, query.message.as_deref())?;
    ensure_local(
        &state,
        &session,
        &hash,
        query.message.as_deref(),
        attachment.as_ref(),
        query.local.unwrap_or(false),
    )
    .await?;
    let offset = query.offset.unwrap_or(0);
    let (size, bytes) = BlobDir::under(&state.state_dir)
        .read_range(&hash, offset, blobs::CHUNK_BYTES)
        .map_err(ApiError::internal)?
        .ok_or_else(|| {
            blob_error(St3Error::new(
                "blob-expired",
                "the attachment was removed after its retention window",
            ))
        })?;
    Ok(Json(json!({
        "sha256": hash,
        "size": size,
        "offset": offset.min(size),
        "data": base64::engine::general_purpose::STANDARD.encode(bytes),
    })))
}

/// The attachments of a send, completed from this member's files: each must be an upload by
/// the sender that is still held, of the type the sender names, and the message may carry few.
pub(super) fn resolve_attachments(
    state: &AppState,
    sender: &str,
    inputs: &[AttachmentInput],
) -> Result<Vec<MessageAttachment>, ApiError> {
    if inputs.len() > blobs::MAX_ATTACHMENTS {
        return Err(blob_error(St3Error::new(
            "too-many-attachments",
            format!("a message carries at most {} attachments", blobs::MAX_ATTACHMENTS),
        )));
    }
    let directory = BlobDir::under(&state.state_dir);
    let mut resolved: Vec<MessageAttachment> = Vec::new();
    for input in inputs {
        let hash = blobs::parse_reference(&input.blob).map_err(blob_error)?;
        if resolved.iter().any(|attachment| attachment.sha256 == hash) {
            continue;
        }
        let uploaded = state
            .store
            .blob_upload_media_type(&hash, sender)
            .map_err(ApiError::internal)?;
        let held = directory
            .read_range(&hash, 0, 16)
            .map_err(ApiError::internal)?;
        let (Some(_), Some((size, head))) = (uploaded, held) else {
            return Err(blob_error(St3Error::new(
                "blob-not-found",
                format!("{sender} has no held upload `blob/{hash}`; upload it first"),
            )));
        };
        let media_type = input
            .media_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if blobs::sniff_media_type(&head) != Some(media_type.as_str()) {
            return Err(blob_error(St3Error::new(
                "blob-content-mismatch",
                format!("`blob/{hash}` is not {media_type}"),
            )));
        }
        resolved.push(MessageAttachment {
            sha256: hash,
            media_type,
            name: input.name.as_deref().and_then(attachment_name),
            size,
            origin: client_host_id(&state.node),
        });
    }
    Ok(resolved)
}

/// A name for display only: its last path component without control characters.
fn attachment_name(name: &str) -> Option<String> {
    let leaf = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = leaf
        .chars()
        .filter(|ch| !ch.is_control())
        .take(120)
        .collect();
    let clean = clean.trim().to_owned();
    (!clean.is_empty()).then_some(clean)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;

    /// The smallest bytes that sniff as a PNG, padded to `length`.
    fn png(length: usize, seed: u8) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.resize(length.max(bytes.len()), seed);
        bytes
    }

    async fn call(
        app: &Router,
        method: &str,
        path: &str,
        person: &str,
        content_type: Option<&str>,
        body: Vec<u8>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("x-st3-person", person);
        if let Some(content_type) = content_type {
            request = request.header("content-type", content_type);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, headers, bytes.to_vec())
    }

    fn json_of(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).unwrap_or_else(|_| json!({"raw": bytes.len()}))
    }

    #[tokio::test]
    async fn an_image_uploaded_by_one_person_reaches_a_message_and_only_its_readers() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::api::tests::state(root.path());
        let app = router_for_transport(state.clone(), ClientTransportBoundary::Unix);
        let image = png(700 * 1024, 7);
        let hash = hex::encode(Sha256::digest(&image));

        // The types and sizes an upload may have, in the API's own error vocabulary.
        let upload = |media: &'static str, bytes: Vec<u8>| {
            let app = app.clone();
            async move {
                let (status, _, body) =
                    call(&app, "POST", "/v1/client/blobs", "person/alex", Some(media), bytes)
                        .await;
                (status, json_of(&body))
            }
        };
        let (status, body) = upload("text/plain", image.clone()).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
        assert_eq!(body["code"], "unsupported-media-type");
        let (status, body) = upload("image/jpeg", image.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["code"], "blob-content-mismatch");
        let (status, body) = upload("image/png", png(MAX_BLOB_BYTES + 1, 1)).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
        assert_eq!(body["code"], "blob-too-large");
        let (status, body) = upload("image/png", image.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["value"]["blob"], format!("blob/{hash}"));
        assert_eq!(body["value"]["size"], image.len());
        assert_eq!(
            upload("image/png", image.clone()).await.1["value"]["blob"],
            format!("blob/{hash}"),
            "the same bytes answer the same reference"
        );
        assert_eq!(
            BlobDir::under(root.path()).read(&hash).unwrap().unwrap(),
            image
        );

        // A message names the upload; only its sender may, so another person's cannot.
        let send = |from: &str, to: &str, key: &str, blob: String| {
            accept_message(
                &state,
                MessageSendRequest {
                    idempotency_key: key.into(),
                    from: from.into(),
                    to: to.into(),
                    content: String::new(),
                    title: None,
                    in_reply_to: None,
                    tags: Vec::new(),
                    attachments: vec![AttachmentInput {
                        blob,
                        media_type: "image/png".into(),
                        name: Some("/Users/alex/Screen Shot.png".into()),
                    }],
                },
                None,
            )
        };
        let stolen = send("person/bea", "person/carol", "stolen", format!("blob/{hash}"));
        assert_eq!(stolen.unwrap_err().code, "blob-not-found");
        let missing = send("person/alex", "person/carol", "missing", format!("blob/{}", "0".repeat(64)));
        assert_eq!(missing.unwrap_err().code, "blob-not-found");
        let sent = send("person/alex", "person/carol", "with-image", format!("blob/{hash}"))
            .unwrap()
            .0;
        assert_eq!(sent.attachments.len(), 1);
        let attachment = &sent.attachments[0];
        assert_eq!(attachment.sha256, hash);
        assert_eq!(attachment.name.as_deref(), Some("Screen Shot.png"));
        assert_eq!(attachment.origin, "host/node");
        assert_eq!(attachment.size, image.len() as u64);

        // The claim holds the reference and never the bytes.
        let claim = state
            .store
            .claims_for(&sent.subject, Some("message.sent"))
            .unwrap()
            .remove(0);
        assert_eq!(claim.body["fields"]["attachments"][0]["sha256"], hash);
        assert!(!claim.body.to_string().contains("blob_hash"));
        assert!(state.store.get_blob(&hash).unwrap().is_none());
        assert_eq!(
            client_message_resources(&state.store, None, true, None).unwrap()[0]["attachments"][0]
                ["blob"],
            format!("blob/{hash}")
        );
        let too_many = AttachmentInput {
            blob: format!("blob/{hash}"),
            media_type: "image/png".into(),
            name: None,
        };
        let refused = accept_message(
            &state,
            MessageSendRequest {
                idempotency_key: "many".into(),
                from: "person/alex".into(),
                to: "person/carol".into(),
                content: "x".into(),
                title: None,
                in_reply_to: None,
                tags: Vec::new(),
                attachments: (0..5)
                    .map(|index| AttachmentInput {
                        blob: format!("blob/{}", hex::encode(Sha256::digest([index]))),
                        ..too_many.clone()
                    })
                    .collect(),
            },
            None,
        );
        assert_eq!(refused.unwrap_err().code, "too-many-attachments");

        // The sender and the recipient read it through the message; a bystander does not.
        let path = format!("/v1/client/blobs/{hash}?message={}", sent.subject);
        let (status, headers, bytes) = call(&app, "GET", &path, "person/carol", None, vec![]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["content-type"], "image/png");
        assert_eq!(bytes, image);
        let (status, _, bytes) = call(&app, "GET", &path, "person/alex", None, vec![]).await;
        assert_eq!((status, bytes), (StatusCode::OK, image.clone()));
        let (status, _, body) = call(&app, "GET", &path, "person/dana", None, vec![]).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{}", json_of(&body));
        let bare = format!("/v1/client/blobs/{hash}");
        let (status, _, _) = call(&app, "GET", &bare, "person/dana", None, vec![]).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "a bystander cannot read it unnamed");
        let (status, _, bytes) = call(&app, "GET", &bare, "person/alex", None, vec![]).await;
        assert_eq!(
            (status, bytes.len()),
            (StatusCode::OK, image.len()),
            "an uploader reads its own upload before any message names it"
        );

        // A seat reads what a message gives it.
        let (status, _, bytes) =
            call(&app, "GET", &path, "agent/node.worker", None, vec![]).await;
        assert_eq!((status, bytes), (StatusCode::OK, image.clone()));

        // Chunks cover the file and then stop.
        let mut collected = Vec::new();
        loop {
            let chunk_path = format!("{bare}/chunk?message={}&offset={}", sent.subject, collected.len());
            let (status, _, body) = call(&app, "GET", &chunk_path, "person/carol", None, vec![]).await;
            assert_eq!(status, StatusCode::OK);
            let chunk = &json_of(&body)["value"];
            assert_eq!(chunk["size"], image.len());
            collected.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(chunk["data"].as_str().unwrap())
                    .unwrap(),
            );
            if collected.len() >= image.len() {
                break;
            }
        }
        assert_eq!(collected, image);

        // After the retention window the bytes are gone and the message is not.
        assert_eq!(BlobDir::under(root.path()).sweep(Duration::ZERO), 1);
        let (status, _, body) = call(&app, "GET", &path, "person/carol", None, vec![]).await;
        assert_eq!(status, StatusCode::GONE, "{}", json_of(&body));
        assert_eq!(json_of(&body)["code"], "blob-expired");
        assert_eq!(
            state.store.message(&sent.subject).unwrap().unwrap().attachments.len(),
            1
        );
    }

    #[tokio::test]
    async fn uploads_are_limited_per_person_and_expire() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::api::tests::state(root.path());
        state
            .store
            .record_blob_upload("person/alex", &"a".repeat(64), "image/png", 100, 150, 60_000)
            .unwrap();
        // The same bytes again cost nothing; other bytes past the limit are refused.
        state
            .store
            .record_blob_upload("person/alex", &"a".repeat(64), "image/png", 100, 150, 60_000)
            .unwrap();
        let refused = state
            .store
            .record_blob_upload("person/alex", &"b".repeat(64), "image/png", 100, 150, 60_000)
            .unwrap_err();
        assert_eq!(refused.code, "blob-quota-exceeded");
        state
            .store
            .record_blob_upload("person/bea", &"b".repeat(64), "image/png", 100, 150, 60_000)
            .unwrap();
        // Past the window the record goes, and with it the limit it counted against.
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(state.store.expire_blob_uploads(0).unwrap(), 2);
        state
            .store
            .record_blob_upload("person/alex", &"b".repeat(64), "image/png", 100, 150, 60_000)
            .unwrap();
    }
}
