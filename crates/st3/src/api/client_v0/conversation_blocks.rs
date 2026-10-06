//! Owner-local wire normalization. References contain no path or payload and hold no durable
//! state: a fetch rebinds the exact native source and refuses a changed revision.
use super::*;
use crate::external_sessions::{ExternalConversation, ExternalSession};
use base64::engine::general_purpose::STANDARD;
use std::io::Read as _;

const VALUE_BYTES: usize = 8 * 1024;
const CHUNK_BYTES: usize = 256 * 1024;
const IMAGE_BYTES: usize = 32 * 1024 * 1024;

fn invalidated() -> ApiError {
    ApiError { status: StatusCode::GONE, code: "conversation-content-invalidated".into(),
        message: "the owner content reference expired or its session/source revision changed; reload the conversation".into(),
        details: Box::new(serde_json::Map::from_iter([("full_resync".into(), json!(true))])) }
}

pub(super) fn source(state: &AppState, session_id: &str) -> Result<ExternalSession, ApiError> {
    let snapshot = new_client_snapshot(state);
    if let Some((owner, incarnation, origin)) =
        super::super::managed_session_owner_at(&state.store, snapshot.store_index, session_id)
            .map_err(ApiError::internal)?
    {
        if origin
            .as_deref()
            .is_some_and(|origin| origin != state.store.origin())
        {
            return Err(availability(super::super::remote_unavailable(
                &client_host_id(origin.as_deref().unwrap()),
            )));
        }
        if let Some(incarnation) = incarnation
            && let Some(managed) = managed_transcript(state, &owner, &incarnation)?
        {
            return managed
                .transcript
                .map_err(|missing| transcript_unavailable(&missing.reason));
        }
        return Err(transcript_unavailable(
            "the managed native session has not been bound",
        ));
    }
    match crate::external_sessions::find_conversation(
        state.native_session_home.as_deref(),
        session_id,
    )
    .map_err(ApiError::internal)?
    {
        Some(ExternalConversation::Readable(source)) => Ok(source),
        Some(ExternalConversation::Unavailable(_)) => Err(transcript_unavailable(
            "the native session could not be identified",
        )),
        None => Err(ApiError::not_found("the session does not exist")),
    }
}

/// Preserve transport/authorization codes while making conversation availability explicit.
pub(in crate::api) fn availability(mut error: ApiError) -> ApiError {
    if error.code == "remote-unavailable" {
        error
            .details
            .insert("availability".into(), json!("owner-unavailable"));
        error.message = format!(
            "owner unavailable: {}",
            error.message.replace("; cached data remains usable", "")
        );
    }
    error
}

fn transcript_unavailable(reason: &str) -> ApiError {
    ApiError {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        code: "transcript-unavailable".into(),
        message: format!("transcript unavailable: {reason}"),
        details: Box::default(),
    }
}

fn basis(source: &ExternalSession) -> Result<String, ApiError> {
    let metadata = std::fs::metadata(&source.transcript)
        .map_err(|_| transcript_unavailable("the owner cannot inspect its transcript"))?;
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt as _;
        format!("{}:{}", metadata.dev(), metadata.ino())
    };
    #[cfg(not(unix))]
    let identity = format!("{:?}", metadata.created().ok());
    // SQLite may change in its WAL without touching the database file.
    let wal = source.transcript.with_file_name(format!(
        "{}-wal",
        source
            .transcript
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    ));
    let wal_revision = std::fs::metadata(wal).ok().map(|metadata| {
        (
            metadata.len(),
            metadata.modified().ok().map(|time| format!("{time:?}")),
        )
    });
    Ok(hex::encode(Sha256::digest(
        serde_json::to_vec(&json!([
            source.driver.as_str(),
            source.native_id,
            source.revision,
            source.transcript,
            identity,
            wal_revision,
            metadata.len(),
            metadata.modified().ok().map(|time| format!("{time:?}"))
        ]))
        .map_err(ApiError::internal)?,
    )))
}

fn reference(basis: &str, session: &str, item: &Value, pointer: &str) -> String {
    let identity = json!([basis, session, item["id"], item["revision"], pointer]);
    hex::encode(Sha256::digest(
        serde_json::to_vec(&identity).expect("JSON value encodes"),
    ))
}

fn clipped(text: &str) -> String {
    if text.len() <= VALUE_BYTES {
        return text.to_owned();
    }
    let mut end = VALUE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[st truncated this native timeline value: size limit; {} bytes]",
        &text[..end],
        text.len()
    )
}

fn bound(value: &mut Value) {
    let encoded = serde_json::to_string(value).expect("JSON value encodes");
    if encoded.len() > VALUE_BYTES {
        *value = json!(clipped(&encoded));
    }
}

fn image(value: &Value) -> bool {
    (value.get("type").and_then(Value::as_str) == Some("file")
        && value
            .get("mime")
            .and_then(Value::as_str)
            .is_some_and(|mime| mime.starts_with("image/")))
        || matches!(
            value.get("type").and_then(Value::as_str),
            Some("image" | "input_image" | "output_image" | "image_url")
        )
}

fn image_media(value: &Value) -> &str {
    value
        .get("mimeType")
        .or_else(|| value.get("mime_type"))
        .or_else(|| value.get("mime"))
        .or_else(|| value.pointer("/source/media_type"))
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream")
}

fn continuation(reference: String, media: &str, size: Option<usize>, reason: &str) -> Value {
    let mut result = json!({"ref":reference,"media_type":media,"reason":reason});
    if let Some(size) = size {
        result["size"] = json!(size);
    }
    result
}

fn image_refs(value: &mut Value, pointer: &str, basis: &str, session: &str, item: &Value) {
    if image(value) {
        *value = json!({"type":"image","content":continuation(reference(basis,session,item,pointer),image_media(value),None,"on-demand")});
        return;
    }
    match value {
        Value::Array(values) => {
            for (index, value) in values.iter_mut().enumerate() {
                image_refs(value, &format!("{pointer}/{index}"), basis, session, item);
            }
        }
        Value::Object(values) => {
            for (key, value) in values.iter_mut() {
                image_refs(
                    value,
                    &format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1")),
                    basis,
                    session,
                    item,
                );
            }
        }
        _ => {}
    }
}

/// Called before pagination: its held vectors contain only bounded display values and refs.
pub(super) fn read(
    source: &ExternalSession,
    session: &ClientSession,
    session_id: &str,
) -> Result<Vec<Value>, ApiError> {
    let before = basis(source)?;
    let items =
        crate::external_sessions::normalized_timeline(source).map_err(ApiError::internal)?;
    if before != basis(source)? {
        return Err(invalidated());
    }
    let items = prepare(source, session, session_id, items)?;
    if before != basis(source)? {
        return Err(invalidated());
    }
    Ok(items)
}

/// Called before pagination: its held vectors contain only bounded display values and refs.
pub(super) fn prepare(
    source: &ExternalSession,
    session: &ClientSession,
    session_id: &str,
    mut items: Vec<Value>,
) -> Result<Vec<Value>, ApiError> {
    let basis = basis(source)?;
    for item in &mut items {
        let original = item.clone();
        let body = item["body"]
            .as_object_mut()
            .ok_or_else(|| ApiError::internal("native body is not an object"))?;
        if let Some(blocks) = body.get_mut("blocks").and_then(Value::as_array_mut) {
            for (index, block) in blocks.iter_mut().enumerate() {
                let pointer = format!("/body/blocks/{index}/payload");
                let encoded = serde_json::to_vec(&block["payload"]).map_err(ApiError::internal)?;
                if block["kind"] == "image" {
                    block["continuation"] = continuation(
                        reference(&basis, session_id, &original, &pointer),
                        image_media(&block["payload"]),
                        None,
                        "on-demand",
                    );
                    block["payload"] = json!({});
                } else {
                    image_refs(
                        &mut block["payload"],
                        &pointer,
                        &basis,
                        session_id,
                        &original,
                    );
                    if encoded.len() > VALUE_BYTES {
                        block["continuation"] = continuation(
                            reference(&basis, session_id, &original, &pointer),
                            "application/json",
                            Some(encoded.len()),
                            "size-limit",
                        );
                        bound(&mut block["payload"]);
                    }
                }
            }
        }
        if let Some(block) = body
            .get("blocks")
            .and_then(Value::as_array)
            .and_then(|blocks| blocks.first())
        {
            if block["kind"] == "unknown" {
                let label = body
                    .get("text")
                    .and_then(Value::as_str)
                    .and_then(|text| text.lines().next())
                    .unwrap_or("[unknown]");
                body["text"] = json!(format!("{label}\n{}", block["payload"]));
            }
        }
        // Fallback bodies use known v0 types and also move native pixels to owner fetch refs.
        for key in ["text", "arguments", "content"] {
            if let Some(value) = body.get_mut(key) {
                image_refs(
                    value,
                    &format!("/body/{key}"),
                    &basis,
                    session_id,
                    &original,
                );
                if let Value::String(text) = value {
                    *text = clipped(text);
                } else {
                    bound(value);
                }
            }
        }
        if !session.conversation_blocks {
            body.remove("blocks");
        }
    }
    if basis != self::basis(source)? {
        return Err(invalidated());
    }
    Ok(items)
}

/// Keep negotiated data out of legacy relayed clients without changing their known enum.
pub(in crate::api) fn legacy(value: &mut Value, session: &ClientSession) {
    if session.conversation_blocks {
        return;
    }
    if let Some(items) = value.get_mut("items").and_then(Value::as_array_mut) {
        for item in items {
            if let Some(body) = item["body"].as_object_mut() {
                body.remove("blocks");
            }
        }
    }
}

#[derive(Default, Deserialize)]
pub(in crate::api) struct ChunkQuery {
    offset: Option<u64>,
}

pub(in crate::api) async fn chunk(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    AxumPath((id, reference)): AxumPath<(String, String)>,
    Query(query): Query<ChunkQuery>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    let session_id = conversation_session_id(&state, &id)?;
    if let Some(owner) = conversation_owner_host(&state, &session, &session_id)? {
        let value = state
            .client_relay
            .as_ref()
            .ok_or_else(|| availability(super::super::remote_unavailable(&owner)))?
            .read(
                &owner,
                &crate::peer::ClientReadRequest {
                    authority_actor: session.authority_actor,
                    relay: None,
                    request: crate::peer::ClientReadOperation::ConversationContent {
                        session_id,
                        reference,
                        offset: query.offset.unwrap_or(0),
                    },
                },
            )
            .await
            .map_err(|error| availability(remote_read_error(&owner, error)))?;
        return Ok(Json(value));
    }
    chunk_local(
        &state,
        &session,
        &session_id,
        &reference,
        query.offset.unwrap_or(0),
    )
    .await
    .map(Json)
}

pub(super) async fn chunk_local(
    state: &AppState,
    session: &ClientSession,
    session_id: &str,
    wanted: &str,
    offset: u64,
) -> Result<Value, ApiError> {
    require_scope(session, "read.projections")?;
    if wanted.len() != 64 || !wanted.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalidated());
    }
    // Source discovery and bounded transcript scans run off the async reactor. No cache or
    // durable reference registry is introduced by this request.
    let request_state = state.clone();
    let request_session = session_id.to_owned();
    let request_reference = wanted.to_owned();
    let (source, before, value) = tokio::task::spawn_blocking(move || -> Result<_, ApiError> {
        let source = source(&request_state, &request_session)?;
        let before = basis(&source)?;
        let items =
            crate::external_sessions::normalized_timeline(&source).map_err(ApiError::internal)?;
        let mut found = None;
        for item in &items {
            find_content(
                &item["body"],
                "/body",
                item,
                &before,
                &request_session,
                &request_reference,
                &mut found,
            );
        }
        Ok((source, before, found.ok_or_else(invalidated)?))
    })
    .await
    .map_err(ApiError::internal)??;
    let (bytes, media) = if image(&value) {
        image_bytes(&source, &value).await?
    } else {
        (
            serde_json::to_vec(&value).map_err(ApiError::internal)?,
            "application/json".to_owned(),
        )
    };
    if before != basis(&source)? || basis(&self::source(state, session_id)?)? != before {
        return Err(invalidated());
    }
    let start =
        usize::try_from(offset).map_err(|_| validation("content offset is out of range"))?;
    if start > bytes.len() {
        return Err(validation("content offset is out of range"));
    }
    let end = start.saturating_add(CHUNK_BYTES).min(bytes.len());
    Ok(
        json!({"kind":"conversation-content-chunk","ref":wanted,"media_type":media,
        "offset":offset,"size":bytes.len(),"data":STANDARD.encode(&bytes[start..end]),
        "next_offset":(end<bytes.len()).then_some(end)}),
    )
}

fn find_content(
    value: &Value,
    pointer: &str,
    item: &Value,
    basis: &str,
    session: &str,
    wanted: &str,
    found: &mut Option<Value>,
) {
    if found.is_some() {
        return;
    }
    if reference(basis, session, item, pointer) == wanted {
        *found = Some(value.clone());
        return;
    }
    match value {
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                find_content(
                    value,
                    &format!("{pointer}/{index}"),
                    item,
                    basis,
                    session,
                    wanted,
                    found,
                );
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                find_content(
                    value,
                    &format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1")),
                    item,
                    basis,
                    session,
                    wanted,
                    found,
                );
            }
        }
        _ => {}
    }
}

async fn image_bytes(
    source: &ExternalSession,
    value: &Value,
) -> Result<(Vec<u8>, String), ApiError> {
    let encoded = value
        .get("data")
        .or_else(|| value.pointer("/source/data"))
        .or_else(|| value.get("image_url").filter(|value| value.is_string()))
        .or_else(|| value.pointer("/image_url/url"))
        .or_else(|| value.get("url"))
        .and_then(Value::as_str)
        .ok_or_else(|| transcript_unavailable("native image has no readable source"))?;
    let mut media = image_media(value).to_owned();
    let bytes = if let Some(uri) = encoded.strip_prefix("data:") {
        let (header, body) = uri
            .split_once(',')
            .ok_or_else(|| validation("native image data URI is malformed"))?;
        media = header
            .split(';')
            .next()
            .unwrap_or("application/octet-stream")
            .to_owned();
        if header.ends_with(";base64") {
            STANDARD
                .decode(body)
                .map_err(|_| validation("native image base64 is malformed"))?
        } else {
            urlencoding::decode_binary(body.as_bytes()).into_owned()
        }
    } else if let Some(digest) = encoded.strip_prefix("blob:sha256:") {
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(validation("native blob reference is malformed"));
        }
        // OMP/Pi native layout is <agent-home>/sessions/<project>/<session>.jsonl;
        // its provider-owned blob store is <agent-home>/blobs/<sha256>.
        let home = source
            .transcript
            .ancestors()
            .find(|path| path.file_name().is_some_and(|name| name == "sessions"))
            .and_then(|path| path.parent())
            .ok_or_else(|| transcript_unavailable("native blob store is not bound"))?;
        let file = std::fs::File::open(home.join("blobs").join(digest))
            .map_err(|_| transcript_unavailable("native image blob is missing"))?;
        let mut bytes = Vec::new();
        file.take((IMAGE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(ApiError::internal)?;
        bytes
    } else if let Some(path) = encoded.strip_prefix("file://") {
        let decoded =
            urlencoding::decode(path).map_err(|_| validation("native file URI is malformed"))?;
        let file = std::fs::File::open(decoded.as_ref())
            .map_err(|_| transcript_unavailable("native image file is missing"))?;
        let mut bytes = Vec::new();
        file.take((IMAGE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(ApiError::internal)?;
        bytes
    } else if encoded.starts_with("https://") || encoded.starts_with("http://") {
        let response = reqwest::Client::new()
            .get(encoded)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|_| transcript_unavailable("native image URL could not be read by its owner"))?
            .error_for_status()
            .map_err(|_| transcript_unavailable("native image URL returned an error"))?;
        if let Some(content_type) = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
        {
            media = content_type.to_owned();
        }
        if response
            .content_length()
            .is_some_and(|size| size > IMAGE_BYTES as u64)
        {
            return Err(validation("native image exceeds the 32 MiB read limit"));
        }
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(ApiError::internal)? {
            if bytes.len().saturating_add(chunk.len()) > IMAGE_BYTES {
                return Err(validation("native image exceeds the 32 MiB read limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        bytes
    } else {
        STANDARD
            .decode(encoded)
            .map_err(|_| validation("native image base64 is malformed"))?
    };
    if bytes.len() > IMAGE_BYTES {
        return Err(validation("native image exceeds the 32 MiB read limit"));
    }
    Ok((bytes, media))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_sessions::ExternalDriver;

    fn fixture(root: &std::path::Path, content: Value) -> ExternalSession {
        let path = root.join(".omp/agent/sessions/example/native-test.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let header = json!({"type":"session","id":"native-test","cwd":"/work/example","timestamp":"2026-10-06T12:00:00Z"});
        let message = json!({"type":"message","id":"message-test","timestamp":"2026-10-06T12:00:01Z","message":{"role":"assistant","content":content}});
        std::fs::write(&path, format!("{header}\n{message}\n")).unwrap();
        ExternalSession {
            id: format!(
                "session/external-{}",
                &hex::encode(Sha256::digest(b"omp:native-test"))[..24]
            ),
            revision: "revision-test".into(),
            driver: ExternalDriver::Omp,
            native_id: "native-test".into(),
            transcript: path,
            codex_home: None,
            cwd: None,
            title: None,
            started_at_unix_ms: 0,
            updated_at_unix_ms: 0,
            process: None,
        }
    }

    #[tokio::test]
    async fn full_unknown_arguments_reasoning_and_pixels_survive_owner_fetch_without_storage() {
        let root = tempfile::tempdir().unwrap();
        let raw =
            json!({"type":"future","nested":{"token":"invented-token","large":"é".repeat(10000)}});
        let pixels = vec![7u8; CHUNK_BYTES + 123];
        let image = json!({"type":"image","mimeType":"image/png","data":STANDARD.encode(&pixels)});
        let source = fixture(
            root.path(),
            json!([
                {"type":"thinking","thinking":"visible invented-token"},
                {"type":"toolCall","id":"call-test","name":"shell","arguments":{"command":"echo invented-token","nested":{"value":"full"}}},
                raw.clone(),image
            ]),
        );
        let mut session = ClientSession::local(Some("person/example")).unwrap();
        session.conversation_blocks = true;
        let state = super::super::tests::test_state_named(root.path(), "owner-test");
        let index = state.store.index().unwrap();
        let local = state
            .store
            .changes_since(i64::MAX as u64, i64::MAX)
            .unwrap()
            .local;
        let items = read(&source, &session, &source.id).unwrap();
        let blocks = items
            .iter()
            .filter_map(|item| item["body"]["blocks"].as_array())
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(
            blocks
                .iter()
                .map(|block| block["kind"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["reasoning", "tool_call", "unknown", "image"]
        );
        assert_eq!(blocks[1]["payload"]["arguments"]["nested"]["value"], "full");
        assert_eq!(blocks[2]["continuation"]["reason"], "size-limit");
        assert!(blocks[2]["payload"].as_str().unwrap().contains("truncated"));
        assert_eq!(blocks[3]["payload"], json!({}));
        assert!(
            !serde_json::to_string(&items)
                .unwrap()
                .contains(&STANDARD.encode(&pixels))
        );
        assert!(serde_json::to_vec(&items).unwrap().len() < CLIENT_MAX_RESPONSE_BYTES);
        let before = basis(&source).unwrap();
        let original = crate::external_sessions::normalized_timeline(&source).unwrap();
        let mut unknown = None;
        for item in &original {
            find_content(
                &item["body"],
                "/body",
                item,
                &before,
                &source.id,
                blocks[2]["continuation"]["ref"].as_str().unwrap(),
                &mut unknown,
            );
        }
        assert_eq!(unknown.unwrap(), json!({"raw":raw}));
        let (decoded, mime) = image_bytes(
            &source,
            &original.last().unwrap()["body"]["blocks"][0]["payload"],
        )
        .await
        .unwrap();
        assert_eq!(decoded, pixels);
        assert_eq!(mime, "image/png");
        assert_eq!(
            state.store.index().unwrap(),
            index,
            "normalization/fetch must not write claims"
        );
        assert_eq!(
            state
                .store
                .changes_since(i64::MAX as u64, i64::MAX)
                .unwrap()
                .local,
            local,
            "must not write local observations"
        );
        session.conversation_blocks = false;
        let legacy = read(&source, &session, &source.id).unwrap();
        assert!(
            legacy
                .iter()
                .all(|item| item["body"].get("blocks").is_none())
        );
        let decoded: Vec<st3_client::TimelineEntry> =
            serde_json::from_value(json!(legacy)).unwrap();
        assert_eq!(decoded.len(), items.len());
    }

    #[tokio::test]
    async fn http_owner_chunks_negotiate_bound_bytes_and_visibly_invalidate_without_writes() {
        use axum::body::{Body, to_bytes};
        use axum::http::Request;
        use tower::ServiceExt as _;
        let root = tempfile::tempdir().unwrap();
        let pixels = vec![11u8; CHUNK_BYTES + 101];
        let large = json!({"type":"future", "token":"invented-token", "large":"é".repeat(10000)});
        let native = fixture(
            root.path(),
            json!([
                {"type":"image", "mimeType":"image/png", "data":STANDARD.encode(&pixels)}, large.clone()
            ]),
        );
        let mut state = super::super::tests::test_state_named(root.path(), "http-owner-test");
        state.native_session_home = Some(root.path().to_path_buf());
        let index = state.store.index().unwrap();
        let local = state
            .store
            .changes_since(i64::MAX as u64, i64::MAX)
            .unwrap()
            .local;
        let app = super::super::super::router(state.clone());
        let request = |uri: String, blocks: bool| {
            let app = app.clone();
            async move {
                let mut request = Request::builder().uri(uri);
                if blocks {
                    request = request.header("x-st3-features", "conversation-blocks.v1");
                }
                let response = app
                    .oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                let status = response.status();
                let bytes = to_bytes(response.into_body(), CLIENT_MAX_RESPONSE_BYTES)
                    .await
                    .unwrap();
                (status, serde_json::from_slice::<Value>(&bytes).unwrap())
            }
        };
        let id = native.id.trim_start_matches("session/");
        let timeline = format!("/v1/client/sessions/{id}/timeline");
        let (status, legacy) = request(timeline.clone(), false).await;
        assert_eq!(status, StatusCode::OK, "{legacy}");
        assert!(
            legacy["value"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["body"].get("blocks").is_none())
        );
        let (status, modern) = request(timeline, true).await;
        assert_eq!(status, StatusCode::OK, "{modern}");
        let blocks = modern["value"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["body"]["blocks"].as_array())
            .flatten()
            .collect::<Vec<_>>();
        let image = blocks
            .iter()
            .find(|block| block["kind"] == "image")
            .unwrap();
        let reference = image["continuation"]["ref"].as_str().unwrap();
        let route = format!("/v1/client/conversations/{id}/content/{reference}/chunk");
        let (status, first) = request(route.clone(), true).await;
        assert_eq!(status, StatusCode::OK, "{first}");
        let mut result = STANDARD
            .decode(first["value"]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(result.len(), CHUNK_BYTES);
        assert_eq!(first["value"]["size"], pixels.len());
        let next = first["value"]["next_offset"].as_u64().unwrap();
        let (status, last) = request(format!("{route}?offset={next}"), true).await;
        assert_eq!(status, StatusCode::OK, "{last}");
        result.extend(
            STANDARD
                .decode(last["value"]["data"].as_str().unwrap())
                .unwrap(),
        );
        assert_eq!(result, pixels);
        assert!(last["value"]["next_offset"].is_null());
        let unknown = blocks
            .iter()
            .find(|block| block["kind"] == "unknown")
            .unwrap();
        let reference = unknown["continuation"]["ref"].as_str().unwrap();
        let (status, full) = request(
            format!("/v1/client/conversations/{id}/content/{reference}/chunk"),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{full}");
        let full: Value = serde_json::from_slice(
            &STANDARD
                .decode(full["value"]["data"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(full, json!({"raw":large}));
        // A native replacement invalidates already issued references visibly, including the
        // second chunk of a read. Nothing is fetched from a prepared or durable fallback.
        fixture(
            root.path(),
            json!([{ "type":"text", "text":"replacement" }]),
        );
        let (status, expired) = request(route, true).await;
        assert_eq!(status, StatusCode::GONE, "{expired}");
        assert_eq!(expired["code"], "conversation-content-invalidated");
        assert_eq!(expired["details"]["full_resync"], true);
        assert_eq!(state.store.index().unwrap(), index);
        assert_eq!(
            state
                .store
                .changes_since(i64::MAX as u64, i64::MAX)
                .unwrap()
                .local,
            local
        );
    }

    #[test]
    fn refs_fence_session_entry_revision_and_replacement() {
        let root = tempfile::tempdir().unwrap();
        let source = fixture(root.path(), json!([{"type":"future","text":"one"}]));
        let original = crate::external_sessions::normalized_timeline(&source).unwrap();
        let item = original.last().unwrap();
        let before = basis(&source).unwrap();
        let token = reference(&before, &source.id, item, "/body/blocks/0/payload");
        assert_ne!(
            token,
            reference(&before, "session/other", item, "/body/blocks/0/payload")
        );
        let mut revised = item.clone();
        revised["revision"] = json!(2);
        assert_ne!(
            token,
            reference(&before, &source.id, &revised, "/body/blocks/0/payload")
        );
        std::fs::write(&source.transcript, "replacement\n").unwrap();
        assert_ne!(before, basis(&source).unwrap());
        assert_ne!(
            token,
            reference(
                &basis(&source).unwrap(),
                &source.id,
                item,
                "/body/blocks/0/payload"
            )
        );
        assert_eq!(invalidated().code, "conversation-content-invalidated");
    }

    #[test]
    fn forwarded_missing_transcript_and_invalidated_refs_keep_their_meaning() {
        let unavailable = availability(super::super::super::remote_unavailable("host/owner"));
        assert_eq!(unavailable.details["availability"], "owner-unavailable");
        assert!(unavailable.message.starts_with("owner unavailable:"));
        assert!(!unavailable.message.contains("cached data remains usable"));
        for (code, status) in [
            ("transcript-unavailable", 422),
            ("conversation-content-invalidated", 410),
        ] {
            let rejected = crate::peer::ClientReadRejected {
                status,
                code: code.into(),
                message: "owner-native content changed".into(),
                details: serde_json::Map::from_iter([("full_resync".into(), json!(true))]),
            };
            let error = availability(remote_read_error("host/owner", rejected.into()));
            assert_eq!(error.code, code);
            assert_eq!(error.status.as_u16(), status);
            assert_ne!(
                error.details.get("availability"),
                Some(&json!("owner-unavailable"))
            );
        }
    }

    #[tokio::test]
    async fn chunk_checks_read_scope_before_resolving_any_source() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::test_state_named(root.path(), "owner-test");
        let denied = ClientSession::for_tests("person/example", "person/example", "unix");
        assert_eq!(
            chunk_local(&state, &denied, "session/unknown", &"0".repeat(64), 0)
                .await
                .unwrap_err()
                .code,
            "forbidden"
        );
        let allowed = ClientSession::local(Some("person/example")).unwrap();
        assert_eq!(
            chunk_local(&state, &allowed, "session/unknown", "arbitrary/path", 0)
                .await
                .unwrap_err()
                .code,
            "conversation-content-invalidated"
        );
    }
}
